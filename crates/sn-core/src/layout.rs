//! Rearrangeable 2-column Form Auto-Grid layout model.
//!
//! Generates an intelligent out-of-the-box layout from ServiceNow `TableSchema` dictionary
//! metadata and supports interactive customization (drag/move field order, toggle 1-column vs
//! 2-column width span, hide/show fields, and add section dividers) persisted per `(instance, table)`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

use crate::schema::TableSchema;

/// Column width span inside the 2-column responsive form grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColSpan {
    /// Half-width card (1 of 2 columns).
    One,
    /// Full-width row spanning both columns.
    Two,
}

impl ColSpan {
    pub fn toggled(self) -> Self {
        match self {
            Self::One => Self::Two,
            Self::Two => Self::One,
        }
    }
}

/// Ordered item in the customizable Form Auto-Grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GridItem {
    Field {
        element: String,
        span: ColSpan,
    },
    SectionDivider {
        id: String,
        title: String,
        collapsed: bool,
    },
}

impl GridItem {
    pub fn field_name(&self) -> Option<&str> {
        match self {
            Self::Field { element, .. } => Some(element.as_str()),
            Self::SectionDivider { .. } => None,
        }
    }
}

/// Persisted form layout configuration for a `(instance_id, table)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormGridLayout {
    pub table: String,
    pub pinned_header_fields: Vec<String>,
    pub items: Vec<GridItem>,
    pub hidden_fields: Vec<String>,
    pub journal_fields: Vec<String>,
    pub is_customized: bool,
    pub updated_at: DateTime<Utc>,
}

/// Packed visual row produced by laying out [`FormGridLayout::items`] onto a 2-column grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridVisualRow {
    Divider {
        item_index: usize,
        id: String,
        title: String,
        collapsed: bool,
    },
    FullWidthField {
        item_index: usize,
        element: String,
    },
    TwoColumnPair {
        left: (usize, String),
        right: Option<(usize, String)>,
    },
}

impl FormGridLayout {
    /// Builds a smart default 2-column grid from `TableSchema` dictionary metadata.
    ///
    /// Classic high-signal fields (`number`, `caller_id`, `state`, `priority`, `assignment_group`,
    /// `assigned_to`, `short_description`, `description`) are placed first in intuitive order;
    /// journals (`work_notes`, `comments`) are routed to the Activity Stream; and noisy `sys_*`
    /// audit columns are kept out of the main grid (accessible via the System Metadata drawer
    /// or "+ Add Field" in Customize Grid mode).
    pub fn generate_smart_default(schema: &TableSchema) -> Self {
        let mut placed = HashSet::new();
        let mut items = Vec::new();
        let mut journal_fields = Vec::new();
        let mut hidden_fields = Vec::new();

        for (name, entry) in &schema.fields {
            if entry.is_journal() {
                journal_fields.push(name.clone());
                placed.insert(name.clone());
            }
        }
        // Ensure standard task journal fields maintain predictable order (`work_notes` + `comments`).
        journal_fields.sort_by_key(|f| match f.as_str() {
            "work_notes" => 0,
            "comments" => 1,
            _ => 2,
        });

        let priority_order = [
            "number",
            "name",
            "caller_id",
            "requested_for",
            "opened_by",
            "state",
            "incident_state",
            "priority",
            "impact",
            "urgency",
            "category",
            "subcategory",
            "business_service",
            "service_offering",
            "cmdb_ci",
            "assignment_group",
            "assigned_to",
            "active",
            "due_date",
            "short_description",
            "description",
            "close_code",
            "close_notes",
        ];

        for candidate in priority_order {
            if let Some(entry) = schema.fields.get(candidate) {
                if !entry.active || placed.contains(candidate) || entry.is_system_audit_field() {
                    continue;
                }
                let span = if entry
                    .field_type
                    .prefers_full_width(&entry.element, entry.max_length)
                {
                    ColSpan::Two
                } else {
                    ColSpan::One
                };
                items.push(GridItem::Field {
                    element: entry.element.clone(),
                    span,
                });
                placed.insert(entry.element.clone());
            }
        }

        // Add remaining active business fields (capping default initial grid at 28 fields so massive
        // 120-column task tables stay crisp, while keeping the rest a click away in `hidden_fields`).
        let mut remaining: Vec<_> = schema
            .fields
            .values()
            .filter(|e| e.active && !placed.contains(&e.element))
            .collect();
        remaining.sort_by(|a, b| {
            a.column_label
                .to_ascii_lowercase()
                .cmp(&b.column_label.to_ascii_lowercase())
        });

        for entry in remaining {
            if entry.is_system_audit_field() || items.len() >= 28 {
                hidden_fields.push(entry.element.clone());
            } else {
                let span = if entry
                    .field_type
                    .prefers_full_width(&entry.element, entry.max_length)
                {
                    ColSpan::Two
                } else {
                    ColSpan::One
                };
                items.push(GridItem::Field {
                    element: entry.element.clone(),
                    span,
                });
            }
            placed.insert(entry.element.clone());
        }

        let pinned_header_fields = ["state", "priority", "assigned_to", "active"]
            .into_iter()
            .filter(|f| schema.fields.contains_key(*f))
            .map(str::to_string)
            .collect();

        Self {
            table: schema.table.clone(),
            pinned_header_fields,
            items,
            hidden_fields,
            journal_fields,
            is_customized: false,
            updated_at: Utc::now(),
        }
    }

    /// Computes packed 2-column visual rows from `self.items`, honoring section collapse state.
    pub fn compute_visual_rows(&self) -> Vec<GridVisualRow> {
        let mut rows = Vec::new();
        let mut pending_left: Option<(usize, String)> = None;
        let mut section_collapsed = false;

        for (idx, item) in self.items.iter().enumerate() {
            match item {
                GridItem::SectionDivider {
                    id,
                    title,
                    collapsed,
                } => {
                    if let Some(left) = pending_left.take() {
                        rows.push(GridVisualRow::TwoColumnPair { left, right: None });
                    }
                    section_collapsed = *collapsed;
                    rows.push(GridVisualRow::Divider {
                        item_index: idx,
                        id: id.clone(),
                        title: title.clone(),
                        collapsed: *collapsed,
                    });
                }
                GridItem::Field { element, span } => {
                    if section_collapsed {
                        continue;
                    }
                    match span {
                        ColSpan::Two => {
                            if let Some(left) = pending_left.take() {
                                rows.push(GridVisualRow::TwoColumnPair { left, right: None });
                            }
                            rows.push(GridVisualRow::FullWidthField {
                                item_index: idx,
                                element: element.clone(),
                            });
                        }
                        ColSpan::One => {
                            if let Some(left) = pending_left.take() {
                                rows.push(GridVisualRow::TwoColumnPair {
                                    left,
                                    right: Some((idx, element.clone())),
                                });
                            } else {
                                pending_left = Some((idx, element.clone()));
                            }
                        }
                    }
                }
            }
        }

        if let Some(left) = pending_left.take() {
            rows.push(GridVisualRow::TwoColumnPair { left, right: None });
        }

        rows
    }

    pub fn index_of_field(&self, field: &str) -> Option<usize> {
        self.items
            .iter()
            .position(|item| item.field_name() == Some(field))
    }

    /// Reorders an item from `from_idx` to `to_idx` (used by drag-and-drop and reorder controls).
    pub fn move_item(&mut self, from_idx: usize, to_idx: usize) {
        if from_idx >= self.items.len() || to_idx >= self.items.len() || from_idx == to_idx {
            return;
        }
        let item = self.items.remove(from_idx);
        self.items.insert(to_idx, item);
        self.touch_customized();
    }

    /// Moves `source_field` directly before or after `target_field` (ideal for card drag-drop).
    pub fn move_field_relative(&mut self, source_field: &str, target_field: &str, after: bool) {
        if source_field == target_field {
            return;
        }
        let Some(src_idx) = self.index_of_field(source_field) else {
            return;
        };
        let item = self.items.remove(src_idx);
        if let Some(dst_idx) = self.index_of_field(target_field) {
            let insert_at = if after { dst_idx + 1 } else { dst_idx };
            self.items.insert(insert_at.min(self.items.len()), item);
        } else {
            self.items.push(item);
        }
        self.touch_customized();
    }

    /// Shifts a field earlier (`delta < 0`) or later (`delta > 0`) in the auto-grid.
    pub fn nudge_field(&mut self, field: &str, delta: isize) {
        let Some(idx) = self.index_of_field(field) else {
            return;
        };
        let target =
            (idx as isize + delta).clamp(0, self.items.len().saturating_sub(1) as isize) as usize;
        self.move_item(idx, target);
    }

    pub fn toggle_span(&mut self, field: &str) {
        for item in &mut self.items {
            if let GridItem::Field { element, span } = item
                && element == field
            {
                *span = span.toggled();
                self.touch_customized();
                return;
            }
        }
    }

    pub fn field_span(&self, field: &str) -> Option<ColSpan> {
        self.items.iter().find_map(|item| match item {
            GridItem::Field { element, span } if element == field => Some(*span),
            _ => None,
        })
    }

    pub fn hide_field(&mut self, field: &str) {
        if let Some(idx) = self.index_of_field(field) {
            self.items.remove(idx);
            if !self.hidden_fields.iter().any(|f| f == field) {
                self.hidden_fields.push(field.to_string());
            }
            self.touch_customized();
        }
    }

    pub fn show_field(&mut self, field: &str, span: ColSpan) {
        self.hidden_fields.retain(|f| f != field);
        if self.index_of_field(field).is_none() {
            self.items.push(GridItem::Field {
                element: field.to_string(),
                span,
            });
        }
        self.touch_customized();
    }

    pub fn add_section_divider(&mut self, title: impl Into<String>, at_index: Option<usize>) {
        let divider = GridItem::SectionDivider {
            id: Uuid::new_v4().to_string(),
            title: title.into(),
            collapsed: false,
        };
        match at_index {
            Some(idx) if idx <= self.items.len() => self.items.insert(idx, divider),
            _ => self.items.push(divider),
        }
        self.touch_customized();
    }

    pub fn toggle_section_collapsed(&mut self, divider_id: &str) {
        for item in &mut self.items {
            if let GridItem::SectionDivider { id, collapsed, .. } = item
                && id == divider_id
            {
                *collapsed = !*collapsed;
                self.updated_at = Utc::now();
                return;
            }
        }
    }

    pub fn remove_section_divider(&mut self, divider_id: &str) {
        let before = self.items.len();
        self.items.retain(
            |item| !matches!(item, GridItem::SectionDivider { id, .. } if id == divider_id),
        );
        if self.items.len() != before {
            self.touch_customized();
        }
    }

    /// Ensures newly discovered dictionary columns on the instance become available in
    /// `hidden_fields` (or `journal_fields`) without disturbing an existing customized grid.
    pub fn reconcile_with_schema(&mut self, schema: &TableSchema) {
        let mut known: HashSet<String> = self
            .items
            .iter()
            .filter_map(|i| i.field_name().map(str::to_string))
            .chain(self.hidden_fields.iter().cloned())
            .chain(self.journal_fields.iter().cloned())
            .collect();

        for (name, entry) in &schema.fields {
            if known.insert(name.clone()) {
                if entry.is_journal() {
                    self.journal_fields.push(name.clone());
                } else {
                    self.hidden_fields.push(name.clone());
                }
            }
        }
    }

    fn touch_customized(&mut self) {
        self.is_customized = true;
        self.updated_at = Utc::now();
    }
}
