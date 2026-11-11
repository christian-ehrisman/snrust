use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use indexmap::IndexMap;
use sn_core::{
    ChoiceOption, ColSpan, DictionaryEntry, FieldType, FieldValue, FormGridLayout, InstanceId,
    InstanceProfile, JournalEntry, ListPage, ReferenceCandidate, SnRecord, TableDescriptor,
    TableSchema,
};

use crate::{CacheTtlConfig, FavoriteItem, FavoriteKind, RecentItem, SnCache};

fn assert_send_sync_clone<T: Send + Sync + Clone>() {}

fn sample_incident_schema() -> TableSchema {
    let mut entries_by_table = HashMap::new();
    entries_by_table.insert(
        "task".to_string(),
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
            DictionaryEntry {
                element: "description".into(),
                column_label: "Description".into(),
                defined_on_table: "task".into(),
                internal_type: "string".into(),
                field_type: FieldType::MultiLineText,
                max_length: Some(4000),
                mandatory: false,
                read_only: false,
                active: true,
                display: false,
                default_value: None,
                dependent_on: None,
            },
            DictionaryEntry {
                element: "state".into(),
                column_label: "State".into(),
                defined_on_table: "task".into(),
                internal_type: "integer".into(),
                field_type: FieldType::Choice,
                max_length: None,
                mandatory: false,
                read_only: false,
                active: true,
                display: false,
                default_value: Some("1".into()),
                dependent_on: None,
            },
            DictionaryEntry {
                element: "work_notes".into(),
                column_label: "Work notes".into(),
                defined_on_table: "task".into(),
                internal_type: "journal_input".into(),
                field_type: FieldType::JournalInput,
                max_length: None,
                mandatory: false,
                read_only: false,
                active: true,
                display: false,
                default_value: None,
                dependent_on: None,
            },
        ],
    );
    entries_by_table.insert(
        "incident".to_string(),
        vec![DictionaryEntry {
            element: "caller_id".into(),
            column_label: "Caller".into(),
            defined_on_table: "incident".into(),
            internal_type: "reference".into(),
            field_type: FieldType::Reference {
                target_table: "sys_user".into(),
            },
            max_length: Some(32),
            mandatory: true,
            read_only: false,
            active: true,
            display: false,
            default_value: None,
            dependent_on: None,
        }],
    );

    let mut state_choices = HashMap::new();
    state_choices.insert(
        "state".to_string(),
        vec![
            ChoiceOption {
                element: "state".into(),
                value: "1".into(),
                label: "New".into(),
                sequence: 1,
                dependent_value: None,
                inactive: false,
            },
            ChoiceOption {
                element: "state".into(),
                value: "2".into(),
                label: "In Progress".into(),
                sequence: 2,
                dependent_value: None,
                inactive: false,
            },
        ],
    );
    let mut choices_by_table = HashMap::new();
    choices_by_table.insert("incident".to_string(), state_choices);

    TableSchema::assemble(
        "incident",
        "Incident",
        vec!["incident".into(), "task".into()],
        &entries_by_table,
        &choices_by_table,
        None,
    )
}

fn make_record(
    table: &str,
    sys_id: &str,
    pairs: &[(&str, &str, &str)],
    updated_on: &str,
    mod_count: u64,
    is_partial: bool,
) -> SnRecord {
    let mut fields = IndexMap::new();
    fields.insert("sys_id".into(), FieldValue::scalar(sys_id));
    fields.insert("sys_updated_on".into(), FieldValue::scalar(updated_on));
    fields.insert(
        "sys_mod_count".into(),
        FieldValue::scalar(mod_count.to_string()),
    );
    for &(k, raw, disp) in pairs {
        fields.insert(k.to_string(), FieldValue::new(raw, disp));
    }
    SnRecord::from_field_map(table, fields, is_partial)
}

#[test]
fn sn_cache_is_clone_send_sync() {
    assert_send_sync_clone::<SnCache>();
}

#[test]
fn partial_list_to_full_form_to_partial_refresh_preserves_full_fields() {
    let cache = SnCache::open_in_memory().expect("in-memory cache");
    let inst = InstanceId::from("dev-instance");

    let schema = sample_incident_schema();
    cache.save_table_schema(&inst, &schema).unwrap();

    // 1. Hydrate a partial record from a List View query (only 3 columns).
    let list_row_v1 = make_record(
        "incident",
        "inc_sys_001",
        &[
            ("number", "INC0010042", "INC0010042"),
            (
                "short_description",
                "VPN drops intermittently",
                "VPN drops intermittently",
            ),
            ("state", "1", "New"),
        ],
        "2026-11-11 10:00:00",
        1,
        true,
    );
    cache.upsert_record(&inst, &list_row_v1).unwrap();

    let step1 = cache
        .get_record(&inst, "incident", "inc_sys_001")
        .unwrap()
        .expect("cached v1");
    assert!(step1.data.is_partial);
    assert_eq!(step1.data.get_display("description"), "");

    // 2. Open Form View -> full record GET arrives with rich fields.
    let full_form_v2 = make_record(
        "incident",
        "inc_sys_001",
        &[
            ("number", "INC0010042", "INC0010042"),
            (
                "short_description",
                "VPN drops intermittently",
                "VPN drops intermittently",
            ),
            (
                "description",
                "Detailed packet loss trace on corp gateway.",
                "Detailed packet loss trace on corp gateway.",
            ),
            ("caller_id", "user_abel_01", "Abel Tuter"),
            (
                "close_notes",
                "Pending firewall patch",
                "Pending firewall patch",
            ),
            ("state", "1", "New"),
        ],
        "2026-11-11 10:01:00",
        1,
        false,
    );
    cache.upsert_record(&inst, &full_form_v2).unwrap();

    let step2 = cache
        .get_record(&inst, "incident", "inc_sys_001")
        .unwrap()
        .expect("cached v2");
    assert!(!step2.data.is_partial);
    assert_eq!(
        step2.data.get_display("description"),
        "Detailed packet loss trace on corp gateway."
    );

    // 3. Subsequent partial List View refresh updates state + short_description without description.
    let partial_refresh_v3 = make_record(
        "incident",
        "inc_sys_001",
        &[
            ("number", "INC0010042", "INC0010042"),
            (
                "short_description",
                "VPN drops on macOS Sequoia",
                "VPN drops on macOS Sequoia",
            ),
            ("state", "2", "In Progress"),
        ],
        "2026-11-11 10:05:00",
        2,
        true,
    );
    cache.upsert_record(&inst, &partial_refresh_v3).unwrap();

    let step3 = cache
        .get_record(&inst, "incident", "inc_sys_001")
        .unwrap()
        .expect("cached v3");
    assert!(
        !step3.data.is_partial,
        "full-record completeness flag must survive partial list refreshes"
    );
    assert_eq!(step3.data.get_raw("state"), "2");
    assert_eq!(step3.data.get_display("state"), "In Progress");
    assert_eq!(
        step3.data.get_display("short_description"),
        "VPN drops on macOS Sequoia"
    );
    assert_eq!(
        step3.data.get_display("description"),
        "Detailed packet loss trace on corp gateway.",
        "full-form description must NOT be stripped by partial list refresh"
    );
    assert_eq!(step3.data.get_display("caller_id"), "Abel Tuter");
    assert_eq!(
        step3.data.get_display("close_notes"),
        "Pending firewall patch"
    );
    assert_eq!(step3.data.sys_mod_count, 2);

    // Verify cold SQLite path matches hot moka cache by evicting record_cache entry manually.
    cache
        .inner
        .record_cache
        .invalidate(&(inst.clone(), "incident".into(), "inc_sys_001".into()));
    let cold_step3 = cache
        .get_record(&inst, "incident", "inc_sys_001")
        .unwrap()
        .expect("cold sqlite v3");
    assert!(!cold_step3.data.is_partial);
    assert_eq!(
        cold_step3.data.get_display("description"),
        "Detailed packet loss trace on corp gateway."
    );

    // Verify find_record_by_field finds INC0010042 case-insensitively and by reference display name!
    let by_num = cache
        .find_record_by_field(&inst, "incident", "number", "inc0010042")
        .unwrap()
        .expect("found by number");
    assert_eq!(by_num.sys_id, "inc_sys_001");

    let by_caller = cache
        .find_record_by_field(&inst, "incident", "caller_id", "Abel Tuter")
        .unwrap()
        .expect("found by caller display value");
    assert_eq!(by_caller.sys_id, "inc_sys_001");

    // Verify reference display value harvesting indexed "Abel Tuter" under sys_user!
    let harvested_users = cache
        .search_reference_candidates(&inst, "sys_user", "Abel", 10)
        .unwrap();
    assert_eq!(harvested_users.len(), 1);
    assert_eq!(harvested_users[0].sys_id, "user_abel_01");
    assert_eq!(harvested_users[0].display_value, "Abel Tuter");
}

#[test]
fn list_page_swr_caching_reconstruction_and_delta_updates() {
    let cache = SnCache::open_in_memory()
        .unwrap()
        .with_ttl_config(CacheTtlConfig {
            schema_ttl: Duration::from_secs(3600),
            record_ttl: Duration::from_secs(300),
            list_ttl: Duration::from_millis(10),
        });
    let inst = InstanceId::from("prod-instance");

    let r1 = make_record(
        "incident",
        "id_1",
        &[
            ("number", "INC0001", "INC0001"),
            ("short_description", "First issue", "First issue"),
        ],
        "2026-11-11 09:00:00",
        0,
        true,
    );
    let r2 = make_record(
        "incident",
        "id_2",
        &[
            ("number", "INC0002", "INC0002"),
            ("short_description", "Second issue", "Second issue"),
        ],
        "2026-11-11 09:05:00",
        1,
        true,
    );

    let page = ListPage {
        table: "incident".into(),
        encoded_query: "active=true^ORDERBYDESCsys_updated_on".into(),
        offset: 0,
        limit: 50,
        total_count: Some(128),
        records: vec![r2, r1],
        max_updated_on: None,
        from_cache: false,
    };

    cache.save_list_page(&inst, &page).unwrap();

    let loaded = cache
        .get_list_page(
            &inst,
            "incident",
            "active=true^ORDERBYDESCsys_updated_on",
            0,
            50,
        )
        .unwrap()
        .expect("cached list page");

    assert!(loaded.data.from_cache);
    assert_eq!(loaded.data.total_count, Some(128));
    assert_eq!(
        loaded.data.max_updated_on.as_deref(),
        Some("2026-11-11 09:05:00")
    );
    assert_eq!(loaded.data.records.len(), 2);
    // Exact server sort order (id_2 before id_1) must be preserved.
    assert_eq!(loaded.data.records[0].sys_id, "id_2");
    assert_eq!(loaded.data.records[1].sys_id, "id_1");

    // Apply delta update for id_1 and verify reconstructed ListPage reflects new title & watermark.
    let delta_r1 = make_record(
        "incident",
        "id_1",
        &[
            ("number", "INC0001", "INC0001"),
            (
                "short_description",
                "First issue (escalated)",
                "First issue (escalated)",
            ),
        ],
        "2026-11-11 09:20:00",
        3,
        true,
    );
    cache
        .apply_list_delta(&inst, "incident", &[delta_r1])
        .unwrap();

    std::thread::sleep(Duration::from_millis(20));

    let after_delta = cache
        .get_list_page(
            &inst,
            "incident",
            "active=true^ORDERBYDESCsys_updated_on",
            0,
            50,
        )
        .unwrap()
        .expect("reconstructed page after delta");

    assert!(
        after_delta.is_stale,
        "list page should report stale past 10ms TTL"
    );
    assert_eq!(
        after_delta.data.max_updated_on.as_deref(),
        Some("2026-11-11 09:20:00")
    );
    assert_eq!(
        after_delta.data.records[1].get_display("short_description"),
        "First issue (escalated)"
    );
}

#[test]
fn fts5_table_catalog_hierarchy_and_reference_candidate_search() {
    let cache = SnCache::open_in_memory().unwrap();
    let inst = InstanceId::from("fts-instance");

    // Empty catalog still surfaces built-in ITSM defaults when query is empty.
    let cold_defaults = cache.search_tables(&inst, "", 5).unwrap();
    assert_eq!(cold_defaults.len(), 5);
    assert_eq!(cold_defaults[0].name, "incident");

    let catalog = vec![
        TableDescriptor {
            sys_id: "t1".into(),
            name: "task".into(),
            label: "Task".into(),
            super_class: None,
            number_prefix: Some("TASK".into()),
            is_extendable: true,
        },
        TableDescriptor {
            sys_id: "t2".into(),
            name: "incident".into(),
            label: "Incident".into(),
            super_class: Some("task".into()),
            number_prefix: Some("INC".into()),
            is_extendable: true,
        },
        TableDescriptor {
            sys_id: "t3".into(),
            name: "sc_req_item".into(),
            label: "Requested Item".into(),
            super_class: Some("task".into()),
            number_prefix: Some("RITM".into()),
            is_extendable: true,
        },
        TableDescriptor {
            sys_id: "t4".into(),
            name: "cmdb_ci_linux_server".into(),
            label: "Linux Server".into(),
            super_class: Some("cmdb_ci".into()),
            number_prefix: None,
            is_extendable: true,
        },
        TableDescriptor {
            sys_id: "t5".into(),
            name: "u_sec_incident".into(),
            label: "Security Incident".into(),
            super_class: Some("incident".into()),
            number_prefix: Some("SIR".into()),
            is_extendable: false,
        },
    ];
    cache.save_table_catalog(&inst, &catalog).unwrap();

    // Hierarchy resolution & custom prefix resolution (SIR009912 -> u_sec_incident).
    let hierarchy = cache.get_table_hierarchy(&inst).unwrap();
    assert_eq!(
        hierarchy.inheritance_chain("u_sec_incident"),
        vec!["u_sec_incident", "incident", "task"]
    );
    assert_eq!(
        hierarchy.resolve_record_number("sir009912"),
        Some(("u_sec_incident".into(), "SIR009912".into()))
    );

    // FTS5 search across snake_case table tokens, human labels, prefixes, and noisy punctuation.
    let req_hits = cache.search_tables(&inst, "sc_req", 5).unwrap();
    assert_eq!(req_hits[0].name, "sc_req_item");

    let linux_hits = cache.search_tables(&inst, "linux serv", 5).unwrap();
    assert_eq!(linux_hits[0].name, "cmdb_ci_linux_server");

    let special_chars = cache
        .search_tables(&inst, "\"incident\"* (sec)", 5)
        .unwrap();
    assert_eq!(special_chars[0].name, "u_sec_incident");

    let warm_defaults = cache.search_tables(&inst, "   ", 10).unwrap();
    assert_eq!(warm_defaults[0].name, "incident");
    assert_eq!(warm_defaults[1].name, "sc_req_item");

    // Reference candidate FTS5 search.
    let candidates = vec![
        ReferenceCandidate {
            target_table: "sys_user".into(),
            sys_id: "u1".into(),
            display_value: "System Administrator".into(),
            secondary: Some("admin@example.com".into()),
        },
        ReferenceCandidate {
            target_table: "sys_user".into(),
            sys_id: "u2".into(),
            display_value: "Beth Anglin".into(),
            secondary: Some("beth.anglin@example.com".into()),
        },
        ReferenceCandidate {
            target_table: "sys_user_group".into(),
            sys_id: "g1".into(),
            display_value: "Network Support".into(),
            secondary: Some("Tier 2 NetOps".into()),
        },
    ];
    cache.save_reference_candidates(&inst, &candidates).unwrap();

    let beth = cache
        .search_reference_candidates(&inst, "sys_user", "anglin", 5)
        .unwrap();
    assert_eq!(beth.len(), 1);
    assert_eq!(beth[0].sys_id, "u2");

    let by_email = cache
        .search_reference_candidates(&inst, "sys_user", "admin@example", 5)
        .unwrap();
    assert_eq!(by_email.len(), 1);
    assert_eq!(by_email[0].sys_id, "u1");
}

#[test]
fn form_grid_layout_and_list_columns_save_load_reset() {
    let cache = SnCache::open_in_memory().unwrap();
    let inst = InstanceId::from("layout-instance");
    let schema = sample_incident_schema();

    assert!(cache.get_form_layout(&inst, "incident").unwrap().is_none());

    let mut layout = FormGridLayout::generate_smart_default(&schema);
    layout.toggle_span("caller_id");
    layout.add_section_divider("Resolution Details", None);
    assert!(layout.is_customized);

    cache.save_form_layout(&inst, &layout).unwrap();

    let loaded = cache
        .get_form_layout(&inst, "incident")
        .unwrap()
        .expect("saved custom layout");
    assert!(loaded.is_customized);
    assert_eq!(loaded.field_span("caller_id"), Some(ColSpan::Two));
    assert_eq!(loaded, layout);

    // Custom list columns persistence.
    let cols = vec![
        "number".to_string(),
        "caller_id".to_string(),
        "state".to_string(),
        "short_description".to_string(),
    ];
    cache.save_list_columns(&inst, "incident", &cols).unwrap();
    assert_eq!(
        cache.get_list_columns(&inst, "incident").unwrap(),
        Some(cols.clone())
    );

    // clear_instance_cache clears Tier 1/2 while keeping user FormGridLayout & list columns!
    cache.save_table_schema(&inst, &schema).unwrap();
    cache.clear_instance_cache(&inst).unwrap();
    assert!(cache.get_table_schema(&inst, "incident").unwrap().is_none());
    assert_eq!(
        cache.get_form_layout(&inst, "incident").unwrap().as_ref(),
        Some(&layout)
    );
    assert_eq!(
        cache.get_list_columns(&inst, "incident").unwrap(),
        Some(cols)
    );

    // Resetting form layout deletes custom override cleanly.
    cache.delete_form_layout(&inst, "incident").unwrap();
    assert!(cache.get_form_layout(&inst, "incident").unwrap().is_none());
}

#[test]
fn instance_profile_crud_workspace_state_journals_and_cache_stats() {
    let db_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".test_db_scratch");
    let db_file = db_dir.join("snrust_test.db");
    let _ = std::fs::remove_dir_all(&db_dir);

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = Cleanup(db_dir.clone());

    let mut dev = InstanceProfile::new("Dev Sandbox", "dev12345", "oauth_client_1", false).unwrap();
    dev.accent_hex = "#34D399".into();
    let prod =
        InstanceProfile::new("Acme Prod", "acme.service-now.com", "oauth_client_2", true).unwrap();

    {
        let cache = SnCache::open_path(&db_file).unwrap();
        cache.upsert_instance(&dev).unwrap();
        cache.upsert_instance(&prod).unwrap();
        cache.set_active_instance_id(Some(&dev.id)).unwrap();

        // Favorites CRUD.
        let fav = FavoriteItem::for_table(
            "fav-my-inc",
            "incident",
            "My Open Incidents",
            "active=true^assigned_toDYNAMIC90d1921e5f510100a9ad2572f2b477fe",
            Some("Assigned to me".into()),
        );
        cache.upsert_favorite(&dev.id, &fav).unwrap();
        assert_eq!(cache.list_favorites(&dev.id).unwrap(), vec![fav]);
        let rec_fav = FavoriteItem {
            id: "fav-rec-1".into(),
            kind: FavoriteKind::Record {
                table: "incident".into(),
                sys_id: "s1".into(),
                title: "INC0010001".into(),
            },
            title: "INC0010001".into(),
            subtitle: Some("Critical outage".into()),
        };
        cache.upsert_favorite(&dev.id, &rec_fav).unwrap();
        cache.remove_favorite(&dev.id, "fav-my-inc").unwrap();
        assert_eq!(cache.list_favorites(&dev.id).unwrap().len(), 1);

        // Recent items cap at 50 + deduplication.
        let base_time = Utc::now() - TimeDelta::minutes(60);
        for i in 0..60 {
            cache
                .record_recent(
                    &dev.id,
                    &RecentItem {
                        table: "incident".into(),
                        sys_id: Some(format!("sys_{i:03}")),
                        title: format!("INC{i:05}"),
                        subtitle: None,
                        visited_at: base_time + TimeDelta::seconds(i),
                    },
                )
                .unwrap();
        }
        let recents = cache.list_recent(&dev.id, 100).unwrap();
        assert_eq!(recents.len(), 50, "recent history must cap at 50 items");
        assert_eq!(recents[0].title, "INC00059");

        // Journals persistence & newest-first ordering.
        cache
            .save_journals(
                &dev.id,
                "incident",
                "sys_059",
                &[
                    JournalEntry {
                        sys_id: "j1".into(),
                        element: "comments".into(),
                        element_id: "sys_059".into(),
                        value: "Initial user report".into(),
                        sys_created_on: "2026-11-11 08:00:00".into(),
                        sys_created_by: "abel.tuter".into(),
                    },
                    JournalEntry {
                        sys_id: "j2".into(),
                        element: "work_notes".into(),
                        element_id: "sys_059".into(),
                        value: "Restarted worker pod".into(),
                        sys_created_on: "2026-11-11 08:15:00".into(),
                        sys_created_by: "admin".into(),
                    },
                ],
            )
            .unwrap();
        let journals = cache.get_journals(&dev.id, "incident", "sys_059").unwrap();
        assert_eq!(journals.len(), 2);
        assert_eq!(journals[0].sys_id, "j2");
        assert!(journals[0].is_work_note());

        // Populate schema + records to verify cache_stats.
        cache
            .save_table_schema(&dev.id, &sample_incident_schema())
            .unwrap();
        cache
            .upsert_record(
                &dev.id,
                &make_record(
                    "incident",
                    "sys_059",
                    &[("number", "INC00059", "INC00059")],
                    "2026-11-11 08:15:00",
                    1,
                    false,
                ),
            )
            .unwrap();
    }

    // Reopen persistent WAL database from disk and verify durability + stats + delete cascade.
    let reopened = SnCache::open_path(&db_file).unwrap();
    assert_eq!(reopened.list_instances().unwrap().len(), 2);
    assert_eq!(
        reopened.get_active_instance_id().unwrap().as_ref(),
        Some(&dev.id)
    );

    let stats = reopened.cache_stats(Some(&dev.id)).unwrap();
    assert_eq!(stats.record_count, 1);
    assert_eq!(stats.schema_count, 1);
    assert!(stats.db_bytes > 0);

    // Deleting dev cascades schemas, records, favorites, recents, and resets active instance id.
    reopened.delete_instance(&dev.id).unwrap();
    assert_eq!(reopened.list_instances().unwrap(), vec![prod]);
    assert!(reopened.get_active_instance_id().unwrap().is_none());
    assert_eq!(reopened.cache_stats(Some(&dev.id)).unwrap().record_count, 0);
    assert!(reopened.list_favorites(&dev.id).unwrap().is_empty());
    assert!(reopened.list_recent(&dev.id, 10).unwrap().is_empty());
}

#[tokio::test]
async fn async_spawn_blocking_helpers_round_trip() {
    let cache = SnCache::open_in_memory().unwrap();
    let inst = InstanceId::from("async-inst");

    let schema = sample_incident_schema();
    cache
        .save_table_schema_async(inst.clone(), schema)
        .await
        .unwrap();

    let fetched_schema = cache
        .get_table_schema_async(inst.clone(), "incident".into())
        .await
        .unwrap()
        .expect("async schema");
    assert_eq!(fetched_schema.data.table, "incident");

    let rec = make_record(
        "incident",
        "async_001",
        &[("number", "INC999001", "INC999001")],
        "2026-11-11 11:00:00",
        0,
        false,
    );
    cache
        .upsert_records_batch_async(inst.clone(), vec![rec])
        .await
        .unwrap();

    let fetched_rec = cache
        .get_record_async(inst, "incident".into(), "async_001".into())
        .await
        .unwrap()
        .expect("async record");
    assert_eq!(fetched_rec.data.primary_identifier(), "INC999001");
}
