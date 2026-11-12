//! Unified error types shared across snrust backend and UI crates.

use thiserror::Error;

pub type SnResult<T> = Result<T, SnError>;

#[derive(Debug, Error, Clone)]
pub enum SnError {
    #[error("Authentication required for instance '{0}'")]
    AuthRequired(String),

    #[error("OAuth token exchange or refresh failed: {0}")]
    OAuth(String),

    #[error("Keychain operation failed: {0}")]
    Keychain(String),

    #[error("ServiceNow HTTP {status}: {message}{}", detail_suffix(message, detail.as_deref()))]
    Api {
        status: u16,
        message: String,
        detail: Option<String>,
    },

    #[error("Network transport error: {0}")]
    Network(String),

    #[error("Local cache error: {0}")]
    Cache(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Record conflict on {sys_id} (server mod count {server_mod_count})")]
    Conflict {
        sys_id: String,
        server_mod_count: u64,
    },

    #[error("Real-time bus error: {0}")]
    Realtime(String),
}

/// Renders ServiceNow's `error.detail` (often the only text naming the real cause, e.g. an
/// OAuth scope or ACL denial) as a short ` (detail)` suffix. HTML error pages and details that
/// merely repeat `message` are omitted so toasts stay readable.
fn detail_suffix(message: &str, detail: Option<&str>) -> String {
    const MAX_CHARS: usize = 200;
    let Some(raw) = detail else {
        return String::new();
    };
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.starts_with('<') || collapsed == message.trim() {
        return String::new();
    }
    let mut shown: String = collapsed.chars().take(MAX_CHARS).collect();
    if collapsed.chars().count() > MAX_CHARS {
        shown.push('…');
    }
    format!(" ({shown})")
}

impl From<serde_json::Error> for SnError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::SnError;

    fn api(message: &str, detail: Option<&str>) -> SnError {
        SnError::Api {
            status: 403,
            message: message.to_string(),
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn api_error_display_includes_servicenow_detail() {
        assert_eq!(
            api("User Not Authorized", Some("Not permitted")).to_string(),
            "ServiceNow HTTP 403: User Not Authorized (Not permitted)"
        );
        assert_eq!(
            api("Forbidden", None).to_string(),
            "ServiceNow HTTP 403: Forbidden"
        );
        assert_eq!(
            api("Forbidden", Some("<html><body>Denied</body></html>")).to_string(),
            "ServiceNow HTTP 403: Forbidden"
        );
        assert_eq!(
            api("Forbidden", Some("Forbidden")).to_string(),
            "ServiceNow HTTP 403: Forbidden"
        );
        let long = api("Forbidden", Some(&"x".repeat(500))).to_string();
        assert!(long.ends_with("…)"));
        assert!(long.chars().count() < 260);
    }
}
