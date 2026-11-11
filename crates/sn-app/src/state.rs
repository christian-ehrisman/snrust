//! Shared application services, Tokio runtime bridge, and workspace command bus.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use sn_api::{
    CredentialStore, MacKeychainStore, MemoryCredentialStore, SnClient, StoredCredentials,
};
use sn_cache::SnCache;
use sn_core::{InstanceId, InstanceProfile, SnRecord, SnResult, TableHierarchy, TableSchema};
use sn_realtime::{RealtimeConfig, RealtimeCoordinator};
use tokio::runtime::Handle;
use tracing::warn;

use crate::demo_server::DemoSandboxServer;

/// High-level navigation & UI commands dispatched from child panels (`ListViewPanel`,
/// `FormViewPanel`, `CommandPalette`, `ConnectModal`) up to the `WorkspaceShell`.
#[derive(Debug, Clone)]
pub enum WorkspaceCommand {
    OpenList {
        table: String,
        label: Option<String>,
        encoded_query: String,
        in_split: bool,
    },
    OpenRecord {
        table: String,
        sys_id: String,
        seed_record: Option<SnRecord>,
        in_split: bool,
    },
    OpenNewRecord {
        table: String,
        in_split: bool,
    },
    OpenRecordByNumber {
        table: String,
        number: String,
    },
    SwitchInstance {
        profile: InstanceProfile,
    },
    InstanceDeleted {
        instance_id: InstanceId,
    },
    ShowInstanceModal,
    RecordSaved {
        record: SnRecord,
        close_after: bool,
    },
    ToastInfo(String),
    ToastSuccess(String),
    ToastError(String),
    RefreshSidebar,
}

/// Composite credential vault that delegates real OAuth instances to macOS Keychain while keeping
/// ephemeral Demo Sandbox tokens in memory (avoiding macOS Keychain authorization prompts for demo mode).
#[derive(Debug)]
pub struct HybridCredentialStore {
    keychain: MacKeychainStore,
    memory: MemoryCredentialStore,
}

impl HybridCredentialStore {
    pub fn new() -> Self {
        Self {
            keychain: MacKeychainStore::new(),
            memory: MemoryCredentialStore::new(),
        }
    }

    pub fn store_ephemeral(
        &self,
        instance_id: &InstanceId,
        creds: &StoredCredentials,
    ) -> SnResult<()> {
        self.memory.save_credentials(instance_id, creds)
    }
}

impl CredentialStore for HybridCredentialStore {
    fn get_credentials(&self, instance_id: &InstanceId) -> SnResult<Option<StoredCredentials>> {
        if let Some(mem) = self.memory.get_credentials(instance_id)? {
            return Ok(Some(mem));
        }
        self.keychain.get_credentials(instance_id)
    }

    fn save_credentials(
        &self,
        instance_id: &InstanceId,
        credentials: &StoredCredentials,
    ) -> SnResult<()> {
        if instance_id.as_str().starts_with("demo-sandbox") {
            return self.memory.save_credentials(instance_id, credentials);
        }
        self.keychain.save_credentials(instance_id, credentials)
    }

    fn delete_credentials(&self, instance_id: &InstanceId) -> SnResult<()> {
        let _ = self.memory.delete_credentials(instance_id);
        if instance_id.as_str().starts_with("demo-sandbox") {
            return Ok(());
        }
        self.keychain.delete_credentials(instance_id)
    }

    fn save_client_secret(&self, instance_id: &InstanceId, client_secret: &str) -> SnResult<()> {
        if instance_id.as_str().starts_with("demo-sandbox") {
            return self.memory.save_client_secret(instance_id, client_secret);
        }
        self.keychain.save_client_secret(instance_id, client_secret)
    }

    fn get_client_secret(&self, instance_id: &InstanceId) -> SnResult<Option<String>> {
        if let Some(sec) = self.memory.get_client_secret(instance_id)? {
            return Ok(Some(sec));
        }
        self.keychain.get_client_secret(instance_id)
    }

    fn delete_client_secret(&self, instance_id: &InstanceId) -> SnResult<()> {
        let _ = self.memory.delete_client_secret(instance_id);
        if instance_id.as_str().starts_with("demo-sandbox") {
            return Ok(());
        }
        self.keychain.delete_client_secret(instance_id)
    }
}

/// Shared backend services handle passed cleanly to all GPUI entities.
#[derive(Clone)]
pub struct AppServices {
    pub tokio_handle: Handle,
    pub cache: SnCache,
    pub cred_store: Arc<HybridCredentialStore>,
    pub realtime: RealtimeCoordinator,
    pub cmd_tx: async_channel::Sender<WorkspaceCommand>,
    clients: Arc<RwLock<HashMap<InstanceId, SnClient>>>,
    demo_server: Arc<RwLock<Option<DemoSandboxServer>>>,
}

impl AppServices {
    pub fn new(
        tokio_handle: Handle,
        cache: SnCache,
    ) -> (Self, async_channel::Receiver<WorkspaceCommand>) {
        let cred_store = Arc::new(HybridCredentialStore::new());
        let realtime = RealtimeCoordinator::new(
            tokio_handle.clone(),
            cache.clone(),
            RealtimeConfig::default(),
        );
        let (cmd_tx, cmd_rx) = async_channel::unbounded();

        let this = Self {
            tokio_handle,
            cache,
            cred_store,
            realtime,
            cmd_tx,
            clients: Arc::new(RwLock::new(HashMap::new())),
            demo_server: Arc::new(RwLock::new(None)),
        };

        (this, cmd_rx)
    }

    pub fn send_cmd(&self, cmd: WorkspaceCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    /// Returns a pooled `SnClient` for `profile`, creating one if needed.
    pub fn client_for(&self, profile: &InstanceProfile) -> SnResult<SnClient> {
        if let Some(existing) = self.clients.read().get(&profile.id)
            && existing.profile().base_url == profile.base_url
            && existing.profile().client_id == profile.client_id
        {
            return Ok(existing.clone());
        }
        let dyn_store: Arc<dyn CredentialStore> = self.cred_store.clone();
        let client = SnClient::new(profile.clone(), dyn_store)?;
        self.clients
            .write()
            .insert(profile.id.clone(), client.clone());
        Ok(client)
    }

    pub fn evict_client(&self, instance_id: &InstanceId) {
        self.clients.write().remove(instance_id);
        self.realtime.shutdown_instance(instance_id);
    }

    /// Resolves `TableSchema` with SWR: returns cached schema immediately if available and
    /// fetches/caches from ServiceNow when missing or `force_refresh` is true.
    pub async fn ensure_table_schema(
        &self,
        client: &SnClient,
        table: &str,
        force_refresh: bool,
    ) -> SnResult<TableSchema> {
        let instance_id = client.profile().id.clone();
        if !force_refresh && let Ok(Some(cached)) = self.cache.get_table_schema(&instance_id, table)
        {
            if cached.is_stale {
                // Fire-and-forget background revalidation!
                let bg_self = self.clone();
                let bg_client = client.clone();
                let bg_table = table.to_string();
                self.tokio_handle.spawn(async move {
                    let _ = bg_self
                        .refresh_table_schema_network(&bg_client, &bg_table)
                        .await;
                });
            }
            return Ok(cached.data);
        }

        self.refresh_table_schema_network(client, table).await
    }

    async fn refresh_table_schema_network(
        &self,
        client: &SnClient,
        table: &str,
    ) -> SnResult<TableSchema> {
        let instance_id = client.profile().id.clone();
        let hierarchy = self.ensure_table_hierarchy(client).await;
        let schema = client.fetch_table_schema(table, &hierarchy).await?;
        if let Err(err) = self
            .cache
            .save_table_schema_async(instance_id, schema.clone())
            .await
        {
            warn!("Failed caching schema for {table}: {err}");
        }
        Ok(schema)
    }

    pub async fn ensure_table_hierarchy(&self, client: &SnClient) -> TableHierarchy {
        let instance_id = client.profile().id.clone();
        if let Ok(existing) = self.cache.get_table_hierarchy(&instance_id)
            && !existing.is_empty()
        {
            return existing;
        }
        match client.fetch_table_catalog().await {
            Ok(catalog) => {
                let _ = self
                    .cache
                    .save_table_catalog_async(instance_id.clone(), catalog)
                    .await;
                self.cache
                    .get_table_hierarchy(&instance_id)
                    .unwrap_or_default()
            }
            Err(err) => {
                warn!("Catalog fetch fallback: {err}");
                TableHierarchy::from_descriptors(Vec::new())
            }
        }
    }

    /// Starts (or reuses) the local Demo Sandbox server and activates its `InstanceProfile`.
    pub async fn launch_demo_sandbox(&self) -> anyhow::Result<InstanceProfile> {
        let server = {
            let existing = self.demo_server.read().clone();
            match existing {
                Some(s) => s,
                None => {
                    let started = DemoSandboxServer::start(&self.tokio_handle).await?;
                    *self.demo_server.write() = Some(started.clone());
                    started
                }
            }
        };

        let mut profile = InstanceProfile::new(
            "Demo Sandbox (Local Live)",
            server.base_url(),
            "snrust_demo_client",
            false,
        )?;
        profile.id = InstanceId::from("demo-sandbox-local");
        profile.accent_hex = "#34D399".to_string();
        profile.last_connected_at = Some(chrono::Utc::now());

        self.cred_store.store_ephemeral(
            &profile.id,
            &StoredCredentials {
                access_token: "demo_sandbox_token".into(),
                refresh_token: Some("demo_sandbox_refresh".into()),
                expires_at: None,
                client_secret: None,
            },
        )?;

        self.cache.upsert_instance(&profile)?;
        self.cache.set_active_instance_id(Some(&profile.id))?;
        self.clients.write().remove(&profile.id);
        Ok(profile)
    }

    pub fn is_demo_instance(&self, instance_id: &InstanceId) -> bool {
        instance_id.as_str().starts_with("demo-sandbox")
    }

    /// Triggers a simulated external server update on `(table, sys_id)` when running in Demo Sandbox mode.
    pub fn trigger_demo_live_push(&self, table: &str, sys_id: &str) -> bool {
        self.demo_server
            .read()
            .as_ref()
            .is_some_and(|srv| srv.simulate_remote_agent_update(table, sys_id))
    }
}
