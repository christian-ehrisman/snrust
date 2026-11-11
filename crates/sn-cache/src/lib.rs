//! Multi-tier SQLite WAL + hot in-memory `moka` LRU cache for `snrust`.
//!
//! ### Cache Tiers
//! - **Tier 1 (Metadata & Dictionary)**: `sys_db_object` table catalog with FTS5 search,
//!   [`TableHierarchy`] graph, and merged [`TableSchema`] definitions.
//! - **Tier 2 (Normalized Records & SWR Lists)**: Single-source-of-truth [`SnRecord`] store with
//!   partial-safe field merging (`SnRecord::merge_from`), paginated [`ListPage`] query index,
//!   `sys_journal_field` streams, and FTS5 [`ReferenceCandidate`] typeahead index.
//! - **Tier 3 (Workspace & Layouts)**: Non-secret [`sn_core::InstanceProfile`]s, active instance selection,
//!   sidebar favorites, 50-item recent history, rearrangeable [`FormGridLayout`]s, and custom list columns.

mod schema_sql;
mod tier1;
mod tier2;
mod tier3;
mod types;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use moka::sync::Cache;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, params};
use sn_core::{
    FormGridLayout, InstanceId, JournalEntry, ListPage, ReferenceCandidate, SnError, SnRecord,
    SnResult, TableDescriptor, TableHierarchy, TableSchema,
};

use crate::schema_sql::{cache_err, initialize_database};
use crate::tier2::compute_db_bytes;

pub use crate::types::{
    CacheStats, CacheTtlConfig, CachedItem, FavoriteItem, FavoriteKind, RecentItem,
};

const DEFAULT_DB_FILENAME: &str = "snrust_cache.db";

pub(crate) struct SnCacheInner {
    pub(crate) conn: Mutex<Connection>,
    pub(crate) db_path: Option<PathBuf>,
    pub(crate) ttls: RwLock<CacheTtlConfig>,
    pub(crate) schema_cache: Cache<(InstanceId, String), (TableSchema, DateTime<Utc>)>,
    pub(crate) record_cache: Cache<(InstanceId, String, String), (SnRecord, DateTime<Utc>)>,
    pub(crate) layout_cache: Cache<(InstanceId, String), Option<FormGridLayout>>,
    pub(crate) list_columns_cache: Cache<(InstanceId, String), Option<Vec<String>>>,
    pub(crate) hierarchy_cache: Cache<InstanceId, TableHierarchy>,
}

/// Thread-safe (`Clone + Send + Sync`) local cache combining sub-microsecond `moka` hot layers
/// with durable SQLite WAL + FTS5 persistence.
#[derive(Clone)]
pub struct SnCache {
    pub(crate) inner: Arc<SnCacheInner>,
}

impl std::fmt::Debug for SnCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnCache")
            .field("db_path", &self.inner.db_path)
            .finish()
    }
}

impl SnCache {
    /// Opens the default per-user cache database (`snrust_cache.db` inside the OS application data directory).
    pub fn open_default() -> SnResult<Self> {
        let proj_dirs = ProjectDirs::from("dev", "snrust", "snrust").ok_or_else(|| {
            SnError::Cache("Unable to resolve OS application data directory for snrust".into())
        })?;
        let data_dir = proj_dirs.data_dir();
        std::fs::create_dir_all(data_dir).map_err(cache_err)?;
        Self::open_path(data_dir.join(DEFAULT_DB_FILENAME))
    }

    /// Opens (or creates) a SQLite WAL cache database at `path`.
    pub fn open_path(path: impl AsRef<Path>) -> SnResult<Self> {
        let path_buf = path.as_ref().to_path_buf();
        if let Some(parent) = path_buf.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(cache_err)?;
        }

        let conn = Connection::open(&path_buf).map_err(cache_err)?;
        initialize_database(&conn)?;
        Ok(Self::from_connection(conn, Some(path_buf)))
    }

    /// Opens an isolated in-memory SQLite + `moka` cache instance for fast unit and integration tests.
    pub fn open_in_memory() -> SnResult<Self> {
        let conn = Connection::open_in_memory().map_err(cache_err)?;
        initialize_database(&conn)?;
        Ok(Self::from_connection(conn, None))
    }

    fn from_connection(conn: Connection, db_path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(SnCacheInner {
                conn: Mutex::new(conn),
                db_path,
                ttls: RwLock::new(CacheTtlConfig::default()),
                schema_cache: Cache::builder().max_capacity(256).build(),
                record_cache: Cache::builder().max_capacity(10_000).build(),
                layout_cache: Cache::builder().max_capacity(512).build(),
                list_columns_cache: Cache::builder().max_capacity(512).build(),
                hierarchy_cache: Cache::builder().max_capacity(32).build(),
            }),
        }
    }

    /// Overrides the SWR freshness TTL configuration (`schema_ttl`, `record_ttl`, `list_ttl`).
    pub fn set_ttl_config(&self, config: CacheTtlConfig) {
        *self.inner.ttls.write() = config;
    }

    /// Builder-style helper to customize SWR freshness TTLs.
    pub fn with_ttl_config(self, config: CacheTtlConfig) -> Self {
        self.set_ttl_config(config);
        self
    }

    /// Convenience builder to customize the `TableSchema` staleness window (defaults to 24 hours).
    pub fn with_schema_ttl(self, schema_ttl: Duration) -> Self {
        self.inner.ttls.write().schema_ttl = schema_ttl;
        self
    }

    /// Returns cached row counts (`record_count`, `schema_count`) and SQLite database byte size.
    pub fn cache_stats(&self, instance_id: Option<&InstanceId>) -> SnResult<CacheStats> {
        let conn = self.inner.conn.lock();

        let (record_count, schema_count): (i64, i64) = match instance_id {
            Some(id) => {
                let recs: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM records WHERE instance_id = ?1",
                        params![id.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(cache_err)?;
                let schemas: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM table_schemas WHERE instance_id = ?1",
                        params![id.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(cache_err)?;
                (recs, schemas)
            }
            None => {
                let recs: i64 = conn
                    .query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))
                    .map_err(cache_err)?;
                let schemas: i64 = conn
                    .query_row("SELECT COUNT(*) FROM table_schemas", [], |r| r.get(0))
                    .map_err(cache_err)?;
                (recs, schemas)
            }
        };

        let db_bytes = compute_db_bytes(&conn, self.inner.db_path.as_deref());

        Ok(CacheStats {
            record_count: usize::try_from(record_count).unwrap_or(0),
            schema_count: usize::try_from(schema_count).unwrap_or(0),
            db_bytes,
        })
    }

    /// Clears Tier 1 (catalog & schemas) and Tier 2 (records, lists, journals, references) data for
    /// `instance_id` while preserving Tier 3 `InstanceProfile`s, favorites, and user `FormGridLayout`s.
    pub fn clear_instance_cache(&self, instance_id: &InstanceId) -> SnResult<()> {
        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        for sql in [
            "DELETE FROM table_catalog_fts WHERE instance_id = ?1",
            "DELETE FROM table_catalog WHERE instance_id = ?1",
            "DELETE FROM table_schemas WHERE instance_id = ?1",
            "DELETE FROM records WHERE instance_id = ?1",
            "DELETE FROM list_pages WHERE instance_id = ?1",
            "DELETE FROM journals WHERE instance_id = ?1",
            "DELETE FROM reference_candidates_fts WHERE instance_id = ?1",
            "DELETE FROM reference_candidates WHERE instance_id = ?1",
        ] {
            tx.execute(sql, params![instance_id.as_str()])
                .map_err(cache_err)?;
        }

        tx.commit().map_err(cache_err)?;
        drop(conn);

        self.evict_memory_for_instance(instance_id, false);
        Ok(())
    }

    pub(crate) fn evict_memory_for_instance(
        &self,
        instance_id: &InstanceId,
        include_tier3_layouts: bool,
    ) {
        self.inner.hierarchy_cache.invalidate(instance_id);

        let schema_keys: Vec<(InstanceId, String)> = self
            .inner
            .schema_cache
            .iter()
            .filter(|(k, _)| &k.0 == instance_id)
            .map(|(k, _)| (*k).clone())
            .collect();
        for key in schema_keys {
            self.inner.schema_cache.invalidate(&key);
        }

        let record_keys: Vec<(InstanceId, String, String)> = self
            .inner
            .record_cache
            .iter()
            .filter(|(k, _)| &k.0 == instance_id)
            .map(|(k, _)| (*k).clone())
            .collect();
        for key in record_keys {
            self.inner.record_cache.invalidate(&key);
        }

        if include_tier3_layouts {
            let layout_keys: Vec<(InstanceId, String)> = self
                .inner
                .layout_cache
                .iter()
                .filter(|(k, _)| &k.0 == instance_id)
                .map(|(k, _)| (*k).clone())
                .collect();
            for key in layout_keys {
                self.inner.layout_cache.invalidate(&key);
            }

            let col_keys: Vec<(InstanceId, String)> = self
                .inner
                .list_columns_cache
                .iter()
                .filter(|(k, _)| &k.0 == instance_id)
                .map(|(k, _)| (*k).clone())
                .collect();
            for key in col_keys {
                self.inner.list_columns_cache.invalidate(&key);
            }
        }
    }

    /// Executes a cache operation on Tokio's blocking pool so bulk SQLite writes never stall async tasks.
    pub async fn run_blocking<F, T>(&self, op: F) -> SnResult<T>
    where
        F: FnOnce(&SnCache) -> SnResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let cloned = self.clone();
        tokio::task::spawn_blocking(move || op(&cloned))
            .await
            .map_err(|join_err| SnError::Cache(format!("Blocking cache task failed: {join_err}")))?
    }

    /// Async wrapper around [`Self::save_table_catalog`] via `tokio::task::spawn_blocking`.
    pub async fn save_table_catalog_async(
        &self,
        instance_id: InstanceId,
        tables: Vec<TableDescriptor>,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.save_table_catalog(&instance_id, &tables))
            .await
    }

    /// Async wrapper around [`Self::save_table_schema`] via `tokio::task::spawn_blocking`.
    pub async fn save_table_schema_async(
        &self,
        instance_id: InstanceId,
        schema: TableSchema,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.save_table_schema(&instance_id, &schema))
            .await
    }

    /// Async wrapper around [`Self::get_table_schema`] (returns immediately if warm in `moka`).
    pub async fn get_table_schema_async(
        &self,
        instance_id: InstanceId,
        table: String,
    ) -> SnResult<Option<CachedItem<TableSchema>>> {
        let key = (instance_id.clone(), table.clone());
        if let Some((schema, cached_at)) = self.inner.schema_cache.get(&key) {
            return Ok(Some(CachedItem::from_timestamp(
                schema,
                cached_at,
                self.inner.ttls.read().schema_ttl,
            )));
        }
        self.run_blocking(move |c| c.get_table_schema(&instance_id, &table))
            .await
    }

    /// Async wrapper around [`Self::get_record`] (returns immediately if warm in `moka`).
    pub async fn get_record_async(
        &self,
        instance_id: InstanceId,
        table: String,
        sys_id: String,
    ) -> SnResult<Option<CachedItem<SnRecord>>> {
        let key = (instance_id.clone(), table.clone(), sys_id.clone());
        if let Some((record, cached_at)) = self.inner.record_cache.get(&key) {
            return Ok(Some(CachedItem::from_timestamp(
                record,
                cached_at,
                self.inner.ttls.read().record_ttl,
            )));
        }
        self.run_blocking(move |c| c.get_record(&instance_id, &table, &sys_id))
            .await
    }

    /// Async wrapper around [`Self::upsert_records_batch`] via `tokio::task::spawn_blocking`.
    pub async fn upsert_records_batch_async(
        &self,
        instance_id: InstanceId,
        records: Vec<SnRecord>,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.upsert_records_batch(&instance_id, &records))
            .await
    }

    /// Async wrapper around [`Self::save_list_page`] via `tokio::task::spawn_blocking`.
    pub async fn save_list_page_async(
        &self,
        instance_id: InstanceId,
        page: ListPage,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.save_list_page(&instance_id, &page))
            .await
    }

    /// Async wrapper around [`Self::get_list_page`] via `tokio::task::spawn_blocking`.
    pub async fn get_list_page_async(
        &self,
        instance_id: InstanceId,
        table: String,
        encoded_query: String,
        offset: usize,
        limit: usize,
    ) -> SnResult<Option<CachedItem<ListPage>>> {
        self.run_blocking(move |c| {
            c.get_list_page(&instance_id, &table, &encoded_query, offset, limit)
        })
        .await
    }

    /// Async wrapper around [`Self::apply_list_delta`] via `tokio::task::spawn_blocking`.
    pub async fn apply_list_delta_async(
        &self,
        instance_id: InstanceId,
        table: String,
        updated_records: Vec<SnRecord>,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.apply_list_delta(&instance_id, &table, &updated_records))
            .await
    }

    /// Async wrapper around [`Self::save_journals`] via `tokio::task::spawn_blocking`.
    pub async fn save_journals_async(
        &self,
        instance_id: InstanceId,
        table: String,
        record_sys_id: String,
        entries: Vec<JournalEntry>,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.save_journals(&instance_id, &table, &record_sys_id, &entries))
            .await
    }

    /// Async wrapper around [`Self::save_reference_candidates`] via `tokio::task::spawn_blocking`.
    pub async fn save_reference_candidates_async(
        &self,
        instance_id: InstanceId,
        candidates: Vec<ReferenceCandidate>,
    ) -> SnResult<()> {
        self.run_blocking(move |c| c.save_reference_candidates(&instance_id, &candidates))
            .await
    }
}
