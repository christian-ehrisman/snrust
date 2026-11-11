//! ServiceNow table inheritance (`sys_db_object`), dictionary (`sys_dictionary`),
//! and choice list (`sys_choice`) metadata models.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Entry from `sys_db_object` describing a table and its optional parent (`super_class`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDescriptor {
    pub sys_id: String,
    pub name: String,
    pub label: String,
    /// Parent table `name` (resolved from `super_class`), e.g. `Some("task")` for `incident`.
    pub super_class: Option<String>,
    /// Optional record number prefix (`INC`, `CHG`, `PRB`, `RITM`) from `sys_number`.
    pub number_prefix: Option<String>,
    pub is_extendable: bool,
}

/// In-memory graph of `sys_db_object` relationships used to resolve inherited dictionary fields
/// and SN Utils-style record number prefixes (`INC0010042` -> `incident`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableHierarchy {
    tables: HashMap<String, TableDescriptor>,
    prefix_index: HashMap<String, String>,
}

impl TableHierarchy {
    pub fn from_descriptors(descriptors: impl IntoIterator<Item = TableDescriptor>) -> Self {
        let mut tables = HashMap::new();
        let mut prefix_index = HashMap::new();

        for desc in descriptors {
            if let Some(prefix) = &desc.number_prefix {
                let normalized = prefix.trim().to_ascii_uppercase();
                if !normalized.is_empty() {
                    prefix_index.insert(normalized, desc.name.clone());
                }
            }
            tables.insert(desc.name.clone(), desc);
        }

        // Seed well-known ServiceNow task prefixes as fallbacks before `sys_number` is synced.
        for (prefix, table) in [
            ("INC", "incident"),
            ("PRB", "problem"),
            ("CHG", "change_request"),
            ("CTASK", "change_task"),
            ("REQ", "sc_request"),
            ("RITM", "sc_req_item"),
            ("SCTASK", "sc_task"),
            ("TASK", "task"),
            ("KB", "kb_knowledge"),
        ] {
            prefix_index
                .entry(prefix.to_string())
                .or_insert_with(|| table.to_string());
        }

        Self {
            tables,
            prefix_index,
        }
    }

    pub fn get(&self, table: &str) -> Option<&TableDescriptor> {
        self.tables.get(table)
    }

    pub fn all_tables(&self) -> impl Iterator<Item = &TableDescriptor> {
        self.tables.values()
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Returns `[table, parent, grandparent, ...]` from most-specific child up to root.
    ///
    /// Built-in fallback chains cover standard ITSM `task` extensions even before the full
    /// `sys_db_object` catalog finishes hydrating on first connect.
    pub fn inheritance_chain(&self, table: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        let mut current = Some(table.to_string());

        while let Some(tbl) = current {
            if !visited.insert(tbl.clone()) {
                break;
            }
            let next = self
                .tables
                .get(&tbl)
                .and_then(|d| d.super_class.clone())
                .or_else(|| Self::well_known_parent(&tbl).map(str::to_string));
            chain.push(tbl);
            current = next;
        }

        chain
    }

    fn well_known_parent(table: &str) -> Option<&'static str> {
        match table {
            "incident" | "problem" | "change_request" | "change_task" | "sc_request"
            | "sc_req_item" | "sc_task" => Some("task"),
            "cmdb_ci_server" | "cmdb_ci_computer" | "cmdb_ci_appl" => Some("cmdb_ci"),
            _ => None,
        }
    }

    /// Parses a record number token like `INC0010042` into `Some(("incident", "INC0010042"))`.
    pub fn resolve_record_number(&self, input: &str) -> Option<(String, String)> {
        let trimmed = input.trim().to_ascii_uppercase();
        let alpha_len = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        if alpha_len < 2 || alpha_len >= trimmed.len() {
            return None;
        }
        let (prefix, rest) = trimmed.split_at(alpha_len);
        if !rest.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        self.prefix_index
            .get(prefix)
            .map(|table| (table.clone(), trimmed))
    }
}

/// Normalized widget/data type derived from `sys_dictionary.internal_type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldType {
    String,
    MultiLineText,
    Choice,
    Integer,
    Decimal,
    Boolean,
    Reference { target_table: String },
    Date,
    DateTime,
    JournalInput,
    JournalList,
    Html,
    Email,
    Url,
    PhoneNumber,
    Currency,
    SysClassName,
    Other(String),
}

impl FieldType {
    pub fn from_dictionary(
        internal_type: &str,
        max_length: Option<u32>,
        reference_table: Option<&str>,
        has_choices: bool,
    ) -> Self {
        let norm = internal_type.trim().to_ascii_lowercase();
        if let Some(target) = reference_table.map(str::trim).filter(|s| !s.is_empty())
            && (norm == "reference" || norm == "glide_list")
        {
            return Self::Reference {
                target_table: target.to_string(),
            };
        }

        match norm.as_str() {
            "boolean" => Self::Boolean,
            "choice" => Self::Choice,
            "integer" | "longint" | "order_index" => {
                if has_choices {
                    Self::Choice
                } else {
                    Self::Integer
                }
            }
            "decimal" | "float" | "percent_complete" => Self::Decimal,
            "glide_date" | "due_date" => Self::Date,
            "glide_date_time" | "calendar_date_time" | "datetime" => Self::DateTime,
            "journal_input" | "journal" => Self::JournalInput,
            "journal_list" => Self::JournalList,
            "html" | "translated_html" => Self::Html,
            "email" => Self::Email,
            "url" => Self::Url,
            "ph_number" | "phone_number_e164" => Self::PhoneNumber,
            "currency" | "price" => Self::Currency,
            "sys_class_name" => Self::SysClassName,
            "string" | "string_full_utf8" | "translated_text" | "char" | "" => {
                if has_choices {
                    Self::Choice
                } else if max_length.unwrap_or(40) >= 255 {
                    Self::MultiLineText
                } else {
                    Self::String
                }
            }
            "script" | "script_plain" | "xml" | "conditions" => Self::MultiLineText,
            other => {
                if has_choices {
                    Self::Choice
                } else {
                    Self::Other(other.to_string())
                }
            }
        }
    }

    /// Heuristic for whether a field should span both columns (`ColSpan::Two`) in the Auto-Grid.
    pub fn prefers_full_width(&self, element: &str, max_length: Option<u32>) -> bool {
        if matches!(
            element,
            "short_description"
                | "description"
                | "close_notes"
                | "work_around"
                | "justification"
                | "implementation_plan"
                | "backout_plan"
                | "test_plan"
        ) {
            return true;
        }
        match self {
            Self::MultiLineText | Self::Html | Self::JournalInput | Self::JournalList => true,
            Self::String => max_length.unwrap_or(40) >= 160,
            _ => false,
        }
    }
}

/// Column definition resolved from `sys_dictionary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryEntry {
    pub element: String,
    pub column_label: String,
    pub defined_on_table: String,
    pub internal_type: String,
    pub field_type: FieldType,
    pub max_length: Option<u32>,
    pub mandatory: bool,
    pub read_only: bool,
    pub active: bool,
    pub display: bool,
    pub default_value: Option<String>,
    pub dependent_on: Option<String>,
}

impl DictionaryEntry {
    pub fn is_system_audit_field(&self) -> bool {
        matches!(
            self.element.as_str(),
            "sys_id"
                | "sys_created_on"
                | "sys_created_by"
                | "sys_updated_on"
                | "sys_updated_by"
                | "sys_mod_count"
                | "sys_domain"
                | "sys_domain_path"
                | "sys_class_path"
                | "sys_tags"
                | "sys_package"
                | "sys_policy"
                | "sys_scope"
                | "sys_update_name"
        )
    }

    pub fn is_journal(&self) -> bool {
        matches!(
            self.field_type,
            FieldType::JournalInput | FieldType::JournalList
        )
    }
}

/// Dropdown option resolved from `sys_choice`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub element: String,
    pub value: String,
    pub label: String,
    pub sequence: i32,
    pub dependent_value: Option<String>,
    pub inactive: bool,
}

/// Complete merged schema for a table (including inherited parent columns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub table: String,
    pub label: String,
    /// Inheritance chain from child to root (`["incident", "task"]`).
    pub hierarchy: Vec<String>,
    pub fields: IndexMap<String, DictionaryEntry>,
    pub choices: HashMap<String, Vec<ChoiceOption>>,
    pub default_list_columns: Vec<String>,
    pub display_field: String,
}

impl TableSchema {
    /// Builds a merged schema where child dictionary overrides win over parent definitions.
    pub fn assemble(
        table: impl Into<String>,
        label: impl Into<String>,
        hierarchy: Vec<String>,
        entries_by_table: &HashMap<String, Vec<DictionaryEntry>>,
        choices_by_table: &HashMap<String, HashMap<String, Vec<ChoiceOption>>>,
        custom_default_list_columns: Option<Vec<String>>,
    ) -> Self {
        let table = table.into();
        let label = label.into();
        let mut merged_choices: HashMap<String, Vec<ChoiceOption>> = HashMap::new();

        // Merge from root -> child so child table choice overrides take precedence!
        for ancestor in hierarchy.iter().rev() {
            if let Some(table_choices) = choices_by_table.get(ancestor) {
                for (elem, opts) in table_choices {
                    let mut active_opts: Vec<ChoiceOption> =
                        opts.iter().filter(|c| !c.inactive).cloned().collect();
                    active_opts.sort_by_key(|c| c.sequence);
                    if !active_opts.is_empty() {
                        merged_choices.insert(elem.clone(), active_opts);
                    }
                }
            }
        }

        let mut merged_fields: IndexMap<String, DictionaryEntry> = IndexMap::new();
        for ancestor in hierarchy.iter().rev() {
            if let Some(entries) = entries_by_table.get(ancestor) {
                for entry in entries {
                    if entry.element.is_empty() || entry.internal_type == "collection" {
                        continue;
                    }
                    let mut resolved = entry.clone();
                    let has_choices = merged_choices.contains_key(&resolved.element);
                    let ref_target = match &resolved.field_type {
                        FieldType::Reference { target_table } => Some(target_table.clone()),
                        _ => None,
                    };
                    resolved.field_type = FieldType::from_dictionary(
                        &resolved.internal_type,
                        resolved.max_length,
                        ref_target.as_deref(),
                        has_choices,
                    );
                    merged_fields.insert(resolved.element.clone(), resolved);
                }
            }
        }

        let display_field = merged_fields
            .values()
            .find(|f| f.display)
            .map(|f| f.element.clone())
            .or_else(|| {
                ["number", "name", "short_description", "u_name", "title"]
                    .into_iter()
                    .find(|cand| merged_fields.contains_key(*cand))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "sys_id".to_string());

        let default_list_columns = custom_default_list_columns
            .filter(|cols| !cols.is_empty())
            .unwrap_or_else(|| Self::derive_default_list_columns(&merged_fields));

        Self {
            table,
            label,
            hierarchy,
            fields: merged_fields,
            choices: merged_choices,
            default_list_columns,
            display_field,
        }
    }

    pub fn derive_default_list_columns(fields: &IndexMap<String, DictionaryEntry>) -> Vec<String> {
        let preferred = [
            "number",
            "name",
            "short_description",
            "caller_id",
            "requested_for",
            "priority",
            "state",
            "category",
            "assignment_group",
            "assigned_to",
            "active",
            "sys_updated_on",
        ];

        let mut cols: Vec<String> = preferred
            .into_iter()
            .filter(|name| fields.contains_key(*name))
            .map(str::to_string)
            .collect();

        if cols.len() < 5 {
            for (name, entry) in fields {
                if cols.len() >= 7 {
                    break;
                }
                if entry.active
                    && !entry.is_system_audit_field()
                    && !entry.is_journal()
                    && !cols.contains(name)
                {
                    cols.push(name.clone());
                }
            }
        }

        if cols.is_empty() {
            cols.push("sys_id".to_string());
        }
        cols
    }

    /// Returns choice options applicable to `element`, optionally filtered by `dependent_value`.
    pub fn choices_for(&self, element: &str, dependent_val: Option<&str>) -> Vec<&ChoiceOption> {
        let Some(list) = self.choices.get(element) else {
            return Vec::new();
        };

        if let Some(dep) = dependent_val.filter(|s| !s.is_empty()) {
            let filtered: Vec<&ChoiceOption> = list
                .iter()
                .filter(|c| match c.dependent_value.as_deref() {
                    Some(dv) if !dv.is_empty() => dv == dep,
                    _ => true,
                })
                .collect();
            if !filtered.is_empty() {
                return filtered;
            }
        }

        list.iter().collect()
    }
}
