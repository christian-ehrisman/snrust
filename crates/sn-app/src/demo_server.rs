//! Embedded loopback ServiceNow REST & real-time simulator for one-click Demo Sandbox mode.
//!
//! Lets users explore 120fps virtualized lists, 0ms SWR List-to-Form transitions, rearrangeable
//! 2-column Auto-Grids, reference lookups, `Cmd+S` PATCH saves, and live inbound server updates
//! (including dirty-field conflict protection) immediately without external credentials.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde_json::{Value, json};
use sn_core::{FieldValue, JournalEntry, SnRecord};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;
use url::Url;

#[derive(Clone)]
pub struct DemoSandboxServer {
    base_url: String,
    state: Arc<RwLock<DemoDataStore>>,
}

struct DemoDataStore {
    tables: HashMap<String, IndexMap<String, SnRecord>>,
    journals: Vec<JournalEntry>,
    next_number_seq: u64,
    next_journal_seq: u64,
}

impl DemoSandboxServer {
    pub async fn start(handle: &Handle) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let state = Arc::new(RwLock::new(DemoDataStore::seeded()));
        let state_bg = Arc::clone(&state);

        handle.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let st = Arc::clone(&state_bg);
                tokio::spawn(async move {
                    let _ = handle_http_connection(stream, st).await;
                });
            }
        });

        Ok(Self { base_url, state })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Mutates a record on the simulated server (just like another ITIL agent or Flow Designer
    /// updating the ticket in ServiceNow!) so `sn-realtime` automatically detects and streams
    /// the live field update + journal entry into any open Form or List view.
    pub fn simulate_remote_agent_update(&self, table: &str, sys_id: &str) -> bool {
        let mut store = self.state.write();
        let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

        let Some(tbl_map) = store.tables.get_mut(table) else {
            return false;
        };
        let Some(rec) = tbl_map.get_mut(sys_id) else {
            return false;
        };

        rec.sys_mod_count += 1;
        let mc = rec.sys_mod_count;
        rec.sys_updated_on = Some(now_str.clone());
        rec.fields
            .insert("sys_mod_count".into(), FieldValue::scalar(mc.to_string()));
        rec.fields
            .insert("sys_updated_on".into(), FieldValue::scalar(now_str.clone()));
        rec.fields.insert(
            "sys_updated_by".into(),
            FieldValue::new("sre.bot", "SRE Auto-Responder"),
        );

        // Cycle state/priority/assigned_to realistically so user sees crisp live field pulses!
        let next_state = match rec.get_raw("state") {
            "1" => FieldValue::new("2", "In Progress"),
            "2" => FieldValue::new("3", "On Hold"),
            "3" => FieldValue::new("6", "Resolved"),
            _ => FieldValue::new("2", "In Progress"),
        };
        rec.fields.insert("state".into(), next_state.clone());
        rec.fields.insert(
            "assigned_to".into(),
            FieldValue::new("usr_beth", "Beth Anglin"),
        );

        let j_seq = store.next_journal_seq;
        store.next_journal_seq += 1;
        store.journals.push(JournalEntry {
            sys_id: format!("demo_jrn_{j_seq}"),
            element: "work_notes".into(),
            element_id: sys_id.to_string(),
            value: format!(
                "Automated telemetry update #{mc}: transitioned ticket state to '{}' and assigned to Beth Anglin.",
                next_state.display_value
            ),
            sys_created_on: now_str,
            sys_created_by: "sre.bot".into(),
        });
        true
    }
}

impl DemoDataStore {
    fn seeded() -> Self {
        let mut tables: HashMap<String, IndexMap<String, SnRecord>> = HashMap::new();

        // Seed `sys_user`
        let users = [
            (
                "usr_beth",
                "Beth Anglin",
                "beth.anglin@acme.io",
                "Cloud SRE",
            ),
            (
                "usr_fred",
                "Fred Luddy",
                "fred.luddy@acme.io",
                "Platform Architecture",
            ),
            (
                "usr_abel",
                "Abel Tuter",
                "abel.tuter@acme.io",
                "Network Operations",
            ),
            (
                "usr_diana",
                "Diana Prince",
                "diana.prince@acme.io",
                "Security Operations",
            ),
            (
                "usr_marcus",
                "Marcus Chen",
                "marcus.chen@acme.io",
                "Database Reliability",
            ),
        ];
        let mut user_map = IndexMap::new();
        for (id, name, email, dept) in users {
            let mut f = IndexMap::new();
            f.insert("sys_id".into(), FieldValue::scalar(id));
            f.insert("name".into(), FieldValue::scalar(name));
            f.insert("email".into(), FieldValue::scalar(email));
            f.insert("department".into(), FieldValue::scalar(dept));
            f.insert("active".into(), FieldValue::new("true", "true"));
            f.insert(
                "sys_updated_on".into(),
                FieldValue::scalar("2026-11-11 08:00:00"),
            );
            f.insert("sys_mod_count".into(), FieldValue::scalar("1"));
            user_map.insert(
                id.to_string(),
                SnRecord::from_field_map("sys_user", f, false),
            );
        }
        tables.insert("sys_user".into(), user_map);

        // Seed `sys_user_group`
        let groups = [
            (
                "grp_sre",
                "Core Infrastructure SRE",
                "24x7 Production Reliability",
            ),
            (
                "grp_net",
                "Global Network Engineering",
                "Backbone & Edge BGP/VPN",
            ),
            ("grp_sec", "Security Incident Response", "SOC Tier 3"),
            (
                "grp_db",
                "Distributed Storage & DB",
                "Postgres / ClickHouse / Redis",
            ),
            (
                "grp_iam",
                "Identity & Access Platform",
                "SSO / OAuth / Okta",
            ),
        ];
        let mut grp_map = IndexMap::new();
        for (id, name, desc) in groups {
            let mut f = IndexMap::new();
            f.insert("sys_id".into(), FieldValue::scalar(id));
            f.insert("name".into(), FieldValue::scalar(name));
            f.insert("description".into(), FieldValue::scalar(desc));
            f.insert("active".into(), FieldValue::new("true", "true"));
            f.insert(
                "sys_updated_on".into(),
                FieldValue::scalar("2026-11-11 08:00:00"),
            );
            f.insert("sys_mod_count".into(), FieldValue::scalar("1"));
            grp_map.insert(
                id.to_string(),
                SnRecord::from_field_map("sys_user_group", f, false),
            );
        }
        tables.insert("sys_user_group".into(), grp_map);

        // Seed `cmdb_ci`
        let cis = [
            ("ci_k8s_usw2", "prod-eks-us-west-2", "Kubernetes Cluster"),
            ("ci_pg_prim", "aurora-orders-primary", "PostgreSQL Cluster"),
            ("ci_edge_gw", "envoy-edge-iad-01", "API Gateway"),
            (
                "ci_sso_idp",
                "auth-gateway-global",
                "OAuth Identity Provider",
            ),
            (
                "ci_vpn_core",
                "tailscale-subnet-router-02",
                "Zero-Trust Relay",
            ),
        ];
        let mut ci_map = IndexMap::new();
        for (id, name, cls) in cis {
            let mut f = IndexMap::new();
            f.insert("sys_id".into(), FieldValue::scalar(id));
            f.insert("name".into(), FieldValue::scalar(name));
            f.insert("sys_class_name".into(), FieldValue::scalar(cls));
            f.insert(
                "operational_status".into(),
                FieldValue::new("1", "Operational"),
            );
            f.insert(
                "sys_updated_on".into(),
                FieldValue::scalar("2026-11-11 08:00:00"),
            );
            f.insert("sys_mod_count".into(), FieldValue::scalar("1"));
            ci_map.insert(
                id.to_string(),
                SnRecord::from_field_map("cmdb_ci", f, false),
            );
        }
        tables.insert("cmdb_ci".into(), ci_map);

        // Seed 180 realistic `incident` records so virtual scrolling, filtering, and sorting shine!
        let templates = [
            (
                "Elevated p99 TLS handshake latency on envoy-edge-iad-01",
                "Edge PoP telemetry indicates TLS handshake latency spiked from 14ms to 190ms following certificate rotation.",
                ("1", "1 - Critical"),
                ("2", "In Progress"),
                "network",
                ("usr_abel", "Abel Tuter"),
                ("grp_net", "Global Network Engineering"),
                ("ci_edge_gw", "envoy-edge-iad-01"),
            ),
            (
                "Read replica replication lag exceeding 12s on aurora-orders-primary",
                "Write burst from checkout settlement batch saturated WAL replay on read-replica-03.",
                ("1", "1 - Critical"),
                ("2", "In Progress"),
                "database",
                ("usr_marcus", "Marcus Chen"),
                ("grp_db", "Distributed Storage & DB"),
                ("ci_pg_prim", "aurora-orders-primary"),
            ),
            (
                "OAuth PKCE token refresh intermittent 502 in EU-Central",
                "Subset of desktop clients receiving upstream connection reset during /oauth_token.do refresh.",
                ("2", "2 - High"),
                ("1", "New"),
                "software",
                ("usr_diana", "Diana Prince"),
                ("grp_iam", "Identity & Access Platform"),
                ("ci_sso_idp", "auth-gateway-global"),
            ),
            (
                "Pod autoscaler hitting max node group quota in prod-eks-us-west-2",
                "HPA requested 18 additional c7g.2xlarge nodes during morning traffic ramp; ASG capped at 120.",
                ("2", "2 - High"),
                ("2", "In Progress"),
                "cloud",
                ("usr_beth", "Beth Anglin"),
                ("grp_sre", "Core Infrastructure SRE"),
                ("ci_k8s_usw2", "prod-eks-us-west-2"),
            ),
            (
                "macOS VPN split-tunnel DNS resolution delay for internal *.corp zones",
                "Engineers on macOS 27 report 5-second fallback timeout when resolving internal staging endpoints.",
                ("3", "3 - Moderate"),
                ("3", "On Hold"),
                "network",
                ("usr_fred", "Fred Luddy"),
                ("grp_net", "Global Network Engineering"),
                ("ci_vpn_core", "tailscale-subnet-router-02"),
            ),
            (
                "Scheduled certificate renewal verification for payment webhook listeners",
                "Confirm mTLS trust bundle includes new intermediate CA prior to cutover window.",
                ("4", "4 - Low"),
                ("6", "Resolved"),
                "security",
                ("usr_diana", "Diana Prince"),
                ("grp_sec", "Security Incident Response"),
                ("ci_edge_gw", "envoy-edge-iad-01"),
            ),
        ];

        let mut inc_map = IndexMap::new();
        for idx in 1..=180usize {
            let tpl = &templates[(idx - 1) % templates.len()];
            let sys_id = format!("inc_demo_{idx:04}");
            let number = format!("INC{:07}", 10000 + idx);
            let active = tpl.3.0 != "6" && tpl.3.0 != "7";
            let updated = format!(
                "2026-11-{:02} {:02}:{:02}:00",
                1 + (idx % 10),
                8 + (idx % 12),
                idx % 60
            );

            let mut f = IndexMap::new();
            f.insert("sys_id".into(), FieldValue::scalar(&sys_id));
            f.insert("number".into(), FieldValue::scalar(&number));
            f.insert(
                "short_description".into(),
                FieldValue::scalar(if idx <= templates.len() {
                    tpl.0.to_string()
                } else {
                    format!("{} [shard-{}]", tpl.0, idx)
                }),
            );
            f.insert("description".into(), FieldValue::scalar(tpl.1));
            f.insert("priority".into(), FieldValue::new(tpl.2.0, tpl.2.1));
            f.insert("state".into(), FieldValue::new(tpl.3.0, tpl.3.1));
            f.insert("impact".into(), FieldValue::new(tpl.2.0, tpl.2.1));
            f.insert("urgency".into(), FieldValue::new("2", "2 - Medium"));
            f.insert("category".into(), FieldValue::new(tpl.4, tpl.4));
            f.insert(
                "caller_id".into(),
                FieldValue::new("usr_fred", "Fred Luddy"),
            );
            f.insert("assigned_to".into(), FieldValue::new(tpl.5.0, tpl.5.1));
            f.insert("assignment_group".into(), FieldValue::new(tpl.6.0, tpl.6.1));
            f.insert("cmdb_ci".into(), FieldValue::new(tpl.7.0, tpl.7.1));
            f.insert(
                "active".into(),
                FieldValue::new(
                    if active { "true" } else { "false" },
                    if active { "true" } else { "false" },
                ),
            );
            f.insert("sys_updated_on".into(), FieldValue::scalar(&updated));
            f.insert("sys_updated_by".into(), FieldValue::scalar("beth.anglin"));
            f.insert(
                "sys_created_on".into(),
                FieldValue::scalar("2026-11-01 09:00:00"),
            );
            f.insert("sys_created_by".into(), FieldValue::scalar("system"));
            f.insert("sys_mod_count".into(), FieldValue::scalar("1"));

            inc_map.insert(sys_id, SnRecord::from_field_map("incident", f, false));
        }
        tables.insert("incident".into(), inc_map);

        // Seed `change_request`
        let mut chg_map = IndexMap::new();
        for idx in 1..=25usize {
            let sys_id = format!("chg_demo_{idx:04}");
            let number = format!("CHG{:07}", 30000 + idx);
            let mut f = IndexMap::new();
            f.insert("sys_id".into(), FieldValue::scalar(&sys_id));
            f.insert("number".into(), FieldValue::scalar(&number));
            f.insert(
                "short_description".into(),
                FieldValue::scalar(format!("Upgrade Envoy proxy fleet to v1.34 (Wave {idx})")),
            );
            f.insert(
                "description".into(),
                FieldValue::scalar("Rolling canary deployment across US-West and EU-Central edge PoPs with automated rollback triggers."),
            );
            f.insert("priority".into(), FieldValue::new("2", "2 - High"));
            f.insert("state".into(), FieldValue::new("2", "Scheduled"));
            f.insert(
                "assigned_to".into(),
                FieldValue::new("usr_beth", "Beth Anglin"),
            );
            f.insert(
                "assignment_group".into(),
                FieldValue::new("grp_sre", "Core Infrastructure SRE"),
            );
            f.insert(
                "cmdb_ci".into(),
                FieldValue::new("ci_edge_gw", "envoy-edge-iad-01"),
            );
            f.insert("active".into(), FieldValue::new("true", "true"));
            f.insert(
                "sys_updated_on".into(),
                FieldValue::scalar("2026-11-11 09:15:00"),
            );
            f.insert("sys_mod_count".into(), FieldValue::scalar("1"));
            chg_map.insert(sys_id, SnRecord::from_field_map("change_request", f, false));
        }
        tables.insert("change_request".into(), chg_map);

        let journals = vec![
            JournalEntry {
                sys_id: "demo_jrn_1".into(),
                element: "work_notes".into(),
                element_id: "inc_demo_0001".into(),
                value: "Captured flamegraph on envoy-edge-iad-01; OCSP stapling cache miss rate jumped after leaf rotation.".into(),
                sys_created_on: "2026-11-11 09:12:00".into(),
                sys_created_by: "abel.tuter".into(),
            },
            JournalEntry {
                sys_id: "demo_jrn_2".into(),
                element: "comments".into(),
                element_id: "inc_demo_0001".into(),
                value: "Traffic has been shifted 50% to secondary IAD ingress while OCSP responder warm-up completes.".into(),
                sys_created_on: "2026-11-11 09:24:00".into(),
                sys_created_by: "beth.anglin".into(),
            },
        ];

        Self {
            tables,
            journals,
            next_number_seq: 10200,
            next_journal_seq: 10,
        }
    }
}

async fn handle_http_connection(
    mut stream: TcpStream,
    state: Arc<RwLock<DemoDataStore>>,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req_str = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req_str.lines();
    let request_line = lines.next().unwrap_or("");
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let raw_target = parts[1];

    // Extract body if present
    let body_str = req_str.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");

    let parsed_url = Url::parse(&format!("http://localhost{raw_target}"))?;
    let path = parsed_url.path();
    let query_pairs: HashMap<String, String> = parsed_url.query_pairs().into_owned().collect();

    if path == "/amb" {
        // Demo server uses pure REST delta-polling at 1.5s so `/amb` returns 401 cleanly.
        return write_json_response(&mut stream, 401, &[], &json!({"error": "OAuth REST mode"}))
            .await;
    }

    if path == "/oauth_token.do" {
        return write_json_response(
            &mut stream,
            200,
            &[],
            &json!({
                "access_token": "demo_sandbox_token",
                "refresh_token": "demo_sandbox_refresh",
                "token_type": "Bearer",
                "expires_in": 86400
            }),
        )
        .await;
    }

    // Metadata endpoints
    if path == "/api/now/table/sys_db_object" {
        let result = json!({
            "result": [
                { "sys_id": fv("tbl_task"), "name": fv("task"), "label": fv("Task"), "super_class": fv(""), "is_extendable": fv("true") },
                { "sys_id": fv("tbl_inc"), "name": fv("incident"), "label": fv("Incident"), "super_class": { "value": "tbl_task", "display_value": "Task" }, "is_extendable": fv("true") },
                { "sys_id": fv("tbl_chg"), "name": fv("change_request"), "label": fv("Change Request"), "super_class": { "value": "tbl_task", "display_value": "Task" }, "is_extendable": fv("true") },
                { "sys_id": fv("tbl_usr"), "name": fv("sys_user"), "label": fv("User"), "super_class": fv(""), "is_extendable": fv("false") },
                { "sys_id": fv("tbl_grp"), "name": fv("sys_user_group"), "label": fv("Group"), "super_class": fv(""), "is_extendable": fv("false") },
                { "sys_id": fv("tbl_ci"), "name": fv("cmdb_ci"), "label": fv("Configuration Item"), "super_class": fv(""), "is_extendable": fv("true") }
            ]
        });
        return write_json_response(&mut stream, 200, &[], &result).await;
    }

    if path == "/api/now/table/sys_number" {
        let result = json!({
            "result": [
                { "category": fv("incident"), "prefix": fv("INC") },
                { "category": fv("change_request"), "prefix": fv("CHG") },
                { "category": fv("task"), "prefix": fv("TASK") }
            ]
        });
        return write_json_response(&mut stream, 200, &[], &result).await;
    }

    if path == "/api/now/table/sys_dictionary" {
        let result = json!({ "result": demo_dictionary_entries() });
        return write_json_response(&mut stream, 200, &[], &result).await;
    }

    if path == "/api/now/table/sys_choice" {
        let result = json!({ "result": demo_choice_entries() });
        return write_json_response(&mut stream, 200, &[], &result).await;
    }

    if path == "/api/now/table/sys_ui_list" || path == "/api/now/table/sys_ui_list_element" {
        return write_json_response(&mut stream, 200, &[], &json!({ "result": [] })).await;
    }

    if path == "/api/now/table/sys_journal_field" {
        let sysparm_query = query_pairs
            .get("sysparm_query")
            .cloned()
            .unwrap_or_default();
        let items: Vec<Value> = {
            let store = state.read();
            let mut v: Vec<Value> = store
                .journals
                .iter()
                .filter(|j| matches_journal_query(j, &sysparm_query))
                .map(|j| {
                    json!({
                        "sys_id": fv(&j.sys_id),
                        "element": fv(&j.element),
                        "element_id": fv(&j.element_id),
                        "value": fv(&j.value),
                        "sys_created_on": fv(&j.sys_created_on),
                        "sys_created_by": fv(&j.sys_created_by),
                    })
                })
                .collect();
            v.reverse();
            v
        };
        return write_json_response(&mut stream, 200, &[], &json!({ "result": items })).await;
    }

    // Generic Table API: `/api/now/table/{table}` or `/api/now/table/{table}/{sys_id}`
    if let Some(rest) = path.strip_prefix("/api/now/table/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() == 1 {
            let table = parts[0];
            if method == "GET" {
                let sysparm_query = query_pairs
                    .get("sysparm_query")
                    .cloned()
                    .unwrap_or_default();
                let limit = query_pairs
                    .get("sysparm_limit")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(100);
                let offset = query_pairs
                    .get("sysparm_offset")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);

                let (total, page) = {
                    let store = state.read();
                    let mut rows: Vec<&SnRecord> = store
                        .tables
                        .get(table)
                        .map(|m| {
                            m.values()
                                .filter(|r| matches_sn_query(r, &sysparm_query))
                                .collect()
                        })
                        .unwrap_or_default();

                    apply_sn_sort(&mut rows, &sysparm_query);
                    let total = rows.len();
                    let page: Vec<Value> = rows
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(record_to_sn_json)
                        .collect();
                    (total, page)
                };

                let headers = [("X-Total-Count", total.to_string())];
                return write_json_response(&mut stream, 200, &headers, &json!({ "result": page }))
                    .await;
            } else if method == "POST" {
                let payload: serde_json::Map<String, Value> =
                    serde_json::from_str(body_str).unwrap_or_default();
                let created = create_or_patch_demo_record(&state, table, None, &payload);
                return write_json_response(
                    &mut stream,
                    201,
                    &[],
                    &json!({ "result": record_to_sn_json(&created) }),
                )
                .await;
            }
        } else if parts.len() == 2 {
            let table = parts[0];
            let sys_id = parts[1];
            if method == "GET" {
                let maybe_json = {
                    let store = state.read();
                    store
                        .tables
                        .get(table)
                        .and_then(|m| m.get(sys_id))
                        .map(record_to_sn_json)
                };
                if let Some(rec_json) = maybe_json {
                    return write_json_response(
                        &mut stream,
                        200,
                        &[],
                        &json!({ "result": rec_json }),
                    )
                    .await;
                }
                return write_json_response(
                    &mut stream,
                    404,
                    &[],
                    &json!({ "error": { "message": "Record not found" } }),
                )
                .await;
            } else if method == "PATCH" || method == "PUT" {
                let payload: serde_json::Map<String, Value> =
                    serde_json::from_str(body_str).unwrap_or_default();
                let updated = create_or_patch_demo_record(&state, table, Some(sys_id), &payload);
                return write_json_response(
                    &mut stream,
                    200,
                    &[],
                    &json!({ "result": record_to_sn_json(&updated) }),
                )
                .await;
            }
        }
    }

    write_json_response(&mut stream, 404, &[], &json!({ "error": "Not found" })).await
}

fn fv(s: &str) -> Value {
    json!({ "value": s, "display_value": s })
}

fn record_to_sn_json(rec: &SnRecord) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in &rec.fields {
        map.insert(
            k.clone(),
            json!({
                "value": v.value,
                "display_value": v.display_value
            }),
        );
    }
    Value::Object(map)
}

fn create_or_patch_demo_record(
    state: &Arc<RwLock<DemoDataStore>>,
    table: &str,
    existing_sys_id: Option<&str>,
    payload: &serde_json::Map<String, Value>,
) -> SnRecord {
    let mut store = state.write();
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let sys_id = match existing_sys_id {
        Some(id) => id.to_string(),
        None => {
            let seq = store.next_number_seq;
            store.next_number_seq += 1;
            format!("{table}_demo_{seq}")
        }
    };

    let mut base_rec = store
        .tables
        .get(table)
        .and_then(|m| m.get(&sys_id))
        .cloned()
        .unwrap_or_else(|| {
            let seq = store.next_number_seq;
            let mut f = IndexMap::new();
            f.insert("sys_id".into(), FieldValue::scalar(&sys_id));
            let prefix = if table == "change_request" {
                "CHG"
            } else {
                "INC"
            };
            f.insert(
                "number".into(),
                FieldValue::scalar(format!("{prefix}{seq:07}")),
            );
            f.insert("state".into(), FieldValue::new("1", "New"));
            f.insert("priority".into(), FieldValue::new("3", "3 - Moderate"));
            f.insert("active".into(), FieldValue::new("true", "true"));
            f.insert("sys_created_on".into(), FieldValue::scalar(&now_str));
            f.insert("sys_created_by".into(), FieldValue::scalar("you"));
            SnRecord::from_field_map(table, f, false)
        });

    // Resolve human-readable display values for choices and references!
    let choice_lookup = demo_choice_display_map();
    for (k, raw_v) in payload {
        let val_str = raw_v.as_str().unwrap_or_default().to_string();
        if k == "work_notes" || k == "comments" {
            if !val_str.trim().is_empty() {
                let j_seq = store.next_journal_seq;
                store.next_journal_seq += 1;
                store.journals.push(JournalEntry {
                    sys_id: format!("demo_jrn_{j_seq}"),
                    element: k.clone(),
                    element_id: sys_id.clone(),
                    value: val_str,
                    sys_created_on: now_str.clone(),
                    sys_created_by: "you (snrust)".into(),
                });
            }
            continue;
        }

        let disp = if let Some(c_label) = choice_lookup.get(&(k.as_str(), val_str.as_str())) {
            (*c_label).to_string()
        } else if matches!(k.as_str(), "assigned_to" | "caller_id") {
            store
                .tables
                .get("sys_user")
                .and_then(|m| m.get(&val_str))
                .map(|r| r.get_display("name").to_string())
                .unwrap_or_else(|| val_str.clone())
        } else if k == "assignment_group" {
            store
                .tables
                .get("sys_user_group")
                .and_then(|m| m.get(&val_str))
                .map(|r| r.get_display("name").to_string())
                .unwrap_or_else(|| val_str.clone())
        } else if k == "cmdb_ci" {
            store
                .tables
                .get("cmdb_ci")
                .and_then(|m| m.get(&val_str))
                .map(|r| r.get_display("name").to_string())
                .unwrap_or_else(|| val_str.clone())
        } else {
            val_str.clone()
        };

        base_rec
            .fields
            .insert(k.clone(), FieldValue::new(val_str, disp));
    }

    base_rec.sys_mod_count += 1;
    base_rec.sys_updated_on = Some(now_str.clone());
    base_rec.fields.insert(
        "sys_mod_count".into(),
        FieldValue::scalar(base_rec.sys_mod_count.to_string()),
    );
    base_rec
        .fields
        .insert("sys_updated_on".into(), FieldValue::scalar(now_str));
    base_rec
        .fields
        .insert("sys_updated_by".into(), FieldValue::scalar("you"));

    store
        .tables
        .entry(table.to_string())
        .or_default()
        .insert(sys_id, base_rec.clone());

    base_rec
}

fn matches_journal_query(j: &JournalEntry, query: &str) -> bool {
    for seg in query.split('^') {
        if let Some(id) = seg.strip_prefix("element_id=")
            && j.element_id != id
        {
            return false;
        }
        if let Some(after) = seg.strip_prefix("sys_created_on>")
            && j.sys_created_on.as_str() <= after
        {
            return false;
        }
    }
    true
}

fn matches_sn_query(rec: &SnRecord, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    for seg in query.split('^') {
        if seg.is_empty() || seg == "EQ" || seg.starts_with("ORDERBY") || seg.starts_with("OR") {
            continue;
        }
        if let Some((f, v)) = seg.split_once("!=") {
            if rec.get_raw(f) == v {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once("<=") {
            if rec.get_raw(f) > v {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once(">=") {
            if rec.get_raw(f) < v {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once("LIKE") {
            let needle = v.to_ascii_lowercase();
            let hay_raw = rec.get_raw(f).to_ascii_lowercase();
            let hay_disp = rec.get_display(f).to_ascii_lowercase();
            if !hay_raw.contains(&needle) && !hay_disp.contains(&needle) {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once("STARTSWITH") {
            let needle = v.to_ascii_lowercase();
            if !rec.get_display(f).to_ascii_lowercase().starts_with(&needle)
                && !rec.get_raw(f).to_ascii_lowercase().starts_with(&needle)
            {
                return false;
            }
        } else if let Some(f) = seg.strip_suffix("ISEMPTY") {
            if !rec.get_raw(f).is_empty() {
                return false;
            }
        } else if let Some(f) = seg.strip_suffix("ISNOTEMPTY") {
            if rec.get_raw(f).is_empty() {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once('>') {
            if rec.get_raw(f) <= v {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once('<') {
            if rec.get_raw(f) >= v {
                return false;
            }
        } else if let Some((f, v)) = seg.split_once('=')
            && rec.get_raw(f) != v
            && !rec.get_display(f).eq_ignore_ascii_case(v)
        {
            return false;
        }
    }
    true
}

fn apply_sn_sort(rows: &mut Vec<&SnRecord>, query: &str) {
    for seg in query.split('^') {
        if let Some(field) = seg.strip_prefix("ORDERBYDESC") {
            rows.sort_by(|a, b| b.get_display(field).cmp(a.get_display(field)));
            return;
        }
        if let Some(field) = seg.strip_prefix("ORDERBY") {
            rows.sort_by(|a, b| a.get_display(field).cmp(b.get_display(field)));
            return;
        }
    }
    rows.sort_by_key(|b| std::cmp::Reverse(b.primary_identifier()));
}

async fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    extra_headers: &[(&str, String)],
    body: &Value,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(body)?;
    let reason = match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    for (k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fn demo_choice_display_map() -> HashMap<(&'static str, &'static str), &'static str> {
    HashMap::from([
        (("state", "1"), "New"),
        (("state", "2"), "In Progress"),
        (("state", "3"), "On Hold"),
        (("state", "6"), "Resolved"),
        (("state", "7"), "Closed"),
        (("priority", "1"), "1 - Critical"),
        (("priority", "2"), "2 - High"),
        (("priority", "3"), "3 - Moderate"),
        (("priority", "4"), "4 - Low"),
        (("impact", "1"), "1 - High"),
        (("impact", "2"), "2 - Medium"),
        (("impact", "3"), "3 - Low"),
        (("urgency", "1"), "1 - High"),
        (("urgency", "2"), "2 - Medium"),
        (("urgency", "3"), "3 - Low"),
        (("category", "network"), "Network"),
        (("category", "database"), "Database"),
        (("category", "software"), "Software"),
        (("category", "cloud"), "Cloud Infrastructure"),
        (("category", "security"), "Security"),
    ])
}

#[allow(clippy::too_many_arguments)]
fn dict_row(
    table: &str,
    element: &str,
    label: &str,
    internal_type: &str,
    max_len: &str,
    mandatory: bool,
    read_only: bool,
    reference: &str,
    display: bool,
) -> Value {
    json!({
        "name": fv(table),
        "element": fv(element),
        "column_label": fv(label),
        "internal_type": fv(internal_type),
        "max_length": fv(max_len),
        "mandatory": fv(if mandatory { "true" } else { "false" }),
        "read_only": fv(if read_only { "true" } else { "false" }),
        "active": fv("true"),
        "display": fv(if display { "true" } else { "false" }),
        "reference": { "value": reference, "display_value": reference }
    })
}

fn demo_dictionary_entries() -> Vec<Value> {
    vec![
        dict_row(
            "task", "number", "Number", "string", "40", false, true, "", true,
        ),
        dict_row(
            "task", "state", "State", "integer", "40", false, false, "", false,
        ),
        dict_row(
            "task", "priority", "Priority", "integer", "40", false, false, "", false,
        ),
        dict_row(
            "task", "impact", "Impact", "integer", "40", false, false, "", false,
        ),
        dict_row(
            "task", "urgency", "Urgency", "integer", "40", false, false, "", false,
        ),
        dict_row(
            "incident",
            "caller_id",
            "Caller",
            "reference",
            "32",
            true,
            false,
            "sys_user",
            false,
        ),
        dict_row(
            "incident", "category", "Category", "string", "40", false, false, "", false,
        ),
        dict_row(
            "task",
            "assignment_group",
            "Assignment group",
            "reference",
            "32",
            false,
            false,
            "sys_user_group",
            false,
        ),
        dict_row(
            "task",
            "assigned_to",
            "Assigned to",
            "reference",
            "32",
            false,
            false,
            "sys_user",
            false,
        ),
        dict_row(
            "task",
            "cmdb_ci",
            "Configuration item",
            "reference",
            "32",
            false,
            false,
            "cmdb_ci",
            false,
        ),
        dict_row(
            "task", "active", "Active", "boolean", "40", false, false, "", false,
        ),
        dict_row(
            "task",
            "short_description",
            "Short description",
            "string",
            "160",
            true,
            false,
            "",
            false,
        ),
        dict_row(
            "task",
            "description",
            "Description",
            "string",
            "4000",
            false,
            false,
            "",
            false,
        ),
        dict_row(
            "task",
            "work_notes",
            "Work notes",
            "journal_input",
            "4000",
            false,
            false,
            "",
            false,
        ),
        dict_row(
            "task",
            "comments",
            "Additional comments",
            "journal_input",
            "4000",
            false,
            false,
            "",
            false,
        ),
        dict_row(
            "task",
            "sys_updated_on",
            "Updated",
            "glide_date_time",
            "40",
            false,
            true,
            "",
            false,
        ),
        dict_row(
            "task",
            "sys_updated_by",
            "Updated by",
            "string",
            "40",
            false,
            true,
            "",
            false,
        ),
        dict_row(
            "task",
            "sys_created_on",
            "Created",
            "glide_date_time",
            "40",
            false,
            true,
            "",
            false,
        ),
        dict_row(
            "task",
            "sys_mod_count",
            "Updates",
            "integer",
            "40",
            false,
            true,
            "",
            false,
        ),
        dict_row(
            "sys_user", "name", "Name", "string", "100", true, false, "", true,
        ),
        dict_row(
            "sys_user", "email", "Email", "email", "120", false, false, "", false,
        ),
        dict_row(
            "sys_user",
            "department",
            "Department",
            "string",
            "80",
            false,
            false,
            "",
            false,
        ),
        dict_row(
            "sys_user", "active", "Active", "boolean", "40", false, false, "", false,
        ),
        dict_row(
            "sys_user_group",
            "name",
            "Name",
            "string",
            "100",
            true,
            false,
            "",
            true,
        ),
        dict_row(
            "sys_user_group",
            "description",
            "Description",
            "string",
            "255",
            false,
            false,
            "",
            false,
        ),
        dict_row(
            "cmdb_ci", "name", "Name", "string", "100", true, false, "", true,
        ),
        dict_row(
            "cmdb_ci",
            "sys_class_name",
            "Class",
            "string",
            "80",
            false,
            true,
            "",
            false,
        ),
    ]
}

fn demo_choice_entries() -> Vec<Value> {
    let rows = [
        ("task", "state", "1", "New", "1"),
        ("task", "state", "2", "In Progress", "2"),
        ("task", "state", "3", "On Hold", "3"),
        ("task", "state", "6", "Resolved", "4"),
        ("task", "state", "7", "Closed", "5"),
        ("task", "priority", "1", "1 - Critical", "1"),
        ("task", "priority", "2", "2 - High", "2"),
        ("task", "priority", "3", "3 - Moderate", "3"),
        ("task", "priority", "4", "4 - Low", "4"),
        ("task", "impact", "1", "1 - High", "1"),
        ("task", "impact", "2", "2 - Medium", "2"),
        ("task", "impact", "3", "3 - Low", "3"),
        ("task", "urgency", "1", "1 - High", "1"),
        ("task", "urgency", "2", "2 - Medium", "2"),
        ("task", "urgency", "3", "3 - Low", "3"),
        ("incident", "category", "network", "Network", "1"),
        ("incident", "category", "database", "Database", "2"),
        ("incident", "category", "software", "Software", "3"),
        ("incident", "category", "cloud", "Cloud Infrastructure", "4"),
        ("incident", "category", "security", "Security", "5"),
    ];
    rows.into_iter()
        .map(|(tbl, elem, val, label, seq)| {
            json!({
                "name": fv(tbl),
                "element": fv(elem),
                "value": fv(val),
                "label": fv(label),
                "sequence": fv(seq),
                "inactive": fv("false")
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sn_api::ListRecordsParams;
    use sn_cache::SnCache;
    use sn_core::{FieldValue, FormGridLayout, FormSessionState};
    use sn_realtime::{RealtimeEvent, WatchTarget};

    use crate::state::AppServices;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_demo_swr_grid_realtime_and_patch_save() {
        let handle = tokio::runtime::Handle::current();
        let cache = SnCache::open_in_memory().unwrap();
        let (services, _cmd_rx) = AppServices::new(handle, cache.clone());

        // 1. Launch Demo Sandbox & verify inherited schema (`task` + `incident`)
        let profile = services.launch_demo_sandbox().await.unwrap();
        let client = services.client_for(&profile).unwrap();
        let schema = services
            .ensure_table_schema(&client, "incident", false)
            .await
            .unwrap();
        assert!(schema.fields.contains_key("number"));
        assert!(schema.fields.contains_key("caller_id"));
        assert!(!schema.choices_for("state", None).is_empty());

        // 2. List incidents -> cache page -> verify instant SWR cache hit
        let page = client
            .list_records(
                "incident",
                &ListRecordsParams {
                    encoded_query: "active=true".into(),
                    fields: schema.default_list_columns.clone(),
                    offset: 0,
                    limit: 50,
                    include_total_count: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.records.len(), 50);
        assert!(page.total_count.unwrap_or(0) >= 150);
        cache.save_list_page(&profile.id, &page).unwrap();

        let swr_page = cache
            .get_list_page(&profile.id, "incident", "active=true", 0, 50)
            .unwrap()
            .expect("cached list page");
        assert!(swr_page.data.from_cache);
        assert_eq!(swr_page.data.records.len(), 50);

        // 3. Customize Auto-Grid layout & verify SQLite persistence
        let mut layout = FormGridLayout::generate_smart_default(&schema);
        layout.move_field_relative("short_description", "number", false);
        layout.toggle_span("caller_id");
        cache.save_form_layout(&profile.id, &layout).unwrap();
        let loaded_layout = cache
            .get_form_layout(&profile.id, "incident")
            .unwrap()
            .expect("saved layout");
        assert!(loaded_layout.is_customized);
        assert_eq!(
            loaded_layout.items[0].field_name(),
            Some("short_description")
        );

        // 4. Open `inc_demo_0001`, dirty `state`, trigger remote live push, verify conflict & clean update!
        let rec = client
            .get_record("incident", "inc_demo_0001")
            .await
            .unwrap();
        let mut session = FormSessionState::from_existing_record(profile.id.clone(), rec.clone());
        session.edit_field("state", FieldValue::new("6", "Resolved"));

        let rx = services.realtime.subscribe_events();
        let _guard = services.realtime.watch(
            client.clone(),
            WatchTarget::Record {
                table: "incident".into(),
                sys_id: "inc_demo_0001".into(),
                last_mod_count: rec.sys_mod_count,
                last_updated_on: rec.sys_updated_on.clone(),
                last_journal_ts: None,
            },
        );

        assert!(services.trigger_demo_live_push("incident", "inc_demo_0001"));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut got_live_update = false;
        while tokio::time::Instant::now() < deadline && !got_live_update {
            if let Ok(Ok(RealtimeEvent::RecordUpdated { record, .. })) =
                tokio::time::timeout(Duration::from_millis(400), rx.recv()).await
            {
                let outcome = session.apply_server_record(&record);
                // Clean field `assigned_to` updated live to Beth Anglin!
                assert_eq!(session.effective_display("assigned_to"), "Beth Anglin");
                assert!(
                    outcome
                        .updated_clean_fields
                        .iter()
                        .any(|p| p.field == "assigned_to")
                );
                // Dirty field `state` preserved user's "Resolved" draft & flagged conflict!
                assert_eq!(session.effective_display("state"), "Resolved");
                assert!(session.conflicts.contains_key("state"));
                got_live_update = true;
            }
        }
        assert!(
            got_live_update,
            "Expected live RecordUpdated from DemoSandboxServer"
        );

        // 5. Explicit PATCH save of dirty fields + work_notes
        session.edit_journal("work_notes", "Confirmed fix in snrust integration test");
        let payload = session.build_save_payload();
        let saved = client
            .patch_record("incident", "inc_demo_0001", &payload)
            .await
            .unwrap();
        session.commit_saved_record(saved);
        assert!(!session.is_dirty());
        assert_eq!(session.effective_display("state"), "Resolved");
    }
}
