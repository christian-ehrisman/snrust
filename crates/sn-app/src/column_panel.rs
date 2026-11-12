//! Docked column chooser for list views.
//!
//! It sits beside the table instead of over it, so the headers stay visible and update as
//! columns are toggled. Rows are single-line. Reference fields expand to the fields of the
//! table they point at, which adds dot-walked columns such as `caller_id.email`.

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::SnClient;
use sn_core::{FieldType, TableSchema};

use crate::state::AppServices;
use crate::theme::sp;

/// Dot-walking deeper than this is rarely useful and makes the tree hard to scan.
const MAX_DOT_WALK_DEPTH: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnPanelEvent {
    SetShown {
        key: String,
        shown: bool,
    },
    /// Move `key` to sit immediately before `before`, or to the end when `None`.
    Move {
        key: String,
        before: Option<String>,
    },
    Reset,
    Close,
}

#[derive(Clone)]
struct DraggedColumn {
    panel: EntityId,
    key: String,
    label: String,
}

struct DraggedColumnGhost {
    label: String,
}

impl Render for DraggedColumnGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.primary)
            .text_xs()
            .child(self.label.clone())
    }
}

/// One selectable field: a column of the list's table, or of a table reached by reference.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldNode {
    /// Full column key, dot-walked when nested (`caller_id.email`).
    key: String,
    label: String,
    reference_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Section {
        title: &'static str,
        count: usize,
    },
    Group {
        table: String,
        count: usize,
        collapsed: bool,
    },
    Shown {
        key: String,
        label: String,
    },
    Field {
        node: FieldNode,
        depth: usize,
        shown: bool,
        expanded: bool,
    },
    Note {
        text: String,
        depth: usize,
    },
}

impl Row {
    fn is_actionable(&self) -> bool {
        matches!(self, Row::Shown { .. } | Row::Field { .. })
    }
}

pub struct ColumnPanel {
    services: AppServices,
    client: SnClient,
    search: Entity<InputState>,
    schema: TableSchema,
    shown: Vec<String>,
    /// Schemas of referenced tables, loaded when a reference field is expanded.
    related: HashMap<String, TableSchema>,
    loading: HashSet<String>,
    failed: HashMap<String, String>,
    expanded: HashSet<String>,
    collapsed_groups: HashSet<String>,
    /// Keyboard cursor, as an index into the actionable rows.
    cursor: usize,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl EventEmitter<ColumnPanelEvent> for ColumnPanel {}

fn field_nodes(schema: &TableSchema, prefix: &str) -> Vec<FieldNode> {
    let mut nodes: Vec<FieldNode> = schema
        .fields
        .values()
        .map(|field| FieldNode {
            key: if prefix.is_empty() {
                field.element.clone()
            } else {
                format!("{prefix}.{}", field.element)
            },
            label: field.column_label.clone(),
            reference_target: match &field.field_type {
                FieldType::Reference { target_table } => Some(target_table.clone()),
                _ => None,
            },
        })
        .collect();
    nodes.sort_by_key(|node| node.label.to_lowercase());
    nodes
}

fn matches(words: &[String], node: &FieldNode) -> bool {
    let haystack = format!("{} {}", node.label, node.key).to_lowercase();
    words.iter().all(|word| haystack.contains(word))
}

impl ColumnPanel {
    pub fn new(
        services: AppServices,
        client: SnClient,
        schema: TableSchema,
        shown: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search fields by label or name…"));
        let subscription = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            InputEvent::Change => {
                this.cursor = 0;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.toggle_cursor_row(cx),
            _ => {}
        });
        let mut panel = Self {
            services,
            client,
            search,
            schema,
            shown,
            related: HashMap::new(),
            loading: HashSet::new(),
            failed: HashMap::new(),
            expanded: HashSet::new(),
            collapsed_groups: HashSet::new(),
            cursor: 0,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        };
        panel.load_cached_related_for_shown();
        panel
    }

    /// Focuses the search box. Row clicks call this too: text inputs give up focus on any
    /// click outside them, which would otherwise leave the panel deaf to typing and to its
    /// own shortcuts after the first click.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.search.focus_handle(cx).focus(window, cx);
    }

    pub fn set_shown(&mut self, shown: Vec<String>, cx: &mut Context<Self>) {
        if self.shown != shown {
            self.shown = shown;
            self.load_cached_related_for_shown();
            cx.notify();
        }
    }

    pub fn set_schema(&mut self, schema: TableSchema, cx: &mut Context<Self>) {
        self.schema = schema;
        cx.notify();
    }

    /// Dot-walked columns that are already shown need their target schemas for labels.
    fn load_cached_related_for_shown(&mut self) {
        let instance_id = self.client.profile().id.clone();
        for key in self.shown.clone() {
            let mut schema = self.schema.clone();
            let mut segments = key.split('.').peekable();
            while let Some(segment) = segments.next() {
                if segments.peek().is_none() {
                    break;
                }
                let Some(FieldType::Reference { target_table }) =
                    schema.fields.get(segment).map(|f| f.field_type.clone())
                else {
                    break;
                };
                let target = match self.related.get(&target_table) {
                    Some(loaded) => loaded.clone(),
                    None => match self
                        .services
                        .cache
                        .get_table_schema(&instance_id, &target_table)
                    {
                        Ok(Some(cached)) => {
                            self.related.insert(target_table, cached.data.clone());
                            cached.data
                        }
                        _ => break,
                    },
                };
                schema = target;
            }
        }
    }

    fn label_for(&self, key: &str) -> String {
        self.schema
            .dot_walk_label(key, |table| self.related.get(table).cloned())
            .unwrap_or_else(|| crate::list_view::prettify_column_key(key))
    }

    fn search_words(&self, cx: &App) -> Vec<String> {
        self.search
            .read(cx)
            .value()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let words = self.search_words(cx);
        let mut rows = Vec::new();

        rows.push(Row::Section {
            title: "Shown",
            count: self.shown.len(),
        });
        for key in &self.shown {
            let label = self.label_for(key);
            let node = FieldNode {
                key: key.clone(),
                label: label.clone(),
                reference_target: None,
            };
            if matches(&words, &node) {
                rows.push(Row::Shown {
                    key: key.clone(),
                    label,
                });
            }
        }

        rows.push(Row::Section {
            title: "All fields",
            count: self.schema.fields.len(),
        });
        // Most specific table first, then its parents, then anything defined elsewhere.
        let mut tables = self.schema.hierarchy.clone();
        for field in self.schema.fields.values() {
            if !tables.contains(&field.defined_on_table) {
                tables.push(field.defined_on_table.clone());
            }
        }
        let nodes = field_nodes(&self.schema, "");
        for table in tables {
            let group: Vec<&FieldNode> = nodes
                .iter()
                .filter(|node| {
                    self.schema
                        .fields
                        .get(&node.key)
                        .is_some_and(|field| field.defined_on_table == table)
                })
                .collect();
            if group.is_empty() {
                continue;
            }
            // A search looks inside every group.
            let collapsed = words.is_empty() && self.collapsed_groups.contains(&table);
            let mut group_rows = Vec::new();
            if !collapsed {
                for node in &group {
                    self.push_field(node, 0, &words, &mut group_rows);
                }
            }
            if collapsed || !group_rows.is_empty() {
                rows.push(Row::Group {
                    table,
                    count: group.len(),
                    collapsed,
                });
                rows.extend(group_rows);
            }
        }
        rows
    }

    /// Appends `node` and, when expanded, the fields of the table it references. Under a
    /// search, a parent is kept when it or any loaded descendant matches.
    fn push_field(&self, node: &FieldNode, depth: usize, words: &[String], out: &mut Vec<Row>) {
        let expanded = node.reference_target.is_some() && self.expanded.contains(&node.key);
        let mut children = Vec::new();
        if expanded && let Some(target) = &node.reference_target {
            if let Some(schema) = self.related.get(target) {
                for child in field_nodes(schema, &node.key) {
                    self.push_field(&child, depth + 1, words, &mut children);
                }
            } else if let Some(error) = self.failed.get(target) {
                children.push(Row::Note {
                    text: format!("Cannot load {target} fields: {error}"),
                    depth: depth + 1,
                });
            } else {
                children.push(Row::Note {
                    text: "Loading fields…".to_string(),
                    depth: depth + 1,
                });
            }
        }
        let child_matches = children.iter().any(|row| matches!(row, Row::Field { .. }));
        if !matches(words, node) && !(child_matches && !words.is_empty()) {
            return;
        }
        out.push(Row::Field {
            shown: self.shown.contains(&node.key),
            node: node.clone(),
            depth,
            expanded,
        });
        out.extend(children);
    }

    fn toggle_group(&mut self, table: &str, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(table) {
            self.collapsed_groups.insert(table.to_string());
        }
        cx.notify();
    }

    pub fn toggle_expanded(&mut self, key: &str, target: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
            self.ensure_related(target, cx);
        }
        cx.notify();
    }

    fn ensure_related(&mut self, table: &str, cx: &mut Context<Self>) {
        if self.related.contains_key(table) || self.loading.contains(table) {
            return;
        }
        self.failed.remove(table);
        self.loading.insert(table.to_string());
        let (tx, rx) = async_channel::bounded::<Result<TableSchema, String>>(1);
        let services = self.services.clone();
        let client = self.client.clone();
        let fetch_table = table.to_string();
        self.services.tokio_handle.spawn(async move {
            let result = services
                .ensure_table_schema(&client, &fetch_table, false)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(result).await;
        });
        let table = table.to_string();
        cx.spawn(async move |panel, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = panel.update(cx, |panel, cx| {
                    panel.loading.remove(&table);
                    match result {
                        Ok(schema) => {
                            panel.related.insert(table, schema);
                        }
                        Err(err) => {
                            panel.failed.insert(table, err);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn emit_toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        cx.emit(ColumnPanelEvent::SetShown {
            key: key.to_string(),
            shown: !self.shown.contains(&key.to_string()),
        });
    }

    fn actionable_rows(&self, cx: &App) -> Vec<Row> {
        self.rows(cx)
            .into_iter()
            .filter(Row::is_actionable)
            .collect()
    }

    fn toggle_cursor_row(&mut self, cx: &mut Context<Self>) {
        match self.actionable_rows(cx).get(self.cursor) {
            Some(Row::Shown { key, .. }) => self.emit_toggle(&key.clone(), cx),
            Some(Row::Field { node, .. }) => self.emit_toggle(&node.key.clone(), cx),
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let count = rows.iter().filter(|row| row.is_actionable()).count();
        if count == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).rem_euclid(count as isize) as usize;
        // Scroll positions are in terms of every rendered row, not only actionable ones.
        if let Some(row_ix) = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_actionable())
            .nth(self.cursor)
            .map(|(ix, _)| ix)
        {
            self.scroll.scroll_to_item(row_ix);
        }
        cx.notify();
    }

    /// ⌥↑ / ⌥↓ on a shown column moves it one place.
    fn nudge_cursor_row(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Row::Shown { key, .. }) = self.actionable_rows(cx).get(self.cursor).cloned()
        else {
            return;
        };
        let Some(position) = self.shown.iter().position(|shown| *shown == key) else {
            return;
        };
        let target = position as isize + delta;
        if target < 0 || target >= self.shown.len() as isize {
            return;
        }
        // Moving down means landing before the item after the neighbour.
        let before_ix = if delta > 0 { target + 1 } else { target } as usize;
        cx.emit(ColumnPanelEvent::Move {
            key,
            before: self.shown.get(before_ix).cloned(),
        });
        self.cursor = target as usize;
    }

    #[cfg(test)]
    pub fn visible_field_keys(&self, cx: &App) -> Vec<String> {
        self.rows(cx)
            .into_iter()
            .filter_map(|row| match row {
                Row::Field { node, .. } => Some(node.key),
                _ => None,
            })
            .collect()
    }

    #[cfg(test)]
    pub fn shown_labels(&self) -> Vec<String> {
        self.shown.iter().map(|key| self.label_for(key)).collect()
    }

    #[cfg(test)]
    pub fn is_loading_related(&self) -> bool {
        !self.loading.is_empty()
    }
}

/// The dense single-line field row shared with the form's Add Field dialog: label on the
/// left, technical name in muted monospace on the right, truncated at `name_max_width`
/// design pixels so a long name cannot crowd out the label.
pub fn field_row_text(label: &str, name: &str, name_max_width: f32, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .truncate()
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(sp(name_max_width))
                .text_size(sp(10.5))
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .truncate()
                .child(name.to_string()),
        )
}

fn check_box(checked: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .size(sp(14.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(if checked { theme.primary } else { theme.input })
        .bg(if checked {
            theme.primary
        } else {
            theme.background
        })
        .when(checked, |el| {
            el.child(
                Icon::new(IconName::Check)
                    .xsmall()
                    .text_color(theme.primary_foreground),
            )
        })
}

impl Render for ColumnPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = self.rows(cx);
        let actionable_count = rows.iter().filter(|row| row.is_actionable()).count();
        if self.cursor >= actionable_count {
            self.cursor = actionable_count.saturating_sub(1);
        }
        let cursor = self.cursor;
        let panel_id = cx.entity_id();
        let searching = !self.search_words(cx).is_empty();

        let mut actionable_ix = 0usize;
        let row_elements: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let is_cursor = row.is_actionable() && actionable_ix == cursor && searching;
                if row.is_actionable() {
                    actionable_ix += 1;
                }
                match row {
                    Row::Section { title, count } => h_flex()
                        .h(sp(24.0))
                        .px_2()
                        .mt_1()
                        .flex_shrink_0()
                        .justify_between()
                        .items_center()
                        .text_size(sp(10.0))
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(title.to_uppercase())
                        .child(count.to_string())
                        .into_any_element(),
                    Row::Group {
                        table,
                        count,
                        collapsed,
                    } => {
                        let toggle = table.clone();
                        h_flex()
                            .id(("col-group", ix))
                            .h(sp(22.0))
                            .px_2()
                            .gap_1()
                            .flex_shrink_0()
                            .items_center()
                            .cursor_pointer()
                            .rounded(px(4.0))
                            .hover(|style| style.bg(theme.accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_group(&toggle, cx);
                                this.focus(window, cx);
                            }))
                            .child(
                                Icon::new(if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall()
                                .text_color(theme.muted_foreground),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(sp(11.0))
                                    .font_medium()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(table),
                            )
                            .child(
                                div()
                                    .text_size(sp(10.5))
                                    .text_color(theme.muted_foreground)
                                    .child(count.to_string()),
                            )
                            .into_any_element()
                    }
                    Row::Shown { key, label } => {
                        let toggle_key = key.clone();
                        let drop_key = key.clone();
                        let selector = format!("col-shown:{key}");
                        let payload = DraggedColumn {
                            panel: panel_id,
                            key: key.clone(),
                            label: label.clone(),
                        };
                        h_flex()
                            .id(("col-shown", ix))
                            .debug_selector(move || selector)
                            .h(sp(24.0))
                            .px_1()
                            .gap_1p5()
                            .flex_shrink_0()
                            .items_center()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .when(is_cursor, |el| el.bg(theme.accent))
                            .hover(|style| style.bg(theme.accent))
                            .drag_over::<DraggedColumn>(move |style, dragged, _, cx| {
                                if dragged.panel == panel_id {
                                    style.border_t_2().border_color(cx.theme().primary)
                                } else {
                                    style
                                }
                            })
                            .on_drop(cx.listener(move |_, dragged: &DraggedColumn, _, cx| {
                                if dragged.panel == panel_id && dragged.key != drop_key {
                                    cx.emit(ColumnPanelEvent::Move {
                                        key: dragged.key.clone(),
                                        before: Some(drop_key.clone()),
                                    });
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.emit_toggle(&toggle_key, cx);
                                this.focus(window, cx);
                            }))
                            .child(
                                div()
                                    .id(("col-grip", ix))
                                    .cursor_grab()
                                    .on_drag(payload, |drag, _, _, cx| {
                                        cx.new(|_| DraggedColumnGhost {
                                            label: drag.label.clone(),
                                        })
                                    })
                                    .child(
                                        Icon::new(IconName::GripVertical)
                                            .xsmall()
                                            .text_color(theme.muted_foreground),
                                    ),
                            )
                            .child(check_box(true, cx))
                            .child(field_row_text(&label, &key, 120.0, cx))
                            .into_any_element()
                    }
                    Row::Field {
                        node,
                        depth,
                        shown,
                        expanded,
                    } => {
                        let toggle_key = node.key.clone();
                        let row_selector = format!("col-field:{}", node.key);
                        let expandable =
                            node.reference_target.is_some() && depth < MAX_DOT_WALK_DEPTH;
                        // The trailing name is the leaf element; the path is shown by nesting.
                        let leaf = node.key.rsplit('.').next().unwrap_or(&node.key).to_string();
                        h_flex()
                            .id(("col-field", ix))
                            .debug_selector(move || row_selector)
                            .h(sp(24.0))
                            .pr_1()
                            .pl(sp(4.0 + depth as f32 * 14.0))
                            .gap_1p5()
                            .flex_shrink_0()
                            .items_center()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .when(is_cursor, |el| el.bg(theme.accent))
                            .hover(|style| style.bg(theme.accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.emit_toggle(&toggle_key, cx);
                                this.focus(window, cx);
                            }))
                            .child(match (&node.reference_target, expandable) {
                                (Some(target), true) => {
                                    let expand_key = node.key.clone();
                                    let expand_target = target.clone();
                                    let expand_selector = format!("col-expand:{}", node.key);
                                    div()
                                        .id(("col-expand", ix))
                                        .debug_selector(move || expand_selector)
                                        .flex_shrink_0()
                                        .size(sp(14.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(3.0))
                                        .hover(|style| style.bg(theme.secondary_hover))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.toggle_expanded(&expand_key, &expand_target, cx);
                                            this.focus(window, cx);
                                        }))
                                        .child(
                                            Icon::new(if expanded {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            })
                                            .xsmall()
                                            .text_color(theme.muted_foreground),
                                        )
                                        .into_any_element()
                                }
                                _ => div().flex_shrink_0().size(sp(14.0)).into_any_element(),
                            })
                            .child(check_box(shown, cx))
                            .child(field_row_text(&node.label, &leaf, 120.0, cx))
                            .into_any_element()
                    }
                    Row::Note { text, depth } => div()
                        .h(sp(22.0))
                        .flex_shrink_0()
                        .pl(sp(26.0 + depth as f32 * 14.0))
                        .text_size(sp(11.0))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(text)
                        .into_any_element(),
                }
            })
            .collect();

        v_flex()
            .id("column-panel")
            .size_full()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let alt = event.keystroke.modifiers.alt;
                match event.keystroke.key.as_str() {
                    "down" if alt => this.nudge_cursor_row(1, cx),
                    "up" if alt => this.nudge_cursor_row(-1, cx),
                    "down" => this.move_cursor(1, cx),
                    "up" => this.move_cursor(-1, cx),
                    "escape" => cx.emit(ColumnPanelEvent::Close),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                h_flex()
                    .px_2p5()
                    .pt_2()
                    .pb_1p5()
                    .justify_between()
                    .items_center()
                    .child(div().text_sm().font_semibold().child("Columns"))
                    .child(
                        Button::new("close-column-panel")
                            .ghost()
                            .xsmall()
                            .icon(IconName::X)
                            .tooltip("Close (Esc)")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(ColumnPanelEvent::Close);
                            })),
                    ),
            )
            .child(
                div().px_2p5().pb_1().child(
                    Input::new(&self.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).xsmall())
                        .cleanable(true),
                ),
            )
            .child(
                v_flex()
                    .id("column-panel-rows")
                    .flex_1()
                    .min_h_0()
                    .px_1p5()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(row_elements),
            )
            .child(
                h_flex()
                    .px_2p5()
                    .py_1p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(sp(10.5))
                            .text_color(theme.muted_foreground)
                            .child("↑↓ ↵ toggle · ⌥↑↓ reorder"),
                    )
                    .child(
                        Button::new("reset-list-cols")
                            .ghost()
                            .xsmall()
                            .label("Use ServiceNow columns")
                            .tooltip("Discard this local column layout")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(ColumnPanelEvent::Reset);
                            })),
                    ),
            )
    }
}
