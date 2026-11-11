//! Secure credential persistence backed by macOS Keychain (`security-framework`)
//! and an in-memory store for headless environments and integration tests.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use parking_lot::RwLock;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use serde::{Deserialize, Serialize};
use sn_core::{InstanceId, SnError, SnResult};

/// macOS Keychain service identifier used for all snrust OAuth credentials.
pub const KEYCHAIN_SERVICE: &str = "dev.snrust.oauth";

/// macOS Security.framework `errSecItemNotFound` OSStatus code.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// Persisted OAuth token bundle and optional confidential client secret for one instance.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub client_secret: Option<String>,
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredentials")
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    "<empty>"
                } else {
                    "[REDACTED]"
                },
            )
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("expires_at", &self.expires_at)
            .field("has_client_secret", &self.client_secret.is_some())
            .finish()
    }
}

impl StoredCredentials {
    /// Returns `true` if the access token is empty or expires within `within_secs` seconds.
    pub fn is_expired_or_expiring(&self, within_secs: i64) -> bool {
        if self.access_token.trim().is_empty() {
            return true;
        }
        match self.expires_at {
            Some(expires_at) => Utc::now() + Duration::seconds(within_secs) >= expires_at,
            None => false,
        }
    }
}

/// Pluggable secret storage for per-instance ServiceNow OAuth tokens and client secrets.
pub trait CredentialStore: Send + Sync + fmt::Debug {
    /// Loads persisted OAuth credentials for `instance_id`, returning `Ok(None)` if not signed in.
    fn get_credentials(&self, instance_id: &InstanceId) -> SnResult<Option<StoredCredentials>>;

    /// Persists OAuth credentials for `instance_id`, preserving any pre-configured `client_secret`
    /// when `creds.client_secret` is `None`.
    fn save_credentials(&self, instance_id: &InstanceId, creds: &StoredCredentials)
    -> SnResult<()>;

    /// Deletes stored OAuth tokens for `instance_id`.
    fn delete_credentials(&self, instance_id: &InstanceId) -> SnResult<()>;

    /// Saves a confidential OAuth `client_secret` prior to (or independent of) interactive login.
    fn save_client_secret(&self, instance_id: &InstanceId, client_secret: &str) -> SnResult<()>;

    /// Retrieves a previously saved confidential OAuth `client_secret` for `instance_id`.
    fn get_client_secret(&self, instance_id: &InstanceId) -> SnResult<Option<String>>;

    /// Deletes any separately persisted `client_secret` for `instance_id`.
    fn delete_client_secret(&self, instance_id: &InstanceId) -> SnResult<()>;
}

/// macOS Keychain implementation storing JSON-encoded [`StoredCredentials`] under
/// service `"dev.snrust.oauth"`.
#[derive(Debug, Clone)]
pub struct MacKeychainStore {
    service: String,
}

impl Default for MacKeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MacKeychainStore {
    pub fn new() -> Self {
        Self {
            service: KEYCHAIN_SERVICE.to_string(),
        }
    }

    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn secret_account(instance_id: &InstanceId) -> String {
        format!("{}:client_secret", instance_id.as_str())
    }

    fn read_raw(&self, account: &str) -> SnResult<Option<Vec<u8>>> {
        match get_generic_password(&self.service, account) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(SnError::Keychain(format!(
                "Failed reading Keychain entry '{account}': {err}"
            ))),
        }
    }

    fn write_raw(&self, account: &str, payload: &[u8]) -> SnResult<()> {
        set_generic_password(&self.service, account, payload).map_err(|err| {
            SnError::Keychain(format!("Failed writing Keychain entry '{account}': {err}"))
        })
    }

    fn delete_raw(&self, account: &str) -> SnResult<()> {
        match delete_generic_password(&self.service, account) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(err) => Err(SnError::Keychain(format!(
                "Failed deleting Keychain entry '{account}': {err}"
            ))),
        }
    }
}

impl CredentialStore for MacKeychainStore {
    fn get_credentials(&self, instance_id: &InstanceId) -> SnResult<Option<StoredCredentials>> {
        let Some(bytes) = self.read_raw(instance_id.as_str())? else {
            return Ok(None);
        };
        let mut creds: StoredCredentials = serde_json::from_slice(&bytes).map_err(|err| {
            SnError::Keychain(format!("Corrupt Keychain JSON for '{instance_id}': {err}"))
        })?;
        if creds.client_secret.is_none() {
            creds.client_secret = self.get_client_secret(instance_id)?;
        }
        Ok(Some(creds))
    }

    fn save_credentials(
        &self,
        instance_id: &InstanceId,
        creds: &StoredCredentials,
    ) -> SnResult<()> {
        let mut merged = creds.clone();
        if let Some(secret) = merged.client_secret.as_deref().filter(|s| !s.is_empty()) {
            self.write_raw(&Self::secret_account(instance_id), secret.as_bytes())?;
        } else {
            merged.client_secret = self.get_client_secret(instance_id)?;
        }
        let encoded = serde_json::to_vec(&merged)?;
        self.write_raw(instance_id.as_str(), &encoded)
    }

    fn delete_credentials(&self, instance_id: &InstanceId) -> SnResult<()> {
        self.delete_raw(instance_id.as_str())
    }

    fn save_client_secret(&self, instance_id: &InstanceId, client_secret: &str) -> SnResult<()> {
        let trimmed = client_secret.trim();
        if trimmed.is_empty() {
            return self.delete_client_secret(instance_id);
        }
        self.write_raw(&Self::secret_account(instance_id), trimmed.as_bytes())?;

        if let Some(bytes) = self.read_raw(instance_id.as_str())?
            && let Ok(mut existing) = serde_json::from_slice::<StoredCredentials>(&bytes)
        {
            existing.client_secret = Some(trimmed.to_string());
            let encoded = serde_json::to_vec(&existing)?;
            self.write_raw(instance_id.as_str(), &encoded)?;
        }
        Ok(())
    }

    fn get_client_secret(&self, instance_id: &InstanceId) -> SnResult<Option<String>> {
        if let Some(bytes) = self.read_raw(&Self::secret_account(instance_id))? {
            let secret = String::from_utf8(bytes).map_err(|err| {
                SnError::Keychain(format!("Invalid UTF-8 in stored client_secret: {err}"))
            })?;
            if !secret.is_empty() {
                return Ok(Some(secret));
            }
        }
        if let Some(bytes) = self.read_raw(instance_id.as_str())?
            && let Ok(creds) = serde_json::from_slice::<StoredCredentials>(&bytes)
        {
            return Ok(creds.client_secret.filter(|s| !s.is_empty()));
        }
        Ok(None)
    }

    fn delete_client_secret(&self, instance_id: &InstanceId) -> SnResult<()> {
        self.delete_raw(&Self::secret_account(instance_id))
    }
}

/// Thread-safe in-memory [`CredentialStore`] for integration tests and headless sessions.
#[derive(Debug, Clone, Default)]
pub struct MemoryCredentialStore {
    credentials: Arc<RwLock<HashMap<InstanceId, StoredCredentials>>>,
    client_secrets: Arc<RwLock<HashMap<InstanceId, String>>>,
}

impl MemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a store pre-seeded with `creds` for `instance_id`.
    pub fn with_credentials(instance_id: InstanceId, creds: StoredCredentials) -> Self {
        let store = Self::new();
        let _ = store.save_credentials(&instance_id, &creds);
        store
    }

    /// Direct handle to the backing `RwLock<HashMap<InstanceId, StoredCredentials>>`.
    pub fn inner(&self) -> &Arc<RwLock<HashMap<InstanceId, StoredCredentials>>> {
        &self.credentials
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn get_credentials(&self, instance_id: &InstanceId) -> SnResult<Option<StoredCredentials>> {
        let mut creds = self.credentials.read().get(instance_id).cloned();
        if let Some(ref mut c) = creds
            && c.client_secret.is_none()
        {
            c.client_secret = self.client_secrets.read().get(instance_id).cloned();
        }
        Ok(creds)
    }

    fn save_credentials(
        &self,
        instance_id: &InstanceId,
        creds: &StoredCredentials,
    ) -> SnResult<()> {
        let mut merged = creds.clone();
        if let Some(secret) = merged.client_secret.as_deref().filter(|s| !s.is_empty()) {
            self.client_secrets
                .write()
                .insert(instance_id.clone(), secret.to_string());
        } else if let Some(existing_secret) = self.client_secrets.read().get(instance_id).cloned() {
            merged.client_secret = Some(existing_secret);
        }
        self.credentials.write().insert(instance_id.clone(), merged);
        Ok(())
    }

    fn delete_credentials(&self, instance_id: &InstanceId) -> SnResult<()> {
        self.credentials.write().remove(instance_id);
        Ok(())
    }

    fn save_client_secret(&self, instance_id: &InstanceId, client_secret: &str) -> SnResult<()> {
        let trimmed = client_secret.trim();
        if trimmed.is_empty() {
            return self.delete_client_secret(instance_id);
        }
        self.client_secrets
            .write()
            .insert(instance_id.clone(), trimmed.to_string());
        if let Some(existing) = self.credentials.write().get_mut(instance_id) {
            existing.client_secret = Some(trimmed.to_string());
        }
        Ok(())
    }

    fn get_client_secret(&self, instance_id: &InstanceId) -> SnResult<Option<String>> {
        if let Some(secret) = self.client_secrets.read().get(instance_id).cloned()
            && !secret.is_empty()
        {
            return Ok(Some(secret));
        }
        Ok(self
            .credentials
            .read()
            .get(instance_id)
            .and_then(|c| c.client_secret.clone())
            .filter(|s| !s.is_empty()))
    }

    fn delete_client_secret(&self, instance_id: &InstanceId) -> SnResult<()> {
        self.client_secrets.write().remove(instance_id);
        if let Some(existing) = self.credentials.write().get_mut(instance_id) {
            existing.client_secret = None;
        }
        Ok(())
    }
}
