//! Parallel ServiceNow metadata hydration (`sys_db_object`, `sys_number`, `sys_dictionary`,
//! `sys_choice`, `sys_ui_list`) with graceful non-admin ACL fallbacks.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use sn_core::{
    ChoiceOption, DictionaryEntry, FieldType, FieldValue, SnError, SnResult, TableDescriptor,
    TableHierarchy, TableSchema,
};

use crate::client::{ListRecordsParams, SnClient};

impl SnClient {
    /// Hydrates the full instance table catalog by querying `sys_db_object` and `sys_number`
    /// concurrently and resolving `super_class` references into parent table `name`s.
    pub async fn fetch_table_catalog(&self) -> SnResult<Vec<TableDescriptor>> {
        let (db_objects, prefixes) =
            tokio::try_join!(self.fetch_db_object_rows(), self.fetch_number_prefixes())?;

        let mut id_to_name: HashMap<String, String> = HashMap::with_capacity(db_objects.len());
        let mut known_names: HashSet<String> = HashSet::with_capacity(db_objects.len());

        for row in &db_objects {
            let sys_id = raw_str(row, "sys_id");
            let name = raw_str(row, "name");
            if !name.is_empty() {
                known_names.insert(name.clone());
                if !sys_id.is_empty() {
                    id_to_name.insert(sys_id, name);
                }
            }
        }

        let mut descriptors = Vec::with_capacity(db_objects.len());
        for row in db_objects {
            let name = raw_str(&row, "name");
            if name.is_empty() {
                continue;
            }
            let sys_id = raw_str(&row, "sys_id");
            let label = display_or_raw(&row, "label")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| humanize_identifier(&name));

            let super_class = row.get("super_class").and_then(|sc| {
                let raw = sc.value.trim();
                if raw.is_empty() {
                    return None;
                }
                if let Some(parent_name) = id_to_name.get(raw) {
                    return Some(parent_name.clone());
                }
                if known_names.contains(raw) {
                    return Some(raw.to_string());
                }
                let disp = sc.display_value.trim().to_ascii_lowercase();
                if !disp.is_empty() && known_names.contains(&disp) {
                    return Some(disp);
                }
                Some(raw.to_string())
            });

            let number_prefix = prefixes.get(&name).cloned();
            let is_extendable = parse_sn_bool(row.get("is_extendable"));

            descriptors.push(TableDescriptor {
                sys_id,
                name,
                label,
                super_class,
                number_prefix,
                is_extendable,
            });
        }

        Ok(descriptors)
    }

    /// Hydrates and merges the complete [`TableSchema`] for `table` across its `inheritance_chain`.
    ///
    /// Runs `sys_dictionary`, `sys_choice`, and `sys_ui_list` queries concurrently. Tolerates
    /// `403 Forbidden` on `sys_ui_list` or `sys_choice` for non-admin ITIL roles, and even
    /// synthesizes a working schema from a sample record if `sys_dictionary` itself is ACL-blocked.
    pub async fn fetch_table_schema(
        &self,
        table: &str,
        hierarchy: &TableHierarchy,
    ) -> SnResult<TableSchema> {
        let chain = hierarchy.inheritance_chain(table);
        let chain_csv = chain.join(",");

        let (dict_res, choices_res, ui_cols_res) = tokio::join!(
            self.fetch_dictionary_by_chain(&chain_csv, hierarchy),
            self.fetch_choices_by_chain(&chain_csv),
            self.fetch_default_ui_list_columns(&chain),
        );

        let choices_by_table = match choices_res {
            Ok(map) => map,
            Err(SnError::Api {
                status: 403 | 404, ..
            }) => {
                tracing::warn!(
                    table,
                    "sys_choice returned 403/404; proceeding without server choice lists"
                );
                HashMap::new()
            }
            Err(err) => return Err(err),
        };

        let custom_default_cols = match ui_cols_res {
            Ok(cols) => cols,
            Err(SnError::Api {
                status: 403 | 404, ..
            }) => {
                tracing::debug!(
                    table,
                    "sys_ui_list inaccessible (403/404); falling back to derived list columns"
                );
                None
            }
            Err(err) => {
                tracing::warn!(table, error = %err, "sys_ui_list lookup failed; using defaults");
                None
            }
        };

        let mut resolved_label = hierarchy
            .get(table)
            .map(|d| d.label.clone())
            .filter(|s| !s.is_empty());

        let entries_by_table = match dict_res {
            Ok((entries, collection_labels)) => {
                if resolved_label.is_none() {
                    resolved_label = collection_labels.get(table).cloned();
                }
                entries
            }
            Err(SnError::Api { status: 403, .. }) => {
                tracing::warn!(
                    table,
                    "sys_dictionary returned 403 Forbidden; synthesizing schema from sample record"
                );
                let synthetic = self.synthesize_entries_from_sample(table).await?;
                let mut map = HashMap::new();
                map.insert(table.to_string(), synthetic);
                map
            }
            Err(err) => return Err(err),
        };

        let label = resolved_label.unwrap_or_else(|| humanize_identifier(table));

        Ok(TableSchema::assemble(
            table,
            label,
            chain,
            &entries_by_table,
            &choices_by_table,
            custom_default_cols,
        ))
    }

    async fn fetch_db_object_rows(&self) -> SnResult<Vec<IndexMap<String, FieldValue>>> {
        self.get_raw_table_rows(
            "sys_db_object",
            &[
                ("sysparm_query", "nameISNOTEMPTY^ORDERBYname".to_string()),
                (
                    "sysparm_fields",
                    "sys_id,name,label,super_class,is_extendable".to_string(),
                ),
                ("sysparm_limit", "10000".to_string()),
                ("sysparm_no_count", "true".to_string()),
            ],
        )
        .await
    }

    async fn fetch_number_prefixes(&self) -> SnResult<HashMap<String, String>> {
        let rows = match self
            .get_raw_table_rows(
                "sys_number",
                &[
                    (
                        "sysparm_query",
                        "categoryISNOTEMPTY^prefixISNOTEMPTY".to_string(),
                    ),
                    ("sysparm_fields", "category,prefix".to_string()),
                    ("sysparm_limit", "5000".to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await
        {
            Ok(rows) => rows,
            Err(SnError::Api {
                status: 403 | 404, ..
            }) => {
                tracing::debug!(
                    "sys_number inaccessible (403/404); using built-in prefix fallbacks"
                );
                return Ok(HashMap::new());
            }
            Err(err) => return Err(err),
        };

        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            let category = raw_str(&row, "category");
            let prefix = display_or_raw(&row, "prefix").unwrap_or_default();
            if !category.is_empty() && !prefix.is_empty() {
                map.insert(category, prefix.trim().to_ascii_uppercase());
            }
        }
        Ok(map)
    }

    async fn fetch_dictionary_by_chain(
        &self,
        chain_csv: &str,
        hierarchy: &TableHierarchy,
    ) -> SnResult<(
        HashMap<String, Vec<DictionaryEntry>>,
        HashMap<String, String>,
    )> {
        let rows = self
            .get_raw_table_rows(
                "sys_dictionary",
                &[
                    ("sysparm_query", format!("nameIN{chain_csv}^active=true")),
                    (
                        "sysparm_fields",
                        "name,element,column_label,internal_type,max_length,mandatory,read_only,active,display,default_value,reference,dependent"
                            .to_string(),
                    ),
                    ("sysparm_limit", "5000".to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        let id_to_table: HashMap<&str, &str> = hierarchy
            .all_tables()
            .filter(|d| !d.sys_id.is_empty())
            .map(|d| (d.sys_id.as_str(), d.name.as_str()))
            .collect();

        let mut by_table: HashMap<String, Vec<DictionaryEntry>> = HashMap::new();
        let mut collection_labels: HashMap<String, String> = HashMap::new();

        for row in rows {
            let defined_on_table = raw_str(&row, "name");
            if defined_on_table.is_empty() {
                continue;
            }
            let element = raw_str(&row, "element");
            let mut internal_type = raw_str(&row, "internal_type");

            if element.is_empty() || internal_type == "collection" {
                if let Some(lbl) = display_or_raw(&row, "column_label").filter(|s| !s.is_empty()) {
                    collection_labels.insert(defined_on_table, lbl);
                }
                continue;
            }

            let ref_target = row
                .get("reference")
                .map(|f| f.value.trim())
                .filter(|s| !s.is_empty())
                .map(|raw_ref| {
                    id_to_table
                        .get(raw_ref)
                        .copied()
                        .unwrap_or(raw_ref)
                        .to_string()
                });

            if internal_type.is_empty() && ref_target.is_some() {
                internal_type = "reference".to_string();
            }

            let max_length = row
                .get("max_length")
                .and_then(|f| f.value.trim().parse::<u32>().ok());
            let column_label = display_or_raw(&row, "column_label")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| humanize_identifier(&element));
            let mandatory = parse_sn_bool(row.get("mandatory"));
            let read_only = parse_sn_bool(row.get("read_only"));
            let active = row
                .get("active")
                .map(|f| f.value.is_empty() || matches!(f.value.as_str(), "true" | "1"))
                .unwrap_or(true);
            let display = parse_sn_bool(row.get("display"));
            let default_value = row
                .get("default_value")
                .map(|f| f.value.clone())
                .filter(|s| !s.is_empty());
            let dependent_on = row
                .get("dependent")
                .map(|f| f.value.clone())
                .filter(|s| !s.is_empty());

            let field_type = FieldType::from_dictionary(
                &internal_type,
                max_length,
                ref_target.as_deref(),
                false,
            );

            by_table
                .entry(defined_on_table.clone())
                .or_default()
                .push(DictionaryEntry {
                    element,
                    column_label,
                    defined_on_table,
                    internal_type,
                    field_type,
                    max_length,
                    mandatory,
                    read_only,
                    active,
                    display,
                    default_value,
                    dependent_on,
                });
        }

        Ok((by_table, collection_labels))
    }

    async fn fetch_choices_by_chain(
        &self,
        chain_csv: &str,
    ) -> SnResult<HashMap<String, HashMap<String, Vec<ChoiceOption>>>> {
        let rows = self
            .get_raw_table_rows(
                "sys_choice",
                &[
                    ("sysparm_query", format!("nameIN{chain_csv}^inactive=false")),
                    (
                        "sysparm_fields",
                        "name,element,value,label,sequence,dependent_value,inactive".to_string(),
                    ),
                    ("sysparm_limit", "5000".to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        let mut by_table: HashMap<String, HashMap<String, Vec<ChoiceOption>>> = HashMap::new();
        for row in rows {
            let table_name = raw_str(&row, "name");
            let element = raw_str(&row, "element");
            if table_name.is_empty() || element.is_empty() {
                continue;
            }
            let value = raw_str(&row, "value");
            let label = display_or_raw(&row, "label")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| value.clone());
            let sequence = row
                .get("sequence")
                .and_then(|f| f.value.trim().parse::<i32>().ok())
                .unwrap_or(0);
            let dependent_value = row
                .get("dependent_value")
                .map(|f| f.value.clone())
                .filter(|s| !s.is_empty());
            let inactive = parse_sn_bool(row.get("inactive"));

            by_table
                .entry(table_name)
                .or_default()
                .entry(element.clone())
                .or_default()
                .push(ChoiceOption {
                    element,
                    value,
                    label,
                    sequence,
                    dependent_value,
                    inactive,
                });
        }

        Ok(by_table)
    }

    async fn fetch_default_ui_list_columns(
        &self,
        chain: &[String],
    ) -> SnResult<Option<Vec<String>>> {
        let chain_csv = chain.join(",");
        let list_rows = self
            .get_raw_table_rows(
                "sys_ui_list",
                &[
                    (
                        "sysparm_query",
                        format!("nameIN{chain_csv}^parentISEMPTY^userISEMPTY"),
                    ),
                    ("sysparm_fields", "sys_id,name,view".to_string()),
                    ("sysparm_limit", "25".to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        // Prefer the most specific table in `chain` with the Default view.
        let chosen_list_id = chain.iter().find_map(|tbl| {
            list_rows.iter().find_map(|row| {
                if raw_str(row, "name") != *tbl {
                    return None;
                }
                let view_disp = display_or_raw(row, "view").unwrap_or_default();
                let is_default_view =
                    view_disp.is_empty() || view_disp.eq_ignore_ascii_case("Default view");
                if is_default_view {
                    let id = raw_str(row, "sys_id");
                    (!id.is_empty()).then_some(id)
                } else {
                    None
                }
            })
        });

        let Some(list_sys_id) = chosen_list_id else {
            return Ok(None);
        };

        let elem_rows = self
            .get_raw_table_rows(
                "sys_ui_list_element",
                &[
                    (
                        "sysparm_query",
                        format!("list_id={list_sys_id}^ORDERBYposition"),
                    ),
                    ("sysparm_fields", "element,position".to_string()),
                    ("sysparm_limit", "100".to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        let mut positioned: Vec<(i32, String)> = elem_rows
            .into_iter()
            .filter_map(|row| {
                let elem = raw_str(&row, "element");
                if elem.is_empty() || elem.starts_with('.') {
                    return None;
                }
                let pos = row
                    .get("position")
                    .and_then(|f| f.value.trim().parse::<i32>().ok())
                    .unwrap_or(0);
                Some((pos, elem))
            })
            .collect();

        positioned.sort_by_key(|(pos, _)| *pos);
        let cols: Vec<String> = positioned.into_iter().map(|(_, elem)| elem).collect();
        Ok((!cols.is_empty()).then_some(cols))
    }

    async fn synthesize_entries_from_sample(&self, table: &str) -> SnResult<Vec<DictionaryEntry>> {
        let sample_page = self
            .list_records(
                table,
                &ListRecordsParams {
                    encoded_query: String::new(),
                    fields: Vec::new(),
                    offset: 0,
                    limit: 1,
                    include_total_count: false,
                },
            )
            .await?;

        if let Some(sample) = sample_page.records.first() {
            let mut display_assigned = false;
            let mut entries = Vec::with_capacity(sample.fields.len());

            for (element, fv) in &sample.fields {
                let (internal_type, field_type, max_length) = infer_synthetic_type(element, fv);
                let is_display = !display_assigned
                    && matches!(element.as_str(), "number" | "name" | "short_description");
                if is_display {
                    display_assigned = true;
                }
                entries.push(DictionaryEntry {
                    element: element.clone(),
                    column_label: humanize_identifier(element),
                    defined_on_table: table.to_string(),
                    internal_type,
                    field_type,
                    max_length,
                    mandatory: false,
                    read_only: element == "sys_id" || element.starts_with("sys_"),
                    active: true,
                    display: is_display,
                    default_value: None,
                    dependent_on: None,
                });
            }
            if !entries.is_empty() {
                return Ok(entries);
            }
        }

        // Baseline fallback if the table currently has zero accessible rows.
        Ok([
            "sys_id",
            "number",
            "short_description",
            "state",
            "sys_updated_on",
        ]
        .into_iter()
        .map(|elem| DictionaryEntry {
            element: elem.to_string(),
            column_label: humanize_identifier(elem),
            defined_on_table: table.to_string(),
            internal_type: "string".to_string(),
            field_type: if elem == "sys_updated_on" {
                FieldType::DateTime
            } else {
                FieldType::String
            },
            max_length: Some(if elem == "short_description" { 160 } else { 40 }),
            mandatory: false,
            read_only: elem.starts_with("sys_"),
            active: true,
            display: elem == "number",
            default_value: None,
            dependent_on: None,
        })
        .collect())
    }
}

fn infer_synthetic_type(element: &str, fv: &FieldValue) -> (String, FieldType, Option<u32>) {
    match element {
        "sys_class_name" => {
            return (
                "sys_class_name".to_string(),
                FieldType::SysClassName,
                Some(80),
            );
        }
        "work_notes" | "comments" => {
            return ("journal_input".to_string(), FieldType::JournalInput, None);
        }
        "description" | "close_notes" | "work_around" => {
            return ("string".to_string(), FieldType::MultiLineText, Some(4000));
        }
        "short_description" => return ("string".to_string(), FieldType::String, Some(160)),
        _ => {}
    }

    if element.ends_with("_on") || element.ends_with("_at") {
        return ("glide_date_time".to_string(), FieldType::DateTime, None);
    }
    if matches!(fv.value.as_str(), "true" | "false") {
        return ("boolean".to_string(), FieldType::Boolean, None);
    }
    if element != "sys_id"
        && fv.value.len() == 32
        && fv.value.chars().all(|c| c.is_ascii_hexdigit())
    {
        let guessed_target = match element {
            "caller_id" | "assigned_to" | "opened_by" | "resolved_by" | "requested_for" => {
                "sys_user"
            }
            "assignment_group" => "sys_user_group",
            "cmdb_ci" => "cmdb_ci",
            "company" => "core_company",
            "location" => "cmn_location",
            _ => "",
        };
        if !guessed_target.is_empty() {
            return (
                "reference".to_string(),
                FieldType::Reference {
                    target_table: guessed_target.to_string(),
                },
                Some(32),
            );
        }
    }
    if fv.value.len() >= 255 {
        return ("string".to_string(), FieldType::MultiLineText, Some(4000));
    }
    ("string".to_string(), FieldType::String, Some(80))
}

fn raw_str(row: &IndexMap<String, FieldValue>, key: &str) -> String {
    row.get(key)
        .map(|f| f.value.trim().to_string())
        .unwrap_or_default()
}

fn display_or_raw(row: &IndexMap<String, FieldValue>, key: &str) -> Option<String> {
    row.get(key)
        .map(|f| f.effective_display().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_sn_bool(val: Option<&FieldValue>) -> bool {
    matches!(
        val.map(|f| f.value.trim().to_ascii_lowercase()).as_deref(),
        Some("true" | "1")
    )
}

fn humanize_identifier(ident: &str) -> String {
    ident
        .trim_start_matches("u_")
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part.eq_ignore_ascii_case("id") {
                "ID".to_string()
            } else if part.eq_ignore_ascii_case("ci") {
                "CI".to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => {
                        let mut word = first.to_uppercase().collect::<String>();
                        word.push_str(chars.as_str());
                        word
                    }
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
