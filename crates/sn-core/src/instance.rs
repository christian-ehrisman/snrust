//! ServiceNow instance connection profile and OAuth endpoint derivation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::error::{SnError, SnResult};

/// Default loopback port used for OAuth 2.0 Authorization Code + PKCE callbacks.
///
/// ServiceNow's `oauth_entity` Redirect URL requires an exact port match by default,
/// so keeping a deterministic default (`http://127.0.0.1:8910/oauth/callback`) lets users
/// copy and register the Redirect URL once in System OAuth -> Application Registry.
pub const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 8910;

/// Unique identifier for a configured ServiceNow instance profile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(pub String);

impl InstanceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for InstanceId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for InstanceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Non-secret metadata for a ServiceNow instance connection.
///
/// Secrets (`access_token`, `refresh_token`, and optional `client_secret`) are stored
/// exclusively in macOS Keychain keyed by [`InstanceProfile::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceProfile {
    pub id: InstanceId,
    pub name: String,
    pub base_url: String,
    pub client_id: String,
    pub has_client_secret: bool,
    pub callback_port: u16,
    pub scopes: String,
    /// Hex color badge (e.g. `#34D399` for dev, `#F87171` for prod) shown in the titlebar
    /// to prevent accidental edits on production instances.
    pub accent_hex: String,
    pub created_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
}

impl InstanceProfile {
    pub fn new(
        name: impl Into<String>,
        instance_or_url: &str,
        client_id: impl Into<String>,
        has_client_secret: bool,
    ) -> SnResult<Self> {
        let base_url = Self::normalize_instance_url(instance_or_url)?;
        let name_str = name.into();
        let resolved_name = if name_str.trim().is_empty() {
            Self::host_label(&base_url)
        } else {
            name_str.trim().to_string()
        };

        Ok(Self {
            id: InstanceId::new(),
            name: resolved_name,
            base_url,
            client_id: client_id.into().trim().to_string(),
            has_client_secret,
            callback_port: DEFAULT_OAUTH_CALLBACK_PORT,
            scopes: "useraccount".to_string(),
            accent_hex: "#38BDF8".to_string(),
            created_at: Utc::now(),
            last_connected_at: None,
        })
    }

    /// Normalizes shorthand inputs like `dev12345` or `acme.service-now.com/` into
    /// a canonical `https://<host>` origin without trailing slashes.
    pub fn normalize_instance_url(raw: &str) -> SnResult<String> {
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(SnError::InvalidConfig(
                "Instance URL or name cannot be empty".to_string(),
            ));
        }

        let candidate = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else if trimmed.contains('.')
            || trimmed.starts_with("localhost")
            || trimmed.starts_with("127.0.0.1")
        {
            let scheme = if trimmed.starts_with("localhost") || trimmed.starts_with("127.0.0.1") {
                "http"
            } else {
                "https"
            };
            format!("{scheme}://{trimmed}")
        } else {
            format!("https://{trimmed}.service-now.com")
        };

        let parsed = Url::parse(&candidate)
            .map_err(|e| SnError::InvalidConfig(format!("Invalid instance URL '{raw}': {e}")))?;

        let host = parsed.host_str().ok_or_else(|| {
            SnError::InvalidConfig(format!("Instance URL '{raw}' is missing a host"))
        })?;

        let port_suffix = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();

        Ok(format!("{}://{}{}", parsed.scheme(), host, port_suffix))
    }

    pub fn host_label(base_url: &str) -> String {
        Url::parse(base_url)
            .ok()
            .and_then(|u| {
                u.host_str()
                    .map(|h| h.trim_end_matches(".service-now.com").to_string())
            })
            .unwrap_or_else(|| base_url.to_string())
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/oauth/callback", self.callback_port)
    }

    pub fn authorize_endpoint(&self) -> String {
        format!("{}/oauth_auth.do", self.base_url)
    }

    pub fn token_endpoint(&self) -> String {
        format!("{}/oauth_token.do", self.base_url)
    }

    /// ServiceNow Asynchronous Message Bus (CometD over WebSocket) endpoint.
    pub fn amb_ws_url(&self) -> SnResult<String> {
        let mut parsed = Url::parse(&self.base_url)
            .map_err(|e| SnError::InvalidConfig(format!("Invalid base URL: {e}")))?;
        let ws_scheme = if parsed.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        parsed
            .set_scheme(ws_scheme)
            .map_err(|_| SnError::InvalidConfig("Cannot switch URL scheme to WebSocket".into()))?;
        parsed.set_path("/amb");
        Ok(parsed.to_string())
    }

    /// Browser deep-link to view a record directly in classic ServiceNow UI.
    pub fn browser_record_url(&self, table: &str, sys_id: &str) -> String {
        format!("{}/{}.do?sys_id={}", self.base_url, table, sys_id)
    }
}
