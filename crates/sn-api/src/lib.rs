//! OAuth 2.0 PKCE authentication, Keychain token storage, and ServiceNow REST client.
//!
//! Bridges `sn-core` domain models with ServiceNow's Table API, metadata tables
//! (`sys_db_object`, `sys_dictionary`, `sys_choice`, `sys_ui_list`), OAuth 2.0 Authorization
//! Code + PKCE (`S256`) loopback authentication, and macOS Keychain secret storage.

pub mod auth;
pub mod classic;
pub mod client;
pub mod keychain;
pub mod metadata;

pub use auth::{OAuthPkceFlow, PendingOAuthSession, PkceChallenge, refresh_access_token};
pub use classic::FavoriteChange;
pub use client::{ListRecordsParams, RecordWatermark, SnClient, oauth_access_hint};
pub use keychain::{
    CredentialStore, KEYCHAIN_SERVICE, MacKeychainStore, MemoryCredentialStore, StoredCredentials,
};
