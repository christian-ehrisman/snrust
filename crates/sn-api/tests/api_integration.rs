//! End-to-end `wiremock` integration tests for `sn-api`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use sn_api::{
    CredentialStore, ListRecordsParams, MemoryCredentialStore, OAuthPkceFlow, PkceChallenge,
    SnClient, StoredCredentials,
};
use sn_core::{FieldType, InstanceProfile, SnError, TableHierarchy};
use tokio::sync::watch;
use url::Url;
use wiremock::matchers::{
    bearer_token, body_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_profile(mock_uri: &str) -> InstanceProfile {
    let mut profile = InstanceProfile::new("Test Dev", mock_uri, "snrust-client-id", true)
        .expect("valid mock instance profile");
    profile.callback_port = 0;
    profile
}

fn valid_creds(token: &str) -> StoredCredentials {
    StoredCredentials {
        access_token: token.to_string(),
        refresh_token: Some("refresh-tok-001".to_string()),
        expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
        client_secret: Some("conf-secret-xyz".to_string()),
    }
}

#[test]
fn pkce_s256_rfc7636_vector_and_credential_expiry_checks() {
    // RFC 7636 Appendix B canonical test vector.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = PkceChallenge::compute_s256_challenge(verifier);
    assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");

    let generated = PkceChallenge::generate();
    assert_eq!(generated.code_verifier.len(), 64);
    assert_eq!(generated.state.len(), 32);
    assert_eq!(
        PkceChallenge::compute_s256_challenge(&generated.code_verifier),
        generated.code_challenge
    );

    let store = MemoryCredentialStore::new();
    let id = sn_core::InstanceId::from("inst-1");
    store.save_client_secret(&id, "prelogin-secret").unwrap();
    assert_eq!(
        store.get_client_secret(&id).unwrap().as_deref(),
        Some("prelogin-secret")
    );
    assert!(store.get_credentials(&id).unwrap().is_none());

    let expiring_soon = StoredCredentials {
        access_token: "tok".to_string(),
        refresh_token: None,
        expires_at: Some(Utc::now() + ChronoDuration::seconds(30)),
        client_secret: None,
    };
    assert!(expiring_soon.is_expired_or_expiring(60));
    assert!(!expiring_soon.is_expired_or_expiring(5));

    store.save_credentials(&id, &expiring_soon).unwrap();
    let loaded = store.get_credentials(&id).unwrap().unwrap();
    assert_eq!(loaded.client_secret.as_deref(), Some("prelogin-secret"));
}

#[tokio::test]
async fn oauth_pkce_loopback_flow_exchanges_code_and_persists_credentials() {
    let server = MockServer::start().await;
    let profile = test_profile(&server.uri());
    let store = Arc::new(MemoryCredentialStore::new());
    store
        .save_client_secret(&profile.id, "vault-secret-42")
        .unwrap();

    let session = OAuthPkceFlow::start_pkce_listener(&profile, None, store.clone(), false, None)
        .await
        .expect("loopback listener should bind ephemeral port");

    let parsed_auth = Url::parse(&session.authorize_url).unwrap();
    assert_eq!(parsed_auth.path(), "/oauth_auth.do");
    let auth_params: HashMap<String, String> = parsed_auth
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        auth_params.get("response_type").map(String::as_str),
        Some("code")
    );
    assert_eq!(
        auth_params.get("client_id").map(String::as_str),
        Some("snrust-client-id")
    );
    assert_eq!(
        auth_params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(
        auth_params.get("scope").map(String::as_str),
        Some("useraccount")
    );
    let expected_challenge = auth_params.get("code_challenge").cloned().unwrap();
    let state = session.state.clone();
    let callback_port = session.callback_port;

    Mock::given(method("POST"))
        .and(path("/oauth_token.do"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=sn_code_abc123"))
        .and(body_string_contains("client_id=snrust-client-id"))
        .and(body_string_contains("client_secret=vault-secret-42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "fresh-pkce-access-token",
            "refresh_token": "fresh-pkce-refresh-token",
            "expires_in": 1799,
            "token_type": "Bearer",
            "scope": "useraccount"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let redirect_task = tokio::spawn(async move {
        let browser = reqwest::Client::new();
        // Simulate an incidental /favicon.ico probe first to ensure listener ignores non-callback paths.
        let fav = browser
            .get(format!("http://127.0.0.1:{callback_port}/favicon.ico"))
            .send()
            .await
            .unwrap();
        assert_eq!(fav.status().as_u16(), 404);

        let resp = browser
            .get(format!(
                "http://127.0.0.1:{callback_port}/oauth/callback?code=sn_code_abc123&state={state}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let html = resp.text().await.unwrap();
        assert!(html.contains("Authenticated with ServiceNow"));
    });

    let creds = session
        .wait_for_callback(Duration::from_secs(5))
        .await
        .expect("PKCE callback and token exchange should succeed");
    redirect_task.await.unwrap();

    assert_eq!(creds.access_token, "fresh-pkce-access-token");
    assert_eq!(
        creds.refresh_token.as_deref(),
        Some("fresh-pkce-refresh-token")
    );
    assert_eq!(creds.client_secret.as_deref(), Some("vault-secret-42"));
    assert!(creds.expires_at.is_some());

    // Verify the code_verifier sent to `/oauth_token.do` hashes to the exact `code_challenge`.
    let received = server.received_requests().await.unwrap();
    let token_req = received
        .iter()
        .find(|r| r.url.path() == "/oauth_token.do")
        .expect("token endpoint called");
    let form_pairs: HashMap<String, String> = url::form_urlencoded::parse(&token_req.body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let sent_verifier = form_pairs.get("code_verifier").expect("code_verifier sent");
    assert_eq!(
        PkceChallenge::compute_s256_challenge(sent_verifier),
        expected_challenge
    );

    let persisted = store
        .get_credentials(&profile.id)
        .unwrap()
        .expect("stored in credential store");
    assert_eq!(persisted.access_token, "fresh-pkce-access-token");
}

#[tokio::test]
async fn oauth_pkce_rejects_csrf_state_mismatch_and_supports_cancel() {
    let server = MockServer::start().await;
    let profile = test_profile(&server.uri());
    let store = Arc::new(MemoryCredentialStore::new());

    let session = OAuthPkceFlow::start_pkce_listener(&profile, None, store.clone(), false, None)
        .await
        .unwrap();
    let port = session.callback_port;

    tokio::spawn(async move {
        let _ = reqwest::get(format!(
            "http://127.0.0.1:{port}/oauth/callback?code=abc&state=tampered_state"
        ))
        .await;
    });

    let err = session
        .wait_for_callback(Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, SnError::OAuth(msg) if msg.contains("state")));

    // Cancellation channel check
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let session2 =
        OAuthPkceFlow::start_pkce_listener(&profile, None, store, false, Some(cancel_rx))
            .await
            .unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = cancel_tx.send(true);
    });
    let cancel_err = session2
        .wait_for_callback(Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(cancel_err, SnError::OAuth(msg) if msg.contains("cancel")));
}

#[tokio::test]
async fn single_flight_refresh_on_expiring_token_and_401_retry_with_cookies() {
    let server = MockServer::start().await;
    let profile = test_profile(&server.uri());

    // Part A: Expiring token refreshed ONCE across 6 concurrent requests.
    let expiring = StoredCredentials {
        access_token: "about-to-expire".to_string(),
        refresh_token: Some("persist-refresh-token".to_string()),
        expires_at: Some(Utc::now() + ChronoDuration::seconds(15)),
        client_secret: Some("conf-secret-xyz".to_string()),
    };
    let store = Arc::new(MemoryCredentialStore::with_credentials(
        profile.id.clone(),
        expiring,
    ));
    let client = SnClient::new(profile.clone(), store.clone()).unwrap();

    Mock::given(method("POST"))
        .and(path("/oauth_token.do"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=persist-refresh-token"))
        .and(body_string_contains("client_secret=conf-secret-xyz"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(40))
                // Omit `refresh_token` to verify existing refresh_token + client_secret are preserved!
                .set_body_json(json!({
                    "access_token": "refreshed-bearer-001",
                    "expires_in": 1800
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/now/table/incident/inc_sys_1"))
        .and(bearer_token("refreshed-bearer-001"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("Set-Cookie", "glide_user_route=glide.node1; Path=/")
                .append_header("Set-Cookie", "JSESSIONID=sess_9988; Path=/")
                .set_body_json(json!({
                    "result": {
                        "sys_id": { "value": "inc_sys_1", "display_value": "inc_sys_1" },
                        "number": { "value": "INC0010001", "display_value": "INC0010001" }
                    }
                })),
        )
        .expect(6)
        .mount(&server)
        .await;

    let (r1, r2, r3, r4, r5, r6) = tokio::try_join!(
        client.get_record("incident", "inc_sys_1"),
        client.get_record("incident", "inc_sys_1"),
        client.get_record("incident", "inc_sys_1"),
        client.get_record("incident", "inc_sys_1"),
        client.get_record("incident", "inc_sys_1"),
        client.get_record("incident", "inc_sys_1"),
    )
    .expect("all 6 concurrent requests should succeed via single-flight refresh");
    assert_eq!(r1.get_display("number"), "INC0010001");
    assert_eq!(r6.sys_id, "inc_sys_1");
    let _ = (r2, r3, r4, r5);

    let saved = store.get_credentials(&profile.id).unwrap().unwrap();
    assert_eq!(saved.access_token, "refreshed-bearer-001");
    assert_eq!(
        saved.refresh_token.as_deref(),
        Some("persist-refresh-token")
    );
    assert_eq!(saved.client_secret.as_deref(), Some("conf-secret-xyz"));

    // Verify AMB WebSocket helper exposes both Bearer token and session cookies!
    let (bearer, cookies) = client.bearer_token_and_cookies().await.unwrap();
    assert_eq!(bearer, "refreshed-bearer-001");
    let cookie_str = cookies.expect("Set-Cookie headers should be captured in Jar");
    assert!(cookie_str.contains("glide_user_route=glide.node1"));
    assert!(cookie_str.contains("JSESSIONID=sess_9988"));

    // Part B: Non-expired token gets 401 Unauthorized -> automatically refreshes once and retries.
    let server_401 = MockServer::start().await;
    let profile_401 = test_profile(&server_401.uri());
    let store_401 = Arc::new(MemoryCredentialStore::with_credentials(
        profile_401.id.clone(),
        valid_creds("revoked-but-unexpired-token"),
    ));
    let client_401 = SnClient::new(profile_401, store_401).unwrap();

    Mock::given(method("GET"))
        .and(path("/api/now/table/incident/inc_401"))
        .and(bearer_token("revoked-but-unexpired-token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "message": "User Not Authenticated" }
        })))
        .expect(1)
        .mount(&server_401)
        .await;

    Mock::given(method("POST"))
        .and(path("/oauth_token.do"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "recovered-after-401",
            "refresh_token": "refresh-tok-002",
            "expires_in": 1800
        })))
        .expect(1)
        .mount(&server_401)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/now/table/incident/inc_401"))
        .and(bearer_token("recovered-after-401"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": {
                "sys_id": { "value": "inc_401", "display_value": "inc_401" },
                "short_description": { "value": "VPN outage", "display_value": "VPN outage" }
            }
        })))
        .expect(1)
        .mount(&server_401)
        .await;

    let rec = client_401.get_record("incident", "inc_401").await.unwrap();
    assert_eq!(rec.get_display("short_description"), "VPN outage");
    assert!(!rec.is_partial);
}

#[tokio::test]
async fn list_records_parses_dual_values_total_count_and_max_updated_on() {
    let server = MockServer::start().await;
    let profile = test_profile(&server.uri());
    let store = Arc::new(MemoryCredentialStore::with_credentials(
        profile.id.clone(),
        valid_creds("tok-list"),
    ));
    let client = SnClient::new(profile, store).unwrap();

    Mock::given(method("GET"))
        .and(path("/api/now/table/incident"))
        .and(query_param("sysparm_display_value", "all"))
        .and(query_param("sysparm_exclude_reference_link", "true"))
        .and(query_param("sysparm_query", "active=true^ORDERBYDESCpriority"))
        .and(query_param("sysparm_offset", "0"))
        .and(query_param("sysparm_limit", "25"))
        .and(query_param(
            "sysparm_fields",
            "number,state,caller_id,sys_id,sys_updated_on,sys_mod_count",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Total-Count", "247")
                .set_body_json(json!({
                    "result": [
                        {
                            "sys_id": { "value": "a1", "display_value": "a1" },
                            "number": { "value": "INC0010010", "display_value": "INC0010010" },
                            "state": { "value": "2", "display_value": "In Progress" },
                            "caller_id": { "value": "usr_99", "display_value": "Ada Lovelace" },
                            "sys_updated_on": { "value": "2026-11-11 14:10:00", "display_value": "2026-11-11 09:10:00" },
                            "sys_mod_count": { "value": "3", "display_value": "3" }
                        },
                        {
                            "sys_id": { "value": "a2", "display_value": "a2" },
                            "number": { "value": "INC0010011", "display_value": "INC0010011" },
                            "state": { "value": "1", "display_value": "New" },
                            "caller_id": { "value": "usr_88", "display_value": "Grace Hopper" },
                            "sys_updated_on": { "value": "2026-11-11 16:45:00", "display_value": "2026-11-11 11:45:00" },
                            "sys_mod_count": { "value": "1", "display_value": "1" }
                        }
                    ]
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let page = client
        .list_records(
            "incident",
            &ListRecordsParams {
                encoded_query: "active=true^ORDERBYDESCpriority".to_string(),
                fields: vec![
                    "number".to_string(),
                    "state".to_string(),
                    "caller_id".to_string(),
                ],
                offset: 0,
                limit: 25,
                include_total_count: true,
            },
        )
        .await
        .unwrap();

    assert_eq!(page.total_count, Some(247));
    assert_eq!(page.max_updated_on.as_deref(), Some("2026-11-11 16:45:00"));
    assert!(!page.from_cache);
    assert_eq!(page.records.len(), 2);
    assert!(page.records[0].is_partial);
    assert_eq!(page.records[0].get_raw("state"), "2");
    assert_eq!(page.records[0].get_display("state"), "In Progress");
    assert_eq!(page.records[0].get_raw("caller_id"), "usr_99");
    assert_eq!(page.records[0].get_display("caller_id"), "Ada Lovelace");
    assert_eq!(page.records[0].sys_mod_count, 3);
}

#[tokio::test]
async fn patch_create_delete_watermark_journal_and_reference_lookup() {
    let server = MockServer::start().await;
    let profile = test_profile(&server.uri());
    let store = Arc::new(MemoryCredentialStore::with_credentials(
        profile.id.clone(),
        valid_creds("tok-write"),
    ));
    let client = SnClient::new(profile, store).unwrap();

    // 1. patch_record
    Mock::given(method("PATCH"))
        .and(path("/api/now/table/incident/inc_patch_1"))
        .and(header("authorization", "Bearer tok-write"))
        .and(query_param("sysparm_display_value", "all"))
        .and(query_param("sysparm_exclude_reference_link", "true"))
        .and(body_json(json!({
            "state": "2",
            "short_description": "Core switch packet loss (investigating)"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": {
                "sys_id": { "value": "inc_patch_1", "display_value": "inc_patch_1" },
                "number": { "value": "INC0099111", "display_value": "INC0099111" },
                "state": { "value": "2", "display_value": "In Progress" },
                "short_description": {
                    "value": "Core switch packet loss (investigating)",
                    "display_value": "Core switch packet loss (investigating)"
                },
                "sys_updated_on": { "value": "2026-11-11 17:00:00", "display_value": "2026-11-11 12:00:00" },
                "sys_mod_count": { "value": "8", "display_value": "8" }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut dirty = serde_json::Map::new();
    dirty.insert("state".into(), json!("2"));
    dirty.insert(
        "short_description".into(),
        json!("Core switch packet loss (investigating)"),
    );

    let patched = client
        .patch_record("incident", "inc_patch_1", &dirty)
        .await
        .unwrap();
    assert!(!patched.is_partial);
    assert_eq!(patched.sys_id, "inc_patch_1");
    assert_eq!(patched.sys_mod_count, 8);
    assert_eq!(patched.get_display("state"), "In Progress");

    // 2. get_record_watermark
    Mock::given(method("GET"))
        .and(path("/api/now/table/incident/inc_patch_1"))
        .and(query_param(
            "sysparm_fields",
            "sys_id,sys_updated_on,sys_mod_count,sys_updated_by",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": {
                "sys_id": { "value": "inc_patch_1", "display_value": "inc_patch_1" },
                "sys_updated_on": { "value": "2026-11-11 17:01:00", "display_value": "2026-11-11 12:01:00" },
                "sys_mod_count": { "value": "9", "display_value": "9" },
                "sys_updated_by": { "value": "itil.user", "display_value": "itil.user" }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let wm = client
        .get_record_watermark("incident", "inc_patch_1")
        .await
        .unwrap();
    assert_eq!(wm.sys_mod_count, 9);
    assert_eq!(wm.sys_updated_by, "itil.user");

    // 3. find_record_by_query, get_journal_entries, search_reference, delete_record
    Mock::given(method("GET"))
        .and(path("/api/now/table/incident"))
        .and(query_param("sysparm_query", "number=INC0099111"))
        .and(query_param("sysparm_limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [{
                "sys_id": { "value": "inc_patch_1", "display_value": "inc_patch_1" },
                "number": { "value": "INC0099111", "display_value": "INC0099111" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let found = client
        .find_record_by_query("incident", "number=INC0099111")
        .await
        .unwrap()
        .expect("record found");
    assert_eq!(found.sys_id, "inc_patch_1");

    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_journal_field"))
        .and(query_param(
            "sysparm_query",
            "element_id=inc_patch_1^sys_created_on>2026-11-11 16:00:00^ORDERBYDESCsys_created_on",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [{
                "sys_id": { "value": "j1", "display_value": "j1" },
                "element": { "value": "work_notes", "display_value": "work_notes" },
                "element_id": { "value": "inc_patch_1", "display_value": "inc_patch_1" },
                "value": { "value": "Rerouted BGP peer", "display_value": "Rerouted BGP peer" },
                "sys_created_on": { "value": "2026-11-11 17:00:30", "display_value": "2026-11-11 12:00:30" },
                "sys_created_by": { "value": "neteng", "display_value": "neteng" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let notes = client
        .get_journal_entries("inc_patch_1", Some("2026-11-11 16:00:00"), 50)
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].is_work_note());
    assert_eq!(notes[0].value, "Rerouted BGP peer");

    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_user"))
        .and(query_param(
            "sysparm_query",
            "nameLIKEada^ORemailLIKEada^ORDERBYname",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [{
                "sys_id": { "value": "usr_99", "display_value": "usr_99" },
                "name": { "value": "Ada Lovelace", "display_value": "Ada Lovelace" },
                "email": { "value": "ada@analytical.org", "display_value": "ada@analytical.org" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let candidates = client
        .search_reference("sys_user", "name", "ada", Some("email"), 10)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].display_value, "Ada Lovelace");
    assert_eq!(
        candidates[0].secondary.as_deref(),
        Some("ada@analytical.org")
    );

    Mock::given(method("DELETE"))
        .and(path("/api/now/table/incident/inc_patch_1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    client
        .delete_record("incident", "inc_patch_1")
        .await
        .unwrap();
}

#[tokio::test]
async fn metadata_catalog_schema_inheritance_merge_and_acl_403_fallbacks() {
    let server = MockServer::start().await;
    let profile = test_profile(&server.uri());
    let store = Arc::new(MemoryCredentialStore::with_credentials(
        profile.id.clone(),
        valid_creds("tok-meta"),
    ));
    let client = SnClient::new(profile, store).unwrap();

    // 1. Catalog hydration (`sys_db_object` + `sys_number` in parallel)
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_db_object"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [
                {
                    "sys_id": { "value": "obj_task_id", "display_value": "obj_task_id" },
                    "name": { "value": "task", "display_value": "task" },
                    "label": { "value": "Task", "display_value": "Task" },
                    "super_class": { "value": "", "display_value": "" },
                    "is_extendable": { "value": "true", "display_value": "true" }
                },
                {
                    "sys_id": { "value": "obj_inc_id", "display_value": "obj_inc_id" },
                    "name": { "value": "incident", "display_value": "incident" },
                    "label": { "value": "Incident", "display_value": "Incident" },
                    "super_class": { "value": "obj_task_id", "display_value": "Task" },
                    "is_extendable": { "value": "true", "display_value": "true" }
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_number"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [
                {
                    "category": { "value": "incident", "display_value": "Incident" },
                    "prefix": { "value": "INC", "display_value": "INC" }
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = client.fetch_table_catalog().await.unwrap();
    assert_eq!(catalog.len(), 2);
    let hierarchy = TableHierarchy::from_descriptors(catalog);
    let inc_desc = hierarchy.get("incident").unwrap();
    assert_eq!(inc_desc.super_class.as_deref(), Some("task"));
    assert_eq!(inc_desc.number_prefix.as_deref(), Some("INC"));
    assert_eq!(
        hierarchy.inheritance_chain("incident"),
        vec!["incident", "task"]
    );

    // 2. Schema merge (`task` + `incident` dictionary, choices, and `sys_ui_list` columns)
    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_dictionary"))
        .and(query_param("sysparm_query", "nameINincident,task^active=true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [
                {
                    "name": { "value": "task" },
                    "element": { "value": "number" },
                    "column_label": { "value": "Number", "display_value": "Number" },
                    "internal_type": { "value": "string" },
                    "max_length": { "value": "40" },
                    "display": { "value": "true" },
                    "read_only": { "value": "true" }
                },
                {
                    "name": { "value": "task" },
                    "element": { "value": "state" },
                    "column_label": { "value": "Task State", "display_value": "Task State" },
                    "internal_type": { "value": "integer" }
                },
                {
                    "name": { "value": "incident" },
                    "element": { "value": "state" },
                    "column_label": { "value": "Incident State", "display_value": "Incident State" },
                    "internal_type": { "value": "integer" },
                    "mandatory": { "value": "true" }
                },
                {
                    "name": { "value": "incident" },
                    "element": { "value": "caller_id" },
                    "column_label": { "value": "Caller", "display_value": "Caller" },
                    "internal_type": { "value": "reference" },
                    "reference": { "value": "sys_user", "display_value": "User" }
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_choice"))
        .and(query_param(
            "sysparm_query",
            "nameINincident,task^inactive=false",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [
                {
                    "name": { "value": "task" },
                    "element": { "value": "state" },
                    "value": { "value": "1" },
                    "label": { "value": "Open", "display_value": "Open" },
                    "sequence": { "value": "1" }
                },
                {
                    "name": { "value": "incident" },
                    "element": { "value": "state" },
                    "value": { "value": "1" },
                    "label": { "value": "New", "display_value": "New" },
                    "sequence": { "value": "10" }
                },
                {
                    "name": { "value": "incident" },
                    "element": { "value": "state" },
                    "value": { "value": "2" },
                    "label": { "value": "In Progress", "display_value": "In Progress" },
                    "sequence": { "value": "20" }
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_ui_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [{
                "sys_id": { "value": "ui_list_inc", "display_value": "ui_list_inc" },
                "name": { "value": "incident", "display_value": "incident" },
                "view": { "value": "", "display_value": "Default view" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/now/table/sys_ui_list_element"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [
                { "element": { "value": "state" }, "position": { "value": "2" } },
                { "element": { "value": "number" }, "position": { "value": "0" } },
                { "element": { "value": "caller_id" }, "position": { "value": "1" } }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let schema = client
        .fetch_table_schema("incident", &hierarchy)
        .await
        .unwrap();
    assert_eq!(schema.display_field, "number");
    assert_eq!(
        schema.default_list_columns,
        vec!["number", "caller_id", "state"]
    );
    // Child `incident.state` overrides `task.state`!
    let state_field = schema.fields.get("state").unwrap();
    assert_eq!(state_field.column_label, "Incident State");
    assert!(state_field.mandatory);
    assert_eq!(state_field.field_type, FieldType::Choice);
    let caller_field = schema.fields.get("caller_id").unwrap();
    assert_eq!(
        caller_field.field_type,
        FieldType::Reference {
            target_table: "sys_user".to_string()
        }
    );
    let state_choices = schema.choices_for("state", None);
    assert_eq!(state_choices.len(), 2);
    assert_eq!(state_choices[0].label, "New");
    assert_eq!(state_choices[1].label, "In Progress");

    // 3. Restricted user ACL fallback: `sys_dictionary`, `sys_choice`, and `sys_ui_list` all return 403!
    let restricted_server = MockServer::start().await;
    let restricted_profile = test_profile(&restricted_server.uri());
    let restricted_store = Arc::new(MemoryCredentialStore::with_credentials(
        restricted_profile.id.clone(),
        valid_creds("tok-restricted"),
    ));
    let restricted_client = SnClient::new(restricted_profile, restricted_store).unwrap();

    for blocked_table in ["sys_dictionary", "sys_choice", "sys_ui_list"] {
        Mock::given(method("GET"))
            .and(path(format!("/api/now/table/{blocked_table}")))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "error": { "message": "User Not Authorized", "detail": "ACL restricted" }
            })))
            .mount(&restricted_server)
            .await;
    }

    Mock::given(method("GET"))
        .and(path("/api/now/table/incident"))
        .and(query_param("sysparm_limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [{
                "sys_id": { "value": "0123456789abcdef0123456789abcdef", "display_value": "0123456789abcdef0123456789abcdef" },
                "number": { "value": "INC0055000", "display_value": "INC0055000" },
                "short_description": { "value": "Email relay delayed", "display_value": "Email relay delayed" },
                "caller_id": { "value": "fedcba9876543210fedcba9876543210", "display_value": "Alan Turing" },
                "active": { "value": "true", "display_value": "true" },
                "sys_updated_on": { "value": "2026-11-11 17:10:00", "display_value": "2026-11-11 12:10:00" }
            }]
        })))
        .expect(1)
        .mount(&restricted_server)
        .await;

    let fallback_schema = restricted_client
        .fetch_table_schema("incident", &hierarchy)
        .await
        .expect("403 on metadata tables should synthesize schema from sample row");
    assert_eq!(fallback_schema.display_field, "number");
    assert!(fallback_schema.fields.contains_key("short_description"));
    assert_eq!(
        fallback_schema.fields.get("active").unwrap().field_type,
        FieldType::Boolean
    );
    assert_eq!(
        fallback_schema.fields.get("caller_id").unwrap().field_type,
        FieldType::Reference {
            target_table: "sys_user".to_string()
        }
    );
    assert!(
        fallback_schema
            .default_list_columns
            .contains(&"number".to_string())
    );
}

#[tokio::test]
async fn port_busy_error_and_create_record() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy_port = occupied.local_addr().unwrap().port();

    let server = MockServer::start().await;
    let mut profile = test_profile(&server.uri());
    profile.callback_port = busy_port;
    let store = Arc::new(MemoryCredentialStore::new());

    let busy_err = OAuthPkceFlow::start_pkce_listener(&profile, None, store.clone(), false, None)
        .await
        .unwrap_err();
    assert!(matches!(busy_err, SnError::OAuth(msg) if msg.contains(&busy_port.to_string())));
    drop(occupied);

    store
        .save_credentials(&profile.id, &valid_creds("create-tok"))
        .unwrap();
    let client = SnClient::new(profile, store).unwrap();

    Mock::given(method("POST"))
        .and(path("/api/now/table/incident"))
        .and(query_param("sysparm_display_value", "all"))
        .and(query_param("sysparm_exclude_reference_link", "true"))
        .and(body_json(json!({ "short_description": "New outage" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "result": {
                "sys_id": { "value": "created_sys_id", "display_value": "created_sys_id" },
                "number": { "value": "INC0077777", "display_value": "INC0077777" },
                "short_description": { "value": "New outage", "display_value": "New outage" }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut payload = serde_json::Map::new();
    payload.insert("short_description".into(), json!("New outage"));
    let created = client.create_record("incident", &payload).await.unwrap();
    assert_eq!(created.sys_id, "created_sys_id");
    assert_eq!(created.get_display("number"), "INC0077777");
}
