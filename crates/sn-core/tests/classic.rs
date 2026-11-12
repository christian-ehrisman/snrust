use std::collections::HashMap;

use sn_core::classic::{group_favorites, group_modules};
use sn_core::{
    AccountIdentity, BookmarkGroupRow, BookmarkRow, ClassicFormSection, ClassicFormView,
    ClassicTarget, DictionaryEntry, EncodedQuery, FieldType, FieldValue, FormSessionState,
    InstanceProfile, ModuleRow, NavNode, NavigationSnapshot, TableSchema,
};

#[test]
fn classic_links_preserve_query_view_and_reject_unsupported_destinations() {
    let origin = "https://example.service-now.com";
    let list = "incident_list.do?sysparm_query=active%3Dtrue%5Epriority%3D1%5EORpriority%3D2&sysparm_view=ess";
    let expected = ClassicTarget::List {
        table: "incident".into(),
        query: "active=true^priority=1^ORpriority=2".into(),
        view: "ess".into(),
    };
    assert_eq!(
        ClassicTarget::from_url(origin, list),
        Some(expected.clone())
    );
    assert_eq!(
        ClassicTarget::from_url(
            origin,
            &format!("nav_to.do?uri={}", urlencoding::encode(list))
        ),
        Some(expected.clone())
    );
    assert_eq!(
        ClassicTarget::from_url(
            origin,
            &format!(
                "/now/nav/ui/classic/params/target/{}",
                urlencoding::encode(list)
            )
        ),
        Some(expected)
    );
    assert!(
        matches!(ClassicTarget::from_url(origin, "incident.do?sys_id=-1&sysparm_view=ess"), Some(ClassicTarget::Form { sys_id, view, .. }) if sys_id == "-1" && view == "ess")
    );
    for unsupported in [
        "/now/workspace/agent/record/incident/123",
        "/sp?id=ticket&table=incident&sys_id=123",
        "https://external.example/incident_list.do",
        "//external.example/incident_list.do",
        "javascript:alert(1)",
        "home.do",
        "sys_report_template.do?jvar_report_id=123",
        "incident.do?sys_id=not-a-record",
    ] {
        assert_eq!(
            ClassicTarget::from_url(origin, unsupported),
            None,
            "{unsupported}"
        );
    }
}

#[test]
fn complex_navigation_queries_do_not_reorder_or_branches() {
    for query in [
        "priority=1^ORpriority=2^active=true",
        "active=true^NQpriority=1^state=2",
        "assigned_to=javascript:gs.getUserID()^active=true^ORDERBYnumber",
        "resolved_atONLast 60 days@javascript:gs.daysAgoStart(60)@javascript:gs.daysAgoEnd(0)^state=6^ORstate=7",
        "nameLIKEone^^two^ORname=other^active=true",
    ] {
        assert_eq!(EncodedQuery::parse(query).to_encoded_string(), query);
    }
}

#[test]
fn accounts_with_the_same_origin_only_match_verified_principals() {
    let mut a = InstanceProfile::new("Development", "example", "client", false).unwrap();
    let mut b = a.clone();
    assert!(!a.is_same_account(&b));
    a.account = Some(AccountIdentity {
        sys_id: "user-a".into(),
        user_name: "a".into(),
        display_name: "A".into(),
    });
    b.account = Some(AccountIdentity {
        sys_id: "user-b".into(),
        user_name: "b".into(),
        display_name: "B".into(),
    });
    assert!(!a.is_same_account(&b));
    b.account = a.account.clone();
    assert!(a.is_same_account(&b));
    b.base_url = "https://other.service-now.com".into();
    assert!(!a.is_same_account(&b));
    assert!(a.display_label().contains("a"));
}

#[test]
fn classic_sections_and_split_columns_preserve_fields_without_mutating_drafts() {
    let mut schema = TableSchema::assemble(
        "incident",
        "Incident",
        vec!["incident".into()],
        &HashMap::new(),
        &HashMap::new(),
        None,
    );
    schema.ensure_system_fields();
    let view = ClassicFormView {
        name: "audit".into(),
        label: "Audit".into(),
        sections: vec![ClassicFormSection {
            id: "section".into(),
            title: "History".into(),
            elements: vec![
                ".start_split",
                "sys_created_by",
                "sys_created_on",
                ".split",
                "sys_updated_by",
                "sys_updated_on",
                ".end_split",
                "unknown_formatter",
                "sys_created_by",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }],
    };
    let mut session = FormSessionState::new_record("account".into(), "incident");
    session.edit_field("short_description", FieldValue::scalar("Keep this draft"));
    let layout = view.layout(&schema);
    let fields = layout
        .items
        .iter()
        .filter_map(|item| item.field_name())
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        [
            "sys_created_by",
            "sys_updated_by",
            "sys_created_on",
            "sys_updated_on"
        ]
    );
    assert!(layout.hidden_fields.contains(&"sys_id".into()));
    assert!(!layout.hidden_fields.contains(&"sys_created_by".into()));
    assert_eq!(
        session.effective_raw("short_description"),
        "Keep this draft"
    );
    assert_eq!(
        sn_core::classic::view_cache_key("incident", "default"),
        "incident"
    );
    assert_ne!(
        sn_core::classic::view_cache_key("incident", "audit"),
        "incident"
    );
}

fn dictionary_field(
    table: &str,
    element: &str,
    label: &str,
    reference: Option<&str>,
) -> DictionaryEntry {
    DictionaryEntry {
        element: element.into(),
        column_label: label.into(),
        defined_on_table: table.into(),
        internal_type: if reference.is_some() {
            "reference"
        } else {
            "string"
        }
        .into(),
        field_type: match reference {
            Some(target) => FieldType::Reference {
                target_table: target.into(),
            },
            None => FieldType::String,
        },
        max_length: Some(40),
        mandatory: false,
        read_only: false,
        active: true,
        display: false,
        default_value: None,
        dependent_on: None,
    }
}

fn schema_with(table: &str, fields: Vec<DictionaryEntry>) -> TableSchema {
    let entries = HashMap::from([(table.to_string(), fields)]);
    TableSchema::assemble(
        table,
        table,
        vec![table.to_string()],
        &entries,
        &HashMap::new(),
        None,
    )
}

#[test]
fn dot_walked_columns_are_labelled_through_each_reference() {
    let incident = schema_with(
        "incident",
        vec![
            dictionary_field("incident", "caller_id", "Caller", Some("sys_user")),
            dictionary_field("incident", "short_description", "Short description", None),
        ],
    );
    let user = schema_with(
        "sys_user",
        vec![
            dictionary_field("sys_user", "email", "Email", None),
            dictionary_field("sys_user", "manager", "Manager", Some("sys_user")),
        ],
    );
    let related = |table: &str| (table == "sys_user").then(|| user.clone());

    assert_eq!(
        incident
            .dot_walk_label("short_description", related)
            .as_deref(),
        Some("Short description")
    );
    assert_eq!(
        incident
            .dot_walk_label("caller_id.email", related)
            .as_deref(),
        Some("Caller › Email")
    );
    assert_eq!(
        incident
            .dot_walk_label("caller_id.manager.email", related)
            .as_deref(),
        Some("Caller › Manager › Email")
    );
    // Unknown segments, walking through a non-reference, and uncached targets all decline.
    assert_eq!(incident.dot_walk_label("caller_id.nope", related), None);
    assert_eq!(
        incident.dot_walk_label("short_description.email", related),
        None
    );
    assert_eq!(incident.dot_walk_label("caller_id.email", |_| None), None);
}

fn list_target(table: &str, query: &str) -> ClassicTarget {
    ClassicTarget::List {
        table: table.into(),
        query: query.into(),
        view: String::new(),
    }
}

fn module(id: &str, title: &str, target: Option<ClassicTarget>) -> ModuleRow {
    ModuleRow {
        id: id.into(),
        title: title.into(),
        is_separator: false,
        target,
    }
}

fn separator(id: &str, title: &str) -> ModuleRow {
    ModuleRow {
        id: id.into(),
        title: title.into(),
        is_separator: true,
        target: None,
    }
}

fn outline(nodes: &[NavNode]) -> Vec<String> {
    fn walk(nodes: &[NavNode], depth: usize, out: &mut Vec<String>) {
        for node in nodes {
            out.push(format!("{}{}", "  ".repeat(depth), node.title));
            walk(&node.children, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, &mut out);
    out
}

#[test]
fn titled_separators_nest_the_modules_that_follow_them() {
    let all = Some(list_target("incident", ""));
    let nodes = group_modules(vec![
        module("m1", "Create New", all.clone()),
        separator("s1", "Open work"),
        module("m2", "Assigned to me", all.clone()),
        module("m3", "Workspace only", None),
        module("m4", "Unassigned", all.clone()),
        separator("s2", ""),
        module("m5", "All", all.clone()),
        separator("s3", "Reports"),
        module("m6", "Dashboard", None),
        separator("s4", "Admin"),
        module("m7", "Properties", all),
    ]);
    assert_eq!(
        outline(&nodes),
        [
            "Create New",
            "Open work",
            "  Assigned to me",
            "  Unassigned",
            // The untitled separator ended "Open work", so this sits at the top level.
            "All",
            // "Reports" held only an unsupported module and is omitted.
            "Admin",
            "  Properties",
        ]
    );
}

#[test]
fn favorites_nest_in_their_groups_and_share_one_top_level_order() {
    let bookmark = |id: &str, title: &str, order: i64, group: Option<&str>| BookmarkRow {
        id: id.into(),
        title: title.into(),
        order,
        group_id: group.map(str::to_string),
        target: list_target("incident", id),
    };
    let nodes = group_favorites(
        vec![
            BookmarkGroupRow {
                id: "g1".into(),
                title: "Queues".into(),
                order: 20,
            },
            BookmarkGroupRow {
                id: "g2".into(),
                title: "Empty group".into(),
                order: 40,
            },
        ],
        vec![
            bookmark("b1", "Zebra", 30, None),
            bookmark("b2", "P2 queue", 2, Some("g1")),
            bookmark("b3", "P1 queue", 1, Some("g1")),
            bookmark("b4", "First", 10, None),
            bookmark("b5", "Orphan", 50, Some("deleted_group")),
        ],
    );
    assert_eq!(
        outline(&nodes),
        [
            "First",
            "Queues",
            "  P1 queue",
            "  P2 queue",
            "Zebra",
            "Empty group",
            "Orphan",
        ]
    );

    let snapshot = NavigationSnapshot {
        applications: Vec::new(),
        favorites: nodes,
    };
    assert_eq!(
        snapshot
            .favorite_for(&list_target("incident", "b3"))
            .map(|node| node.id.as_str()),
        Some("b3"),
        "favorites inside groups are found"
    );
    assert!(
        snapshot
            .favorite_for(&list_target("incident", "nope"))
            .is_none()
    );
    assert_eq!(snapshot.favorite_groups().count(), 2);
}

#[test]
fn filtering_keeps_matching_branches_and_whole_matching_groups() {
    let tree = NavNode::group(
        "app",
        "Incident",
        vec![
            NavNode::group(
                "sec",
                "Open work",
                vec![
                    NavNode::link("a", "Assigned to me", list_target("incident", "")),
                    NavNode::link("b", "Unassigned", list_target("incident", "")),
                ],
            ),
            NavNode::link("c", "Problems", list_target("problem", "")),
        ],
    );
    assert_eq!(
        outline(&[tree.filtered("unassigned").unwrap()]),
        ["Incident", "  Open work", "    Unassigned"]
    );
    // A matching group keeps all of its children.
    assert_eq!(
        tree.filtered("open work").unwrap().children[0]
            .children
            .len(),
        2
    );
    // Links also match on their table name.
    assert_eq!(
        outline(&[tree.filtered("problem").unwrap()]),
        ["Incident", "  Problems"]
    );
    assert!(tree.filtered("zzz").is_none());
}

#[test]
fn classic_targets_round_trip_through_bookmark_urls() {
    let origin = "https://example.service-now.com";
    for target in [
        list_target("incident", ""),
        list_target(
            "incident",
            "active=true^priority=1^ORpriority=2^ORDERBYDESCnumber",
        ),
        ClassicTarget::List {
            table: "incident".into(),
            query: "short_descriptionLIKEa&b=c d".into(),
            view: "ess".into(),
        },
        ClassicTarget::Form {
            table: "incident".into(),
            sys_id: "46d44a23a9fe19810012d100cca80666".into(),
            query: String::new(),
            view: "compact".into(),
        },
        ClassicTarget::Form {
            table: "incident".into(),
            sys_id: "-1".into(),
            query: "priority=2".into(),
            view: String::new(),
        },
    ] {
        assert_eq!(
            ClassicTarget::from_url(origin, &target.to_url()),
            Some(target.clone()),
            "{}",
            target.to_url()
        );
    }
    assert_eq!(list_target("incident", "").to_url(), "incident_list.do");
}

#[test]
fn favorites_can_be_moved_reordered_and_renamed_in_place() {
    let fav =
        |id: &str, order: i64| NavNode::link(id, id, list_target("incident", id)).with_order(order);
    let mut snapshot = NavigationSnapshot {
        applications: Vec::new(),
        favorites: vec![
            fav("a", 10),
            NavNode::group("g", "Group", vec![fav("x", 10), fav("y", 20)]).with_order(20),
            fav("b", 30),
        ],
    };
    assert_eq!(snapshot.favorite_parent("y"), Some(Some("g".into())));
    assert_eq!(snapshot.favorite_parent("b"), Some(None));
    assert_eq!(snapshot.favorite_parent("missing"), None);
    assert_eq!(snapshot.next_favorite_order(None), 40);
    assert_eq!(snapshot.next_favorite_order(Some("g")), 30);

    // Move "b" into the group ahead of "y": only rows whose order changed are reported.
    let moved = snapshot.take_favorite("b").unwrap();
    let changed = snapshot
        .place_favorite(moved, Some("g"), Some("y"))
        .unwrap();
    assert_eq!(changed, [("b".to_string(), 20), ("y".to_string(), 30)]);
    assert_eq!(
        outline(&snapshot.favorites),
        ["a", "Group", "  x", "  b", "  y"]
    );

    // Reorder at the top level: the group moves ahead of "a".
    let group = snapshot.take_favorite("g").unwrap();
    let changed = snapshot.place_favorite(group, None, Some("a")).unwrap();
    assert_eq!(changed, [("g".to_string(), 10), ("a".to_string(), 20)]);
    assert_eq!(outline(&snapshot.favorites)[0], "Group");

    // An unknown destination leaves the tree alone.
    assert!(
        snapshot
            .place_favorite(fav("z", 0), Some("nope"), None)
            .is_none()
    );
    assert!(snapshot.favorite_parent("z").is_none());

    assert!(snapshot.rename_favorite("x", "Renamed"));
    assert!(snapshot.rename_favorite("g", "Queues"));
    assert!(!snapshot.rename_favorite("missing", "x"));
    assert_eq!(outline(&snapshot.favorites)[..2], ["Queues", "  Renamed"]);
}
