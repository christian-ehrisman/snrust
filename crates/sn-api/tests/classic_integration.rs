use std::sync::Arc;

use serde_json::{Value, json};
use sn_api::{FavoriteChange, MemoryCredentialStore, SnClient, StoredCredentials};
use sn_core::{ClassicTarget, InstanceProfile, NavNode, SnError, TableHierarchy};
use wiremock::matchers::{bearer_token, body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer, token: &str) -> SnClient {
    let profile = InstanceProfile::new("Test", &server.uri(), "client", false).unwrap();
    let store = Arc::new(MemoryCredentialStore::with_credentials(
        profile.id.clone(),
        StoredCredentials {
            access_token: token.into(),
            refresh_token: None,
            expires_at: None,
            client_secret: None,
        },
    ));
    SnClient::new(profile, store).unwrap()
}

async fn table(server: &MockServer, name: &str, result: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/api/now/table/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": result })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn same_origin_accounts_and_bookmarks_follow_the_bearer_principal() {
    let server = MockServer::start().await;
    for (token, user) in [("token-a", "alice"), ("token-b", "bob")] {
        Mock::given(method("GET"))
            .and(path("/api/now/table/sys_user"))
            .and(bearer_token(token))
            .and(query_param(
                "sysparm_query",
                "sys_id=javascript:gs.getUserID()",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": [
                {"sys_id": user, "user_name": user, "name": user}
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/api/now/table/sys_ui_bookmark")).and(bearer_token(token))
            .and(query_param("sysparm_query", "user=javascript:gs.getUserID()^ORDERBYorder^ORDERBYsys_id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": [
                {"sys_id":"valid","title":user,"url":"incident_list.do?sysparm_query=active%3Dtrue&sysparm_view=ess"},
                {"sys_id":"record","title":"Record","url":"incident.do?sys_id=0123456789abcdef0123456789abcdef"},
                {"sys_id":"workspace","title":"Workspace","url":"/now/workspace/agent/home"},
                {"sys_id":"external","title":"External","url":"https://external.example/incident_list.do"},
                {"sys_id":"processor","title":"Home","url":"home.do"}
            ]}))).expect(1).mount(&server).await;
    }
    // No bookmark groups on this instance: favorites stay a flat list.
    table(&server, "sys_ui_bookmark_group", json!([])).await;
    let a = client(&server, "token-a");
    let b = client(&server, "token-b");
    assert_ne!(a.profile().id, b.profile().id);
    for (client, user) in [(a, "alice"), (b, "bob")] {
        assert_eq!(client.current_account().await.unwrap().user_name, user);
        let favorites = client.fetch_user_favorites().await.unwrap();
        assert_eq!(favorites.len(), 2);
        assert_eq!(favorites[0].title, user);
        assert!(
            matches!(favorites[0].target(), Some(ClassicTarget::List { query, view, .. }) if query == "active=true" && view == "ess")
        );
    }
}

#[tokio::test]
async fn personal_named_list_layout_wins_and_parent_default_is_fallback() {
    let server = MockServer::start().await;
    table(
        &server,
        "sys_ui_list",
        json!([
            {"sys_id":"shared","name":"incident","user":"","view":""},
            {"sys_id":"personal","name":"incident","user":"current-user","view":"","view.name":""},
            {"sys_id":"named","name":"incident","user":"","view":"Some label","view.name":"ess"},
            {"sys_id":"parent","name":"task","user":"","view":""}
        ]),
    )
    .await;
    for (id, fields) in [
        (
            "personal",
            json!([{"element":"sys_created_by","position":"2"},{"element":"number","position":"0"},{"element":"sys_created_on","position":"1"}]),
        ),
        (
            "named",
            json!([{"element":"short_description","position":"0"},{"element":"caller_id.name","position":"1"}]),
        ),
        ("parent", json!([{"element":"number","position":"0"}])),
    ] {
        Mock::given(method("GET"))
            .and(path("/api/now/table/sys_ui_list_element"))
            .and(query_param(
                "sysparm_query",
                format!("list_id={id}^ORDERBYposition^ORDERBYsys_id"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":fields})))
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = client(&server, "token");
    let chain = vec!["incident".into(), "task".into()];
    assert_eq!(
        client
            .fetch_list_columns(&chain, "")
            .await
            .unwrap()
            .unwrap(),
        ["number", "sys_created_on", "sys_created_by"]
    );
    assert_eq!(
        client
            .fetch_list_columns(&chain, "ess")
            .await
            .unwrap()
            .unwrap(),
        ["short_description", "caller_id.name"]
    );
    assert_eq!(
        client
            .fetch_list_columns(&["rm_story".into(), "task".into()], "unknown")
            .await
            .unwrap()
            .unwrap(),
        ["number"]
    );
}

#[tokio::test]
async fn form_views_choose_child_sections_in_order_and_exclude_workspaces() {
    let server = MockServer::start().await;
    table(
        &server,
        "sys_ui_form",
        json!([
            {"sys_id":"parent","name":"task","view":""},
            {"sys_id":"child","name":"incident","view":""},
            {"sys_id":"named","name":"task","view":"Audit","view.name":"audit"},
            {"sys_id":"workspace","name":"incident","view":"Workspace","view.name":"workspace"}
        ]),
    )
    .await;
    table(
        &server,
        "sys_ui_form_section",
        json!([
            {"sys_ui_form":"child","sys_ui_section":"second","position":"1"},
            {"sys_ui_form":"child","sys_ui_section":"first","position":"0"},
            {"sys_ui_form":"named","sys_ui_section":"audit","position":"0"},
            {"sys_ui_form":"parent","sys_ui_section":"wrong","position":"0"}
        ]),
    )
    .await;
    table(&server, "sys_ui_section", json!([
        {"sys_id":"first","caption":"Details"},{"sys_id":"second","caption":"Resolution"},{"sys_id":"audit","caption":"History"}
    ])).await;
    table(
        &server,
        "sys_ui_element",
        json!([
            {"sys_ui_section":"first","element":"description","position":"1"},
            {"sys_ui_section":"first","element":"number","position":"0"},
            {"sys_ui_section":"second","element":"close_notes","position":"0"},
            {"sys_ui_section":"audit","element":"sys_created_by","position":"0"}
        ]),
    )
    .await;
    let views = client(&server, "token")
        .fetch_form_views(&["incident".into(), "task".into()])
        .await
        .unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].name, "");
    assert_eq!(views[0].sections[0].title, "Details");
    assert_eq!(views[0].sections[0].elements, ["number", "description"]);
    assert_eq!(views[0].sections[1].title, "Resolution");
    assert_eq!(views[1].name, "audit");
}

#[tokio::test]
async fn application_menu_filters_roles_and_only_routes_classic_lists_and_forms() {
    let server = MockServer::start().await;
    table(
        &server,
        "sys_app_application",
        json!([
            {"sys_id":"itsm","title":"Service Desk","roles":"itil"},
            {"sys_id":"admin","title":"Administration","roles":"admin"}
        ]),
    )
    .await;
    table(&server, "sys_user_has_role", json!([{"role.name":"itil"}])).await;
    table(&server, "sys_app_module", json!([
        {"sys_id":"list","application":"itsm","title":"Open incidents","link_type":"LIST","name":"incident","filter":"active=true^priority=1^ORpriority=2","view_name":"ess"},
        {"sys_id":"new","application":"itsm","title":"Create incident","link_type":"NEW","name":"incident","query":"sysparm_query=priority%3D2&sysparm_view=ess"},
        {"sys_id":"direct","application":"itsm","title":"Record","link_type":"DIRECT","query":"incident.do?sys_id=0123456789abcdef0123456789abcdef&sysparm_view=ess"},
        {"sys_id":"blocked","application":"itsm","title":"Security","link_type":"LIST","name":"incident","roles":"security_admin"},
        {"sys_id":"workspace","application":"itsm","title":"Workspace","link_type":"DIRECT","query":"/now/workspace/agent/home"},
        {"sys_id":"script","application":"itsm","title":"Script","link_type":"SCRIPT","query":"javascript:alert(1)"},
        {"sys_id":"admin","application":"admin","title":"Admin list","link_type":"LIST","name":"sys_user"},
        {"sys_id":"override","application":"admin","title":"Public module","link_type":"LIST","name":"incident","override_menu_roles":"true"}
    ])).await;
    let apps = client(&server, "token")
        .fetch_application_menu()
        .await
        .unwrap();
    assert_eq!(apps.len(), 2);
    assert_eq!(apps[0].id, "itsm");
    assert_eq!(
        apps[0]
            .children
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["list", "new", "direct"]
    );
    assert_eq!(apps[1].children[0].id, "override");
    assert!(
        matches!(apps[0].children[0].target(), Some(ClassicTarget::List { query, view, .. }) if query == "active=true^priority=1^ORpriority=2" && view == "ess")
    );
    assert!(
        matches!(apps[0].children[1].target(), Some(ClassicTarget::Form { sys_id, query, .. }) if sys_id == "-1" && query == "priority=2")
    );
}

fn titles(nodes: &[NavNode]) -> Vec<String> {
    fn walk(nodes: &[NavNode], depth: usize, out: &mut Vec<String>) {
        for node in nodes {
            out.push(format!("{}{}", "> ".repeat(depth), node.title));
            walk(&node.children, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, &mut out);
    out
}

#[tokio::test]
async fn separators_nest_modules_and_bookmark_groups_nest_favorites() {
    let server = MockServer::start().await;
    table(
        &server,
        "sys_app_application",
        json!([{"sys_id":"itsm","title":"Incident","roles":""}]),
    )
    .await;
    table(&server, "sys_user_has_role", json!([{"role.name":"itil"}])).await;
    // The separator query must be requested, or sections cannot be rebuilt.
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_app_module"))
        .and(query_param(
            "sysparm_query",
            "active=true^link_typeINLIST,NEW,DETAIL,DIRECT,SEPARATOR^ORDERBYorder^ORDERBYtitle",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": [
            {"sys_id":"new","application":"itsm","title":"Create New","link_type":"NEW","name":"incident"},
            {"sys_id":"sep_open","application":"itsm","title":"Open","link_type":"SEPARATOR"},
            {"sys_id":"mine","application":"itsm","title":"Assigned to me","link_type":"LIST","name":"incident","filter":"assigned_to=javascript:gs.getUserID()"},
            {"sys_id":"secret","application":"itsm","title":"Security","link_type":"LIST","name":"incident","roles":"security_admin"},
            {"sys_id":"sep_admin","application":"itsm","title":"Admin only","link_type":"SEPARATOR","roles":"admin"},
            {"sys_id":"all","application":"itsm","title":"All","link_type":"LIST","name":"incident"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    table(
        &server,
        "sys_ui_bookmark_group",
        json!([
            {"sys_id":"g_queues","title":"Queues","order":"100"}
        ]),
    )
    .await;
    table(&server, "sys_ui_bookmark", json!([
        {"sys_id":"b_p1","title":"P1","url":"incident_list.do?sysparm_query=priority%3D1","group":"g_queues","order":"1"},
        {"sys_id":"b_home","title":"My incidents","url":"incident_list.do","order":"50"},
        {"sys_id":"b_ws","title":"Workspace","url":"/now/workspace/agent/home","group":"g_queues","order":"2"}
    ])).await;

    let client = client(&server, "token");
    assert_eq!(
        titles(&client.fetch_application_menu().await.unwrap()),
        // A separator the user's roles hide does not start a section, so "All" stays in
        // the section before it, exactly as the classic navigator renders it.
        [
            "Incident",
            "> Create New",
            "> Open",
            "> > Assigned to me",
            "> > All"
        ]
    );
    assert_eq!(
        titles(&client.fetch_user_favorites().await.unwrap()),
        ["My incidents", "Queues", "> P1"]
    );
}

#[tokio::test]
async fn favorites_fall_back_to_a_flat_list_when_groups_are_unreadable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_ui_bookmark_group"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"error": {"message": "ACL"}})),
        )
        .mount(&server)
        .await;
    table(&server, "sys_ui_bookmark", json!([
        {"sys_id":"b1","title":"Grouped","url":"incident_list.do","group":"hidden_group","order":"1"}
    ])).await;
    assert_eq!(
        titles(
            &client(&server, "token")
                .fetch_user_favorites()
                .await
                .unwrap()
        ),
        ["Grouped"]
    );
}

#[tokio::test]
async fn favorite_writes_touch_only_the_users_bookmark_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": [
            {"sys_id": "user_1", "user_name": "ada", "name": "Ada"}
        ]})))
        .mount(&server)
        .await;
    let target = ClassicTarget::List {
        table: "incident".into(),
        query: "active=true^priority=1".into(),
        view: String::new(),
    };
    Mock::given(method("POST"))
        .and(path("/api/now/table/sys_ui_bookmark"))
        .and(body_json(json!({
            "title": "P1 incidents",
            "url": "incident_list.do?sysparm_query=active%3Dtrue%5Epriority%3D1",
            "user": "user_1",
            "group": "g1",
            "order": "70"
        })))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"result": {"sys_id": "new_fav"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/now/table/sys_ui_bookmark/new_fav"))
        .and(body_json(json!({"title": "Renamed", "group": ""})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"result": {"sys_id": "new_fav"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/now/table/sys_ui_bookmark_group"))
        .and(body_json(
            json!({"title": "Queues", "user": "user_1", "order": "10"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"result": {"sys_id": "g2"}})))
        .expect(1)
        .mount(&server)
        .await;
    // Deleting a group first moves its member out, then deletes only the group.
    Mock::given(method("PATCH"))
        .and(path("/api/now/table/sys_ui_bookmark/member"))
        .and(body_json(json!({"group": ""})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"result": {"sys_id": "member"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for (table, id) in [
        ("sys_ui_bookmark_group", "g2"),
        ("sys_ui_bookmark", "new_fav"),
    ] {
        Mock::given(method("DELETE"))
            .and(path(format!("/api/now/table/{table}/{id}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = client(&server, "token");
    assert_eq!(
        client
            .create_favorite("P1 incidents", &target, Some("g1"), 70)
            .await
            .unwrap(),
        "new_fav"
    );
    client
        .update_favorite(
            "new_fav",
            &FavoriteChange {
                title: Some("Renamed".into()),
                group_id: Some(None),
                order: None,
            },
        )
        .await
        .unwrap();
    // An empty change sends nothing (no PATCH mock would match it).
    client
        .update_favorite("new_fav", &FavoriteChange::default())
        .await
        .unwrap();
    assert_eq!(
        client.create_favorite_group("Queues", 10).await.unwrap(),
        "g2"
    );
    client
        .delete_favorite_group("g2", &["member".to_string()])
        .await
        .unwrap();
    client.delete_favorite("new_fav").await.unwrap();
}

#[tokio::test]
async fn catalog_pagination_resolves_story_parent_on_the_next_page() {
    let server = MockServer::start().await;
    let mut first: Vec<_> = (0..1000)
        .map(|i| json!({"sys_id":format!("id{i}"),"name":format!("table_{i}"),"super_class":""}))
        .collect();
    first[0] = json!({"sys_id":"story","name":"rm_story","super_class":"parent"});
    for (offset, records) in [
        (0, json!(first)),
        (
            1000,
            json!([{"sys_id":"parent","name":"task","super_class":""}]),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path("/api/now/table/sys_db_object"))
            .and(query_param("sysparm_offset", offset.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":records})))
            .expect(1)
            .mount(&server)
            .await;
    }
    table(&server, "sys_number", json!([])).await;
    table(&server, "sys_dictionary", json!([
        {"name":"sys","element":"sys_created_by","column_label":"Created by","internal_type":"string","read_only":"true"},
        {"name":"task","element":"short_description","column_label":"Short description","internal_type":"string"},
        {"name":"rm_story","element":"short_description","column_label":"Story title","internal_type":"string"}
    ])).await;
    table(&server, "sys_choice", json!([])).await;
    table(&server, "sys_ui_list", json!([])).await;
    let client = client(&server, "token");
    let hierarchy = TableHierarchy::from_descriptors(client.fetch_table_catalog().await.unwrap());
    assert_eq!(
        hierarchy.inheritance_chain("rm_story"),
        ["rm_story", "task"]
    );
    let schema = client
        .fetch_table_schema("rm_story", &hierarchy)
        .await
        .unwrap();
    assert_eq!(schema.fields["sys_created_by"].defined_on_table, "sys");
    assert_eq!(
        schema.fields["short_description"].column_label,
        "Story title"
    );
    assert!(schema.fields.contains_key("sys_created_on"));
}

#[tokio::test]
async fn metadata_denials_and_hidden_identity_are_explicit_errors() {
    let server = MockServer::start().await;
    table(&server, "sys_user", json!([])).await;
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_ui_bookmark"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"error":{"message":"Forbidden","detail":"ACL denied"}})),
        )
        .mount(&server)
        .await;
    let client = client(&server, "token");
    assert!(matches!(
        client.current_account().await,
        Err(SnError::InvalidConfig(_))
    ));
    assert!(matches!(
        client.fetch_user_favorites().await,
        Err(SnError::Api { status: 403, .. })
    ));
}

#[tokio::test]
async fn saved_filters_list_personal_and_global_and_write_only_personal_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": [
            {"sys_id": "user_1", "user_name": "ada", "name": "Ada"}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_filter"))
        .and(query_param(
            "sysparm_query",
            "tableINincident,task^user=javascript:gs.getUserID()^ORuserISEMPTY^groupISEMPTY^ORDERBYtitle",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": [
            {"sys_id":"f1","title":"My P1s","table":"incident","filter":"priority=1^assigned_to=javascript:gs.getUserID()","user":"user_1"},
            {"sys_id":"f2","title":"Open tasks","table":"task","filter":"active=true","user":""},
            {"sys_id":"f3","title":"","table":"incident","filter":"active=false","user":""}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/now/table/sys_filter"))
        .and(body_json(json!({
            "title": "Unassigned",
            "table": "incident",
            "filter": "assigned_toISEMPTY^ORDERBYDESCnumber",
            "user": "user_1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"result": {"sys_id": "f9"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/now/table/sys_filter/f9"))
        .and(body_json(json!({"filter": "assigned_toISEMPTY"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": {"sys_id": "f9"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/now/table/sys_filter/f9"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, "token");
    let filters = client
        .fetch_saved_filters(&["incident".to_string(), "task".to_string()])
        .await
        .unwrap();
    assert_eq!(
        filters
            .iter()
            .map(|filter| (filter.title.as_str(), filter.editable))
            .collect::<Vec<_>>(),
        [("My P1s", true), ("Open tasks", false)]
    );
    assert_eq!(
        client
            .create_saved_filter(
                "Unassigned",
                "incident",
                "assigned_toISEMPTY^ORDERBYDESCnumber"
            )
            .await
            .unwrap(),
        "f9"
    );
    client
        .update_saved_filter("f9", None, Some("assigned_toISEMPTY"))
        .await
        .unwrap();
    client.update_saved_filter("f9", None, None).await.unwrap();
    client.delete_saved_filter("f9").await.unwrap();
}
