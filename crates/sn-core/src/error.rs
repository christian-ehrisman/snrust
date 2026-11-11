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

    #[error("ServiceNow HTTP {status}: {message}")]
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

impl From<serde_json::Error> for SnError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}
