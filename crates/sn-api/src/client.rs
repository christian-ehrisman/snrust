//! High-speed ServiceNow REST Table API client with single-flight OAuth refresh.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use sn_core::{
    FieldValue, InstanceProfile, JournalEntry, ListPage, ReferenceCandidate, SnError, SnRecord,
    SnResult,
};
use tokio::sync::Mutex;
use url::Url;

use crate::auth::refresh_access_token;
use crate::keychain::CredentialStore;

/// Pre-emptive token refresh window (seconds before `expires_at`).
const TOKEN_EXPIRY_LEEWAY_SECS: i64 = 60;

/// Query parameters for [`SnClient::list_records`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRecordsParams {
    pub encoded_query: String,
    pub fields: Vec<String>,
    pub offset: usize,
    pub limit: usize,
    pub include_total_count: bool,
}

impl ListRecordsParams {
    pub fn new(limit: usize) -> Self {
        Self {
            encoded_query: String::new(),
            fields: Vec::new(),
            offset: 0,
            limit,
            include_total_count: true,
        }
    }
}

/// Ultra-lightweight 4-field poll payload used by `sn-realtime` to detect record edits
/// before pulling a full record payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordWatermark {
    pub sys_id: String,
    pub sys_updated_on: String,
    pub sys_mod_count: u64,
    pub sys_updated_by: String,
}

struct SnClientInner {
    profile: InstanceProfile,
    http: reqwest::Client,
    cookie_jar: Arc<Jar>,
    cred_store: Arc<dyn CredentialStore>,
    refresh_lock: Mutex<()>,
}

/// Cloneable, thread-safe ServiceNow REST client with HTTP/2 pooling, cookie tracking,
/// and single-flight OAuth 2.0 token refresh.
#[derive(Clone)]
pub struct SnClient {
    inner: Arc<SnClientInner>,
}

impl fmt::Debug for SnClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnClient")
            .field("instance_id", &self.inner.profile.id)
            .field("base_url", &self.inner.profile.base_url)
            .finish()
    }
}

impl SnClient {
    /// Builds a new [`SnClient`] configured with connection pooling, `tcp_nodelay`,
    /// `gzip` + `zstd` compression, HTTP/2 adaptive windowing, and an inspectable cookie jar.
    pub fn new(profile: InstanceProfile, cred_store: Arc<dyn CredentialStore>) -> SnResult<Self> {
        let cookie_jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .cookie_provider(Arc::clone(&cookie_jar))
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Some(Duration::from_secs(30)))
            .gzip(true)
            .zstd(true)
            .user_agent(concat!("snrust/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| SnError::Network(format!("Failed building HTTP client: {err}")))?;

        Ok(Self::with_http_client(
            profile, cred_store, http, cookie_jar,
        ))
    }

    /// Constructs a [`SnClient`] from a pre-configured `reqwest::Client` and cookie `Jar`.
    pub fn with_http_client(
        profile: InstanceProfile,
        cred_store: Arc<dyn CredentialStore>,
        http: reqwest::Client,
        cookie_jar: Arc<Jar>,
    ) -> Self {
        Self {
            inner: Arc::new(SnClientInner {
                profile,
                http,
                cookie_jar,
                cred_store,
                refresh_lock: Mutex::new(()),
            }),
        }
    }

    pub fn profile(&self) -> &InstanceProfile {
        &self.inner.profile
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub fn cookie_jar(&self) -> &Arc<Jar> {
        &self.inner.cookie_jar
    }

    pub fn credential_store(&self) -> &Arc<dyn CredentialStore> {
        &self.inner.cred_store
    }

    /// Returns a fresh OAuth Bearer token alongside any session `Cookie` header value
    /// (`glide_user_route`, `JSESSIONID`, etc.) needed for ServiceNow `/amb` WebSocket handshakes.
    pub async fn bearer_token_and_cookies(&self) -> SnResult<(String, Option<String>)> {
        let token = self.ensure_valid_token(None).await?;
        let url = Url::parse(&self.inner.profile.base_url)
            .map_err(|err| SnError::InvalidConfig(format!("Invalid instance base_url: {err}")))?;
        let cookies = self
            .inner
            .cookie_jar
            .cookies(&url)
            .and_then(|hv| hv.to_str().ok().map(str::to_string))
            .filter(|s| !s.trim().is_empty());
        Ok((token, cookies))
    }

    /// Ensures a valid access token is available, coordinating concurrent callers through
    /// a single-flight mutex so N parallel requests never stampede `/oauth_token.do`.
    ///
    /// When `rejected_token` is `Some(tok)` (after an HTTP `401`), refreshes only if the stored
    /// token is still equal to `tok`.
    async fn ensure_valid_token(&self, rejected_token: Option<&str>) -> SnResult<String> {
        if rejected_token.is_none() {
            let current = self
                .inner
                .cred_store
                .get_credentials(&self.inner.profile.id)?
                .ok_or_else(|| SnError::AuthRequired(self.inner.profile.name.clone()))?;
            if !current.is_expired_or_expiring(TOKEN_EXPIRY_LEEWAY_SECS) {
                return Ok(current.access_token);
            }
        }

        let _guard = self.inner.refresh_lock.lock().await;
        let mut current = self
            .inner
            .cred_store
            .get_credentials(&self.inner.profile.id)?
            .ok_or_else(|| SnError::AuthRequired(self.inner.profile.name.clone()))?;

        match rejected_token {
            None if !current.is_expired_or_expiring(TOKEN_EXPIRY_LEEWAY_SECS) => {
                return Ok(current.access_token);
            }
            Some(bad_tok)
                if current.access_token != bad_tok
                    && !current.is_expired_or_expiring(TOKEN_EXPIRY_LEEWAY_SECS) =>
            {
                // Another concurrent caller already rotated the rejected token.
                return Ok(current.access_token);
            }
            _ => {}
        }

        if current.client_secret.is_none() {
            current.client_secret = self
                .inner
                .cred_store
                .get_client_secret(&self.inner.profile.id)?;
        }

        let refreshed =
            refresh_access_token(&self.inner.http, &self.inner.profile, &current).await?;
        self.inner
            .cred_store
            .save_credentials(&self.inner.profile.id, &refreshed)?;
        Ok(refreshed.access_token)
    }

    async fn execute_authenticated(
        &self,
        method: Method,
        url: &str,
        query_params: &[(&str, String)],
        json_body: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> SnResult<(reqwest::header::HeaderMap, String)> {
        let mut token = self.ensure_valid_token(None).await?;

        for attempt in 0..2 {
            let mut req = self
                .inner
                .http
                .request(method.clone(), url)
                .header(reqwest::header::ACCEPT, "application/json")
                .bearer_auth(&token)
                .query(query_params);

            if let Some(body) = json_body {
                req = req.json(body);
            }

            let resp = req.send().await.map_err(|err| {
                SnError::Network(format!("ServiceNow HTTP request failed: {err}"))
            })?;

            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                tracing::debug!(
                    instance = %self.inner.profile.name,
                    url,
                    "Received 401 Unauthorized; attempting single-flight OAuth token refresh"
                );
                token = self.ensure_valid_token(Some(&token)).await?;
                continue;
            }

            let status = resp.status();
            let headers = resp.headers().clone();
            let body = resp.text().await.map_err(|err| {
                SnError::Network(format!("Failed reading ServiceNow response body: {err}"))
            })?;

            if !status.is_success() {
                return Err(parse_sn_api_error(status, &body));
            }

            return Ok((headers, body));
        }

        Err(SnError::AuthRequired(self.inner.profile.name.clone()))
    }

    fn table_url(&self, table: &str) -> String {
        format!("{}/api/now/table/{}", self.inner.profile.base_url, table)
    }

    fn record_url(&self, table: &str, sys_id: &str) -> String {
        format!(
            "{}/api/now/table/{}/{}",
            self.inner.profile.base_url, table, sys_id
        )
    }

    /// Internal helper returning raw `FieldValue` maps from `/api/now/table/{table}`.
    ///
    /// Automatically injects `sysparm_display_value=all` and `sysparm_exclude_reference_link=true`.
    pub(crate) async fn get_raw_table_rows(
        &self,
        table: &str,
        extra_params: &[(&str, String)],
    ) -> SnResult<Vec<IndexMap<String, FieldValue>>> {
        let mut params = Vec::with_capacity(extra_params.len() + 2);
        params.push(("sysparm_display_value", "all".to_string()));
        params.push(("sysparm_exclude_reference_link", "true".to_string()));
        params.extend_from_slice(extra_params);

        let (_headers, body) = self
            .execute_authenticated(Method::GET, &self.table_url(table), &params, None)
            .await?;

        let envelope: TableListEnvelope = serde_json::from_str(&body)?;
        Ok(envelope.result)
    }

    /// Queries a paginated page of records from `/api/now/table/{table}`.
    pub async fn list_records(&self, table: &str, query: &ListRecordsParams) -> SnResult<ListPage> {
        let is_partial = !query.fields.is_empty();
        let mut params: Vec<(&str, String)> = vec![
            ("sysparm_display_value", "all".to_string()),
            ("sysparm_exclude_reference_link", "true".to_string()),
            ("sysparm_offset", query.offset.to_string()),
            ("sysparm_limit", query.limit.to_string()),
        ];

        if !query.encoded_query.is_empty() {
            params.push(("sysparm_query", query.encoded_query.clone()));
        }
        if !query.include_total_count {
            params.push(("sysparm_no_count", "true".to_string()));
        }
        if is_partial {
            let mut fields = query.fields.clone();
            for mandatory in ["sys_id", "sys_updated_on", "sys_mod_count"] {
                if !fields.iter().any(|f| f == mandatory) {
                    fields.push(mandatory.to_string());
                }
            }
            params.push(("sysparm_fields", fields.join(",")));
        }

        let (headers, body) = self
            .execute_authenticated(Method::GET, &self.table_url(table), &params, None)
            .await?;

        let total_count = headers
            .get("x-total-count")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<usize>().ok());

        let envelope: TableListEnvelope = serde_json::from_str(&body)?;
        let records: Vec<SnRecord> = envelope
            .result
            .into_iter()
            .map(|fields| SnRecord::from_field_map(table, fields, is_partial))
            .collect();

        let max_updated_on = records
            .iter()
            .filter_map(|r| r.sys_updated_on.as_deref())
            .filter(|s| !s.is_empty())
            .max()
            .map(str::to_string);

        Ok(ListPage {
            table: table.to_string(),
            encoded_query: query.encoded_query.clone(),
            offset: query.offset,
            limit: query.limit,
            total_count,
            records,
            max_updated_on,
            from_cache: false,
        })
    }

    /// Fetches a complete single record (`is_partial = false`) by `sys_id`.
    pub async fn get_record(&self, table: &str, sys_id: &str) -> SnResult<SnRecord> {
        let params = [
            ("sysparm_display_value", "all".to_string()),
            ("sysparm_exclude_reference_link", "true".to_string()),
        ];
        let (_headers, body) = self
            .execute_authenticated(Method::GET, &self.record_url(table, sys_id), &params, None)
            .await?;
        let envelope: SingleRecordEnvelope = serde_json::from_str(&body)?;
        Ok(SnRecord::from_field_map(table, envelope.result, false))
    }

    /// Performs a minimal 4-field watermark GET (`sys_id`, `sys_updated_on`, `sys_mod_count`,
    /// `sys_updated_by`) for low-overhead live change detection.
    pub async fn get_record_watermark(
        &self,
        table: &str,
        sys_id: &str,
    ) -> SnResult<RecordWatermark> {
        let params = [
            ("sysparm_display_value", "all".to_string()),
            ("sysparm_exclude_reference_link", "true".to_string()),
            (
                "sysparm_fields",
                "sys_id,sys_updated_on,sys_mod_count,sys_updated_by".to_string(),
            ),
        ];
        let (_headers, body) = self
            .execute_authenticated(Method::GET, &self.record_url(table, sys_id), &params, None)
            .await?;
        let envelope: SingleRecordEnvelope = serde_json::from_str(&body)?;
        let fields = envelope.result;

        let resolved_id = fields
            .get("sys_id")
            .map(|f| f.value.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| sys_id.to_string());
        let sys_updated_on = fields
            .get("sys_updated_on")
            .map(|f| f.value.clone())
            .unwrap_or_default();
        let sys_mod_count = fields
            .get("sys_mod_count")
            .and_then(|f| f.value.parse::<u64>().ok())
            .unwrap_or(0);
        let sys_updated_by = fields
            .get("sys_updated_by")
            .map(|f| {
                if !f.value.is_empty() {
                    f.value.clone()
                } else {
                    f.display_value.clone()
                }
            })
            .unwrap_or_default();

        Ok(RecordWatermark {
            sys_id: resolved_id,
            sys_updated_on,
            sys_mod_count,
            sys_updated_by,
        })
    }

    /// Finds the first matching full record (`limit=1`, `is_partial = false`), e.g. `number=INC0010001`.
    pub async fn find_record_by_query(
        &self,
        table: &str,
        encoded_query: &str,
    ) -> SnResult<Option<SnRecord>> {
        let rows = self
            .get_raw_table_rows(
                table,
                &[
                    ("sysparm_query", encoded_query.to_string()),
                    ("sysparm_limit", "1".to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        Ok(rows
            .into_iter()
            .next()
            .map(|fields| SnRecord::from_field_map(table, fields, false)))
    }

    /// Updates dirty fields on an existing record via `PATCH` and returns the updated full [`SnRecord`].
    pub async fn patch_record(
        &self,
        table: &str,
        sys_id: &str,
        payload: &serde_json::Map<String, serde_json::Value>,
    ) -> SnResult<SnRecord> {
        let params = [
            ("sysparm_display_value", "all".to_string()),
            ("sysparm_exclude_reference_link", "true".to_string()),
        ];
        let (_headers, body) = self
            .execute_authenticated(
                Method::PATCH,
                &self.record_url(table, sys_id),
                &params,
                Some(payload),
            )
            .await?;
        let envelope: SingleRecordEnvelope = serde_json::from_str(&body)?;
        Ok(SnRecord::from_field_map(table, envelope.result, false))
    }

    /// Inserts a new record via `POST` and returns the created full [`SnRecord`].
    pub async fn create_record(
        &self,
        table: &str,
        payload: &serde_json::Map<String, serde_json::Value>,
    ) -> SnResult<SnRecord> {
        let params = [
            ("sysparm_display_value", "all".to_string()),
            ("sysparm_exclude_reference_link", "true".to_string()),
        ];
        let (_headers, body) = self
            .execute_authenticated(Method::POST, &self.table_url(table), &params, Some(payload))
            .await?;
        let envelope: SingleRecordEnvelope = serde_json::from_str(&body)?;
        Ok(SnRecord::from_field_map(table, envelope.result, false))
    }

    /// Deletes a record by `sys_id`.
    pub async fn delete_record(&self, table: &str, sys_id: &str) -> SnResult<()> {
        let _ = self
            .execute_authenticated(Method::DELETE, &self.record_url(table, sys_id), &[], None)
            .await?;
        Ok(())
    }

    /// Fetches Activity Stream journal entries (`work_notes`, `comments`) from `sys_journal_field`
    /// ordered newest-first (`^ORDERBYDESCsys_created_on`).
    pub async fn get_journal_entries(
        &self,
        record_sys_id: &str,
        created_after: Option<&str>,
        limit: usize,
    ) -> SnResult<Vec<JournalEntry>> {
        let mut query = format!("element_id={record_sys_id}");
        if let Some(after) = created_after.map(str::trim).filter(|s| !s.is_empty()) {
            query.push_str(&format!("^sys_created_on>{after}"));
        }
        query.push_str("^ORDERBYDESCsys_created_on");

        let rows = self
            .get_raw_table_rows(
                "sys_journal_field",
                &[
                    ("sysparm_query", query),
                    (
                        "sysparm_fields",
                        "sys_id,element,element_id,value,sys_created_on,sys_created_by".to_string(),
                    ),
                    ("sysparm_limit", limit.max(1).to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| JournalEntry {
                sys_id: row
                    .get("sys_id")
                    .map(|f| f.value.clone())
                    .unwrap_or_default(),
                element: row
                    .get("element")
                    .map(|f| f.value.clone())
                    .unwrap_or_default(),
                element_id: row
                    .get("element_id")
                    .map(|f| f.value.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| record_sys_id.to_string()),
                value: row
                    .get("value")
                    .map(|f| f.effective_display().to_string())
                    .unwrap_or_default(),
                sys_created_on: row
                    .get("sys_created_on")
                    .map(|f| {
                        if !f.value.is_empty() {
                            f.value.clone()
                        } else {
                            f.display_value.clone()
                        }
                    })
                    .unwrap_or_default(),
                sys_created_by: row
                    .get("sys_created_by")
                    .map(|f| f.effective_display().to_string())
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Performs a typeahead search against a reference target table (`sys_user`, `cmdb_ci`, etc.).
    pub async fn search_reference(
        &self,
        target_table: &str,
        display_field: &str,
        search_term: &str,
        secondary_field: Option<&str>,
        limit: usize,
    ) -> SnResult<Vec<ReferenceCandidate>> {
        let disp_col = {
            let trimmed = display_field.trim();
            if trimmed.is_empty() { "name" } else { trimmed }
        };
        let sec_col = secondary_field
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != disp_col);

        let needle = search_term.trim();
        let query = if needle.is_empty() {
            format!("ORDERBY{disp_col}")
        } else if let Some(sec) = sec_col {
            format!("{disp_col}LIKE{needle}^OR{sec}LIKE{needle}^ORDERBY{disp_col}")
        } else {
            format!("{disp_col}LIKE{needle}^ORDERBY{disp_col}")
        };

        let mut fields = vec!["sys_id", disp_col];
        if let Some(sec) = sec_col {
            fields.push(sec);
        }

        let rows = self
            .get_raw_table_rows(
                target_table,
                &[
                    ("sysparm_query", query),
                    ("sysparm_fields", fields.join(",")),
                    ("sysparm_limit", limit.max(1).to_string()),
                    ("sysparm_no_count", "true".to_string()),
                ],
            )
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let sys_id = row
                    .get("sys_id")
                    .map(|f| f.value.clone())
                    .unwrap_or_default();
                if sys_id.is_empty() {
                    return None;
                }
                let display_value = row
                    .get(disp_col)
                    .map(|f| f.effective_display().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| sys_id.clone());
                let secondary = sec_col
                    .and_then(|sec| row.get(sec))
                    .map(|f| f.effective_display().to_string())
                    .filter(|s| !s.is_empty());
                Some(ReferenceCandidate {
                    target_table: target_table.to_string(),
                    sys_id,
                    display_value,
                    secondary,
                })
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct TableListEnvelope {
    #[serde(default)]
    result: Vec<IndexMap<String, FieldValue>>,
}

#[derive(Deserialize)]
struct SingleRecordEnvelope {
    #[serde(default)]
    result: IndexMap<String, FieldValue>,
}

#[derive(Deserialize)]
struct SnErrorEnvelope {
    error: Option<SnErrorDetail>,
}

#[derive(Deserialize)]
struct SnErrorDetail {
    message: Option<String>,
    detail: Option<String>,
}

fn parse_sn_api_error(status: StatusCode, body: &str) -> SnError {
    if let Ok(parsed) = serde_json::from_str::<SnErrorEnvelope>(body)
        && let Some(err) = parsed.error
    {
        let message = err
            .message
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                status
                    .canonical_reason()
                    .unwrap_or("ServiceNow API error")
                    .to_string()
            });
        let detail = err.detail.filter(|s| !s.trim().is_empty());
        return SnError::Api {
            status: status.as_u16(),
            message,
            detail,
        };
    }

    let fallback = status
        .canonical_reason()
        .unwrap_or("ServiceNow API error")
        .to_string();
    let detail = {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.chars().take(512).collect())
        }
    };
    SnError::Api {
        status: status.as_u16(),
        message: fallback,
        detail,
    }
}
