//! Dirty-safe live Form Session state machine.
//!
//! Coordinates three concurrent concerns on an open ServiceNow form:
//! 1. **Live inbound server updates** (via AMB WebSocket or adaptive REST delta-polling)
//!    update clean fields immediately in place and emit [`LiveFieldPulse`] visual highlights.
//! 2. **Dirty local draft protection**: unsaved user edits are never overwritten by inbound
//!    server changes; instead, diverging server values surface as [`FieldConflict`]s with
//!    one-click `Keep Mine` / `Use Server` resolution.
//! 3. **Classic explicit Save (`Cmd+S`) / Update**: constructs a minimal `PATCH` payload
//!    containing only modified fields + pending journal entries (`work_notes` / `comments`).

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::instance::InstanceId;
use crate::model::{FieldValue, JournalEntry, SnRecord};

/// Conflict recorded when the server updates a field that the user has unsaved local edits in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldConflict {
    pub field: String,
    pub original_baseline: FieldValue,
    pub local_draft: FieldValue,
    pub server_latest: FieldValue,
    pub updated_by: Option<String>,
    pub detected_at: DateTime<Utc>,
}

/// Notification produced when a clean field updates live from the server, driving the
/// GPU highlight pulse animation on the Form View.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveFieldPulse {
    pub field: String,
    pub previous_display: String,
    pub new_display: String,
    pub updated_by: Option<String>,
    pub detected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyDeltaOutcome {
    pub updated_clean_fields: Vec<LiveFieldPulse>,
    pub new_conflicts: Vec<FieldConflict>,
    pub mod_count_advanced: bool,
}

/// Complete mutable state of an open Form tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormSessionState {
    pub instance_id: InstanceId,
    pub table: String,
    /// `None` when composing a brand-new record (`POST`), `Some(sys_id)` for existing records (`PATCH`).
    pub sys_id: Option<String>,
    pub baseline: SnRecord,
    pub drafts: IndexMap<String, FieldValue>,
    pub journal_drafts: IndexMap<String, String>,
    pub conflicts: IndexMap<String, FieldConflict>,
    pub journals: Vec<JournalEntry>,
    pub last_live_pulses: IndexMap<String, LiveFieldPulse>,
}

impl FormSessionState {
    pub fn from_existing_record(instance_id: InstanceId, record: SnRecord) -> Self {
        let sys_id = (!record.sys_id.is_empty()).then(|| record.sys_id.clone());
        let table = record.table.clone();
        Self {
            instance_id,
            table,
            sys_id,
            baseline: record,
            drafts: IndexMap::new(),
            journal_drafts: IndexMap::new(),
            conflicts: IndexMap::new(),
            journals: Vec::new(),
            last_live_pulses: IndexMap::new(),
        }
    }

    pub fn new_record(instance_id: InstanceId, table: impl Into<String>) -> Self {
        let table_str = table.into();
        Self {
            instance_id,
            table: table_str.clone(),
            sys_id: None,
            baseline: SnRecord::new_draft(table_str),
            drafts: IndexMap::new(),
            journal_drafts: IndexMap::new(),
            conflicts: IndexMap::new(),
            journals: Vec::new(),
            last_live_pulses: IndexMap::new(),
        }
    }

    pub fn is_new_record(&self) -> bool {
        self.sys_id.is_none()
    }

    /// Current value to render in a form control (`draft` if edited, otherwise `baseline`).
    pub fn effective_value(&self, field: &str) -> Option<&FieldValue> {
        self.drafts
            .get(field)
            .or_else(|| self.baseline.fields.get(field))
    }

    pub fn effective_raw(&self, field: &str) -> &str {
        self.effective_value(field)
            .map(|v| v.value.as_str())
            .unwrap_or("")
    }

    pub fn effective_display(&self, field: &str) -> &str {
        self.effective_value(field)
            .map(|v| v.effective_display())
            .unwrap_or("")
    }

    pub fn is_field_dirty(&self, field: &str) -> bool {
        self.drafts.contains_key(field)
    }

    pub fn is_dirty(&self) -> bool {
        !self.drafts.is_empty()
            || self
                .journal_drafts
                .values()
                .any(|text| !text.trim().is_empty())
    }

    pub fn dirty_field_count(&self) -> usize {
        self.drafts.len()
            + self
                .journal_drafts
                .values()
                .filter(|t| !t.trim().is_empty())
                .count()
    }

    /// Records a local edit. If the user edits the value back to match `baseline`, the field
    /// automatically returns to clean status.
    pub fn edit_field(&mut self, field: impl Into<String>, value: FieldValue) {
        let field = field.into();
        let baseline_raw = self
            .baseline
            .fields
            .get(&field)
            .map(|b| b.value.as_str())
            .unwrap_or("");

        if !self.is_new_record() && value.value == baseline_raw {
            self.drafts.shift_remove(&field);
            self.conflicts.shift_remove(&field);
        } else {
            if let Some(conflict) = self.conflicts.get_mut(&field) {
                conflict.local_draft = value.clone();
            }
            self.drafts.insert(field, value);
        }
    }

    pub fn edit_journal(&mut self, element: impl Into<String>, text: impl Into<String>) {
        let element = element.into();
        let text = text.into();
        if text.is_empty() {
            self.journal_drafts.shift_remove(&element);
        } else {
            self.journal_drafts.insert(element, text);
        }
    }

    pub fn journal_draft(&self, element: &str) -> &str {
        self.journal_drafts
            .get(element)
            .map(String::as_str)
            .unwrap_or("")
    }

    pub fn revert_field(&mut self, field: &str) {
        self.drafts.shift_remove(field);
        self.conflicts.shift_remove(field);
    }

    pub fn revert_all(&mut self) {
        self.drafts.clear();
        self.journal_drafts.clear();
        self.conflicts.clear();
    }

    /// Keeps the user's local draft and dismisses the conflict banner while updating baseline
    /// awareness to the latest server value.
    pub fn resolve_conflict_keep_local(&mut self, field: &str) {
        self.conflicts.shift_remove(field);
    }

    /// Accepts the incoming server value, discarding the conflicting local draft for `field`.
    pub fn resolve_conflict_accept_server(&mut self, field: &str) {
        if let Some(conflict) = self.conflicts.shift_remove(field) {
            self.baseline
                .fields
                .insert(field.to_string(), conflict.server_latest);
        }
        self.drafts.shift_remove(field);
    }

    /// Hydrates previously missing columns when a partial row-cache record is upgraded by a
    /// full background `GET`, without triggering false "live change" pulses on initial load.
    pub fn hydrate_initial_full_record(&mut self, full: SnRecord) {
        let was_partial = self.baseline.is_partial;
        if was_partial {
            self.baseline.merge_from(&full);
            self.baseline.is_partial = false;
        } else {
            let _ = self.apply_server_record(&full);
        }
    }

    /// Merges a live server record snapshot (from AMB or REST delta-poll) into the form session.
    pub fn apply_server_record(&mut self, incoming: &SnRecord) -> ApplyDeltaOutcome {
        let mut outcome = ApplyDeltaOutcome::default();
        let now = Utc::now();
        let updated_by = incoming
            .fields
            .get("sys_updated_by")
            .map(|v| v.effective_display().to_string())
            .filter(|s| !s.is_empty());

        if incoming.sys_mod_count > self.baseline.sys_mod_count
            || incoming.sys_updated_on != self.baseline.sys_updated_on
        {
            outcome.mod_count_advanced = true;
        }

        for (field, server_val) in &incoming.fields {
            let prev_baseline = self.baseline.fields.get(field).cloned();
            let baseline_changed = match &prev_baseline {
                Some(prev) => {
                    prev.value != server_val.value || prev.display_value != server_val.display_value
                }
                None => !server_val.is_empty(),
            };

            if !baseline_changed {
                continue;
            }

            if let Some(local_draft) = self.drafts.get(field).cloned() {
                // Update baseline silently so reverting later goes to the true server state.
                let original = prev_baseline.unwrap_or_default();
                self.baseline
                    .fields
                    .insert(field.clone(), server_val.clone());

                if local_draft.value == server_val.value {
                    // Server converged to the exact value the user drafted!
                    self.drafts.shift_remove(field);
                    self.conflicts.shift_remove(field);
                } else {
                    let conflict = FieldConflict {
                        field: field.clone(),
                        original_baseline: original,
                        local_draft,
                        server_latest: server_val.clone(),
                        updated_by: updated_by.clone(),
                        detected_at: now,
                    };
                    self.conflicts.insert(field.clone(), conflict.clone());
                    outcome.new_conflicts.push(conflict);
                }
            } else {
                let prev_disp = prev_baseline
                    .as_ref()
                    .map(|p| p.effective_display().to_string())
                    .unwrap_or_default();
                self.baseline
                    .fields
                    .insert(field.clone(), server_val.clone());

                // Don't pulse internal audit counters when highlighting user-visible field updates.
                if !matches!(
                    field.as_str(),
                    "sys_mod_count" | "sys_updated_on" | "sys_updated_by"
                ) {
                    let pulse = LiveFieldPulse {
                        field: field.clone(),
                        previous_display: prev_disp,
                        new_display: server_val.effective_display().to_string(),
                        updated_by: updated_by.clone(),
                        detected_at: now,
                    };
                    self.last_live_pulses.insert(field.clone(), pulse.clone());
                    outcome.updated_clean_fields.push(pulse);
                }
            }
        }

        if incoming.sys_updated_on.is_some() {
            self.baseline
                .sys_updated_on
                .clone_from(&incoming.sys_updated_on);
        }
        if incoming.sys_mod_count >= self.baseline.sys_mod_count {
            self.baseline.sys_mod_count = incoming.sys_mod_count;
        }

        outcome
    }

    /// Merges newly fetched `sys_journal_field` entries in newest-first chronological order,
    /// returning the count of newly inserted entries.
    pub fn merge_journals(&mut self, incoming: Vec<JournalEntry>) -> usize {
        let mut added = 0;
        for entry in incoming {
            if !self.journals.iter().any(|j| j.sys_id == entry.sys_id) {
                self.journals.push(entry);
                added += 1;
            }
        }
        if added > 0 {
            self.journals
                .sort_by(|a, b| b.sys_created_on.cmp(&a.sys_created_on));
        }
        added
    }

    pub fn latest_journal_timestamp(&self) -> Option<&str> {
        self.journals.first().map(|j| j.sys_created_on.as_str())
    }

    /// Builds the minimal JSON map to send to `PATCH` (existing record) or `POST` (new record).
    pub fn build_save_payload(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut payload = serde_json::Map::new();
        for (field, val) in &self.drafts {
            payload.insert(field.clone(), serde_json::Value::String(val.value.clone()));
        }
        for (journal_elem, text) in &self.journal_drafts {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                payload.insert(
                    journal_elem.clone(),
                    serde_json::Value::String(trimmed.to_string()),
                );
            }
        }
        payload
    }

    /// Commits the server response after a successful `PATCH` / `POST` save.
    pub fn commit_saved_record(&mut self, saved: SnRecord) {
        if !saved.sys_id.is_empty() {
            self.sys_id = Some(saved.sys_id.clone());
        }
        self.baseline.merge_from(&saved);
        self.baseline.is_partial = false;
        self.drafts.clear();
        self.journal_drafts.clear();
        self.conflicts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::InstanceProfile;
    use crate::layout::{FormGridLayout, GridVisualRow};
    use crate::query::{EncodedQuery, FilterOperator};
    use crate::schema::{
        ChoiceOption, DictionaryEntry, FieldType, TableDescriptor, TableHierarchy, TableSchema,
    };
    use std::collections::HashMap;

    fn make_record(mod_count: u64, state: (&str, &str), short_desc: &str) -> SnRecord {
        let mut fields = IndexMap::new();
        fields.insert("sys_id".into(), FieldValue::scalar("abc123"));
        fields.insert("number".into(), FieldValue::scalar("INC0010001"));
        fields.insert(
            "state".into(),
            FieldValue::new(state.0.to_string(), state.1.to_string()),
        );
        fields.insert("short_description".into(), FieldValue::scalar(short_desc));
        fields.insert(
            "sys_mod_count".into(),
            FieldValue::scalar(mod_count.to_string()),
        );
        fields.insert("sys_updated_by".into(), FieldValue::scalar("beth.anglin"));
        SnRecord::from_field_map("incident", fields, false)
    }

    #[test]
    fn instance_url_normalization_handles_shorthand_and_urls() {
        assert_eq!(
            InstanceProfile::normalize_instance_url("dev98765").unwrap(),
            "https://dev98765.service-now.com"
        );
        assert_eq!(
            InstanceProfile::normalize_instance_url("https://acme.service-now.com/").unwrap(),
            "https://acme.service-now.com"
        );
        let profile = InstanceProfile::new("Prod", "acme", "client_1", false).unwrap();
        assert_eq!(
            profile.amb_ws_url().unwrap(),
            "wss://acme.service-now.com/amb"
        );
        assert_eq!(
            profile.redirect_uri(),
            "http://127.0.0.1:8910/oauth/callback"
        );
    }

    #[test]
    fn table_inheritance_and_record_number_resolution_work() {
        let hierarchy = TableHierarchy::from_descriptors(vec![
            TableDescriptor {
                sys_id: "1".into(),
                name: "task".into(),
                label: "Task".into(),
                super_class: None,
                number_prefix: Some("TASK".into()),
                is_extendable: true,
            },
            TableDescriptor {
                sys_id: "2".into(),
                name: "incident".into(),
                label: "Incident".into(),
                super_class: Some("task".into()),
                number_prefix: Some("INC".into()),
                is_extendable: true,
            },
        ]);

        assert_eq!(
            hierarchy.inheritance_chain("incident"),
            vec!["incident".to_string(), "task".to_string()]
        );
        assert_eq!(
            hierarchy.resolve_record_number("inc0010099"),
            Some(("incident".into(), "INC0010099".into()))
        );
    }

    #[test]
    fn encoded_query_roundtrips_clauses_and_sorts() {
        let raw = "active=true^priority<=2^short_descriptionLIKEvpn^ORDERBYDESCnumber";
        let mut parsed = EncodedQuery::parse(raw);
        let clauses: Vec<_> = parsed.groups[0]
            .rows
            .iter()
            .filter_map(|row| row.as_single_clause())
            .collect();
        assert_eq!(clauses.len(), 3);
        assert_eq!(clauses[1].operator, FilterOperator::LessOrEqual);
        assert_eq!(clauses[2].operator, FilterOperator::Contains);
        assert_eq!(parsed.sorts.len(), 1);
        assert!(parsed.sorts[0].descending);
        assert_eq!(parsed.to_encoded_string(), raw);

        parsed.toggle_sort("number");
        assert!(!parsed.sorts[0].descending);
    }

    #[test]
    fn live_delta_updates_clean_fields_and_protects_dirty_drafts() {
        let initial = make_record(1, ("1", "New"), "VPN cannot connect");
        let mut session = FormSessionState::from_existing_record("inst-1".into(), initial);

        // User edits `short_description` locally, leaving `state` clean.
        session.edit_field(
            "short_description",
            FieldValue::scalar("VPN drops on macOS 27"),
        );
        assert!(session.is_dirty());

        // Server pushes an update modifying BOTH `state` and `short_description`!
        let server_push = make_record(2, ("2", "In Progress"), "VPN gateway maintenance");
        let outcome = session.apply_server_record(&server_push);

        // 1. Clean field `state` updated immediately and emitted a live pulse.
        assert_eq!(session.effective_display("state"), "In Progress");
        assert_eq!(outcome.updated_clean_fields.len(), 1);
        assert_eq!(
            outcome.updated_clean_fields[0].field,
            "short_description".replace("short_description", "state")
        );

        // 2. Dirty field `short_description` preserved the user's draft and flagged a conflict!
        assert_eq!(
            session.effective_display("short_description"),
            "VPN drops on macOS 27"
        );
        assert_eq!(outcome.new_conflicts.len(), 1);
        assert_eq!(
            outcome.new_conflicts[0].server_latest.value,
            "VPN gateway maintenance"
        );

        // 3. Save payload only includes dirty fields + non-empty journals.
        session.edit_journal("work_notes", "Investigating tunnel MTU");
        let payload = session.build_save_payload();
        assert_eq!(payload.len(), 2);
        assert_eq!(payload["short_description"], "VPN drops on macOS 27");
        assert_eq!(payload["work_notes"], "Investigating tunnel MTU");

        // 4. Resolving conflict with server value clears draft & conflict.
        session.resolve_conflict_accept_server("short_description");
        assert!(!session.is_field_dirty("short_description"));
        assert_eq!(
            session.effective_display("short_description"),
            "VPN gateway maintenance"
        );
    }

    #[test]
    fn auto_grid_rearrangement_and_visual_row_packing_work() {
        let mut entries_by_table = HashMap::new();
        entries_by_table.insert(
            "incident".to_string(),
            vec![
                DictionaryEntry {
                    element: "number".into(),
                    column_label: "Number".into(),
                    defined_on_table: "task".into(),
                    internal_type: "string".into(),
                    field_type: FieldType::String,
                    max_length: Some(40),
                    mandatory: false,
                    read_only: true,
                    active: true,
                    display: true,
                    default_value: None,
                    dependent_on: None,
                },
                DictionaryEntry {
                    element: "state".into(),
                    column_label: "State".into(),
                    defined_on_table: "task".into(),
                    internal_type: "integer".into(),
                    field_type: FieldType::Choice,
                    max_length: Some(40),
                    mandatory: false,
                    read_only: false,
                    active: true,
                    display: false,
                    default_value: None,
                    dependent_on: None,
                },
                DictionaryEntry {
                    element: "short_description".into(),
                    column_label: "Short description".into(),
                    defined_on_table: "task".into(),
                    internal_type: "string".into(),
                    field_type: FieldType::String,
                    max_length: Some(160),
                    mandatory: true,
                    read_only: false,
                    active: true,
                    display: false,
                    default_value: None,
                    dependent_on: None,
                },
            ],
        );

        let mut choices_by_table = HashMap::new();
        let mut state_choices = HashMap::new();
        state_choices.insert(
            "state".to_string(),
            vec![ChoiceOption {
                element: "state".into(),
                value: "1".into(),
                label: "New".into(),
                sequence: 1,
                dependent_value: None,
                inactive: false,
            }],
        );
        choices_by_table.insert("incident".to_string(), state_choices);

        let schema = TableSchema::assemble(
            "incident",
            "Incident",
            vec!["incident".into()],
            &entries_by_table,
            &choices_by_table,
            None,
        );

        let mut grid = FormGridLayout::generate_smart_default(&schema);
        let rows = grid.compute_visual_rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            rows[0],
            GridVisualRow::TwoColumnPair { right: Some(_), .. }
        ));
        assert!(
            matches!(rows[1], GridVisualRow::FullWidthField { ref element, .. } if element == "short_description")
        );

        // Rearrange: move `short_description` to top & toggle `number` to full width
        grid.move_field_relative("short_description", "number", false);
        grid.toggle_span("number");
        assert!(grid.is_customized);
        let rows_after = grid.compute_visual_rows();
        assert!(
            matches!(rows_after[0], GridVisualRow::FullWidthField { ref element, .. } if element == "short_description")
        );
        assert!(
            matches!(rows_after[1], GridVisualRow::FullWidthField { ref element, .. } if element == "number")
        );
    }
}
