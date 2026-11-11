//! Normalized ServiceNow record, field value, journal, and reference lookup models.

use indexmap::IndexMap;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Dual raw/display representation returned by ServiceNow when `sysparm_display_value=all`.
///
/// Retaining both values avoids N+1 reference lookups when rendering tables/forms
/// while still writing canonical raw values (`sys_id`, choice integer, UTC timestamp)
/// back on `PATCH` / `POST`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FieldValue {
    pub value: String,
    pub display_value: String,
}

impl FieldValue {
    pub fn new(value: impl Into<String>, display_value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            display_value: display_value.into(),
        }
    }

    pub fn scalar(val: impl Into<String>) -> Self {
        let s = val.into();
        Self {
            value: s.clone(),
            display_value: s,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty() && self.display_value.is_empty()
    }

    /// Best human-facing string to render in lists, badges, and read-only views.
    pub fn effective_display(&self) -> &str {
        if !self.display_value.is_empty() {
            &self.display_value
        } else {
            &self.value
        }
    }
}

fn scalar_json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

impl<'de> Deserialize<'de> for FieldValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldValueVisitor;

        impl<'de> Visitor<'de> for FieldValueVisitor {
            type Value = FieldValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a ServiceNow field object {value, display_value} or scalar value")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(FieldValue::default())
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(FieldValue::default())
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(FieldValue::scalar(v.to_string()))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(FieldValue::scalar(v.to_string()))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(FieldValue::scalar(v.to_string()))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(FieldValue::scalar(v.to_string()))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FieldValue::scalar(v))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(FieldValue::scalar(v))
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut value = None;
                let mut display_value = None;

                while let Some(key) = map.next_key::<String>()? {
                    let raw: serde_json::Value = map.next_value()?;
                    match key.as_str() {
                        "value" => value = Some(scalar_json_to_string(&raw)),
                        "display_value" => display_value = Some(scalar_json_to_string(&raw)),
                        _ => {}
                    }
                }

                let val = value.unwrap_or_default();
                let disp = display_value.unwrap_or_else(|| val.clone());
                Ok(FieldValue::new(val, disp))
            }
        }

        deserializer.deserialize_any(FieldValueVisitor)
    }
}

/// Normalized ServiceNow Table API record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnRecord {
    pub table: String,
    pub sys_id: String,
    pub fields: IndexMap<String, FieldValue>,
    pub sys_updated_on: Option<String>,
    pub sys_mod_count: u64,
    /// `true` when hydrated from a list query (`sysparm_fields` subset) rather than a full record GET.
    pub is_partial: bool,
}

impl SnRecord {
    pub fn from_field_map(
        table: impl Into<String>,
        fields: IndexMap<String, FieldValue>,
        is_partial: bool,
    ) -> Self {
        let sys_id = fields
            .get("sys_id")
            .map(|f| f.value.clone())
            .unwrap_or_default();
        let sys_updated_on = fields
            .get("sys_updated_on")
            .map(|f| f.value.clone())
            .filter(|s| !s.is_empty());
        let sys_mod_count = fields
            .get("sys_mod_count")
            .and_then(|f| f.value.parse::<u64>().ok())
            .unwrap_or(0);

        Self {
            table: table.into(),
            sys_id,
            fields,
            sys_updated_on,
            sys_mod_count,
            is_partial,
        }
    }

    pub fn new_draft(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            sys_id: String::new(),
            fields: IndexMap::new(),
            sys_updated_on: None,
            sys_mod_count: 0,
            is_partial: false,
        }
    }

    pub fn get_raw(&self, field: &str) -> &str {
        self.fields
            .get(field)
            .map(|f| f.value.as_str())
            .unwrap_or("")
    }

    pub fn get_display(&self, field: &str) -> &str {
        self.fields
            .get(field)
            .map(|f| f.effective_display())
            .unwrap_or("")
    }

    /// Resolves a concise primary identifier (`INC0010001`, record `name`, or truncated `sys_id`).
    pub fn primary_identifier(&self) -> String {
        for candidate in ["number", "name", "u_number", "title", "user_name"] {
            let val = self.get_display(candidate);
            if !val.is_empty() {
                return val.to_string();
            }
        }
        if self.sys_id.is_empty() {
            format!("New {}", self.table)
        } else {
            self.sys_id.chars().take(8).collect()
        }
    }

    /// Resolves a human-readable summary subtitle (`short_description`, `description`, etc.).
    pub fn summary_text(&self) -> Option<String> {
        for candidate in ["short_description", "name", "label", "email", "description"] {
            let val = self.get_display(candidate);
            if !val.is_empty() && val != self.primary_identifier() {
                return Some(val.to_string());
            }
        }
        None
    }

    /// Merges fields from `newer` into `self` without discarding previously cached full-record
    /// columns when `newer` is a partial list projection.
    pub fn merge_from(&mut self, newer: &SnRecord) {
        for (k, v) in &newer.fields {
            self.fields.insert(k.clone(), v.clone());
        }
        if newer.sys_updated_on.is_some() {
            self.sys_updated_on.clone_from(&newer.sys_updated_on);
        }
        if newer.sys_mod_count >= self.sys_mod_count {
            self.sys_mod_count = newer.sys_mod_count;
        }
        if !newer.is_partial {
            self.is_partial = false;
        }
    }
}

/// Paginated List View payload combining records with total server row count and sync watermark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPage {
    pub table: String,
    pub encoded_query: String,
    pub offset: usize,
    pub limit: usize,
    pub total_count: Option<usize>,
    pub records: Vec<SnRecord>,
    pub max_updated_on: Option<String>,
    pub from_cache: bool,
}

/// Activity Stream entry from ServiceNow's `sys_journal_field` table (`work_notes`, `comments`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub sys_id: String,
    pub element: String,
    pub element_id: String,
    pub value: String,
    pub sys_created_on: String,
    pub sys_created_by: String,
}

impl JournalEntry {
    pub fn is_work_note(&self) -> bool {
        self.element == "work_notes"
    }
}

/// Typeahead item for Reference fields (`sys_user`, `sys_user_group`, `cmdb_ci`, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCandidate {
    pub target_table: String,
    pub sys_id: String,
    pub display_value: String,
    pub secondary: Option<String>,
}
