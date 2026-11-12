//! Tier 3: Instance connection profiles, active workspace state, favorites, recents,
//! rearrangeable [`FormGridLayout`] persistence, and custom list columns.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use sn_core::{FormGridLayout, InstanceId, InstanceProfile, SnResult};

use crate::SnCache;
use crate::schema_sql::{cache_err, format_ts, parse_ts};
use crate::types::{FavoriteItem, FavoriteKind, RecentItem};

const ACTIVE_INSTANCE_KEY: &str = "active_instance_id";
const MAX_RECENT_ITEMS: i64 = 50;

impl SnCache {
    /// Lists all configured ServiceNow [`InstanceProfile`]s ordered by creation time.
    pub fn list_instances(&self) -> SnResult<Vec<InstanceProfile>> {
        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, name, base_url, client_id, has_client_secret,
                        callback_port, scopes, accent_hex, created_at, last_connected_at, account_json
                 FROM instances
                 ORDER BY created_at ASC, name COLLATE NOCASE ASC",
            )
            .map_err(cache_err)?;

        let rows = stmt
            .query_map([], |r| {
                let id_str: String = r.get(0)?;
                let has_secret: i32 = r.get(4)?;
                let port: u16 = r.get(5)?;
                let created_raw: String = r.get(8)?;
                let last_conn_raw: Option<String> = r.get(9)?;
                Ok(InstanceProfile {
                    id: InstanceId(id_str),
                    name: r.get(1)?,
                    base_url: r.get(2)?,
                    client_id: r.get(3)?,
                    has_client_secret: has_secret != 0,
                    callback_port: port,
                    scopes: r.get(6)?,
                    accent_hex: r.get(7)?,
                    created_at: parse_ts(&created_raw),
                    last_connected_at: last_conn_raw.as_deref().map(parse_ts),
                    account: r
                        .get::<_, Option<String>>(10)?
                        .map(|json| serde_json::from_str(&json))
                        .transpose()
                        .map_err(|err| {
                            rusqlite::Error::FromSqlConversionFailure(
                                10,
                                rusqlite::types::Type::Text,
                                Box::new(err),
                            )
                        })?,
                })
            })
            .map_err(cache_err)?;

        let mut profiles = Vec::new();
        for row in rows {
            profiles.push(row.map_err(cache_err)?);
        }
        Ok(profiles)
    }

    /// Inserts or updates a non-secret [`InstanceProfile`].
    pub fn upsert_instance(&self, profile: &InstanceProfile) -> SnResult<()> {
        let created_str = format_ts(profile.created_at);
        let last_conn_str = profile.last_connected_at.map(format_ts);

        let conn = self.inner.conn.lock();
        conn.execute(
            "INSERT INTO instances
                (id, name, base_url, client_id, has_client_secret,
                 callback_port, scopes, accent_hex, created_at, last_connected_at, account_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                base_url = excluded.base_url,
                client_id = excluded.client_id,
                has_client_secret = excluded.has_client_secret,
                callback_port = excluded.callback_port,
                scopes = excluded.scopes,
                accent_hex = excluded.accent_hex,
                created_at = excluded.created_at,
                last_connected_at = excluded.last_connected_at,
                account_json = excluded.account_json",
            params![
                profile.id.as_str(),
                profile.name,
                profile.base_url,
                profile.client_id,
                i32::from(profile.has_client_secret),
                profile.callback_port,
                profile.scopes,
                profile.accent_hex,
                created_str,
                last_conn_str,
                profile
                    .account
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
            ],
        )
        .map_err(cache_err)?;
        Ok(())
    }

    /// Deletes an [`InstanceProfile`] and cascades all Tier 1, Tier 2, and Tier 3 data for `id`.
    pub fn delete_instance(&self, id: &InstanceId) -> SnResult<()> {
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
            "DELETE FROM favorites WHERE instance_id = ?1",
            "DELETE FROM recent_items WHERE instance_id = ?1",
            "DELETE FROM form_layouts WHERE instance_id = ?1",
            "DELETE FROM list_columns WHERE instance_id = ?1",
            "DELETE FROM account_metadata WHERE instance_id = ?1",
            "DELETE FROM instances WHERE id = ?1",
        ] {
            tx.execute(sql, params![id.as_str()]).map_err(cache_err)?;
        }

        tx.execute(
            "DELETE FROM workspace_kv WHERE key = ?1 AND value = ?2",
            params![ACTIVE_INSTANCE_KEY, id.as_str()],
        )
        .map_err(cache_err)?;

        tx.commit().map_err(cache_err)?;
        drop(conn);

        self.evict_memory_for_instance(id, true);
        Ok(())
    }

    /// Returns the last active [`InstanceId`] selected in the workspace, if any.
    pub fn get_active_instance_id(&self) -> SnResult<Option<InstanceId>> {
        let conn = self.inner.conn.lock();
        let val: Option<String> = conn
            .query_row(
                "SELECT value FROM workspace_kv WHERE key = ?1",
                params![ACTIVE_INSTANCE_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(cache_err)?;
        Ok(val.map(InstanceId))
    }

    /// Persists (or clears when `None`) the active [`InstanceId`].
    pub fn set_active_instance_id(&self, id: Option<&InstanceId>) -> SnResult<()> {
        let conn = self.inner.conn.lock();
        match id {
            Some(inst_id) => {
                conn.execute(
                    "INSERT INTO workspace_kv (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![ACTIVE_INSTANCE_KEY, inst_id.as_str()],
                )
                .map_err(cache_err)?;
            }
            None => {
                conn.execute(
                    "DELETE FROM workspace_kv WHERE key = ?1",
                    params![ACTIVE_INSTANCE_KEY],
                )
                .map_err(cache_err)?;
            }
        }
        Ok(())
    }

    /// Reads an application-wide setting (not scoped to an instance) stored as JSON.
    pub fn get_setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> SnResult<Option<T>> {
        let json: Option<String> = self
            .inner
            .conn
            .lock()
            .query_row(
                "SELECT value FROM workspace_kv WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(cache_err)?;
        json.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    /// Persists an application-wide setting as JSON, replacing any previous value.
    pub fn set_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> SnResult<()> {
        let json = serde_json::to_string(value)?;
        self.inner
            .conn
            .lock()
            .execute(
                "INSERT INTO workspace_kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, json],
            )
            .map_err(cache_err)?;
        Ok(())
    }

    /// Lists pinned sidebar favorites for `instance_id` in creation order.
    pub fn list_favorites(&self, instance_id: &InstanceId) -> SnResult<Vec<FavoriteItem>> {
        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, kind_json, title, subtitle
                 FROM favorites
                 WHERE instance_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )
            .map_err(cache_err)?;

        let rows = stmt
            .query_map(params![instance_id.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(cache_err)?;

        let mut items = Vec::new();
        for row in rows {
            let (id, kind_json, title, subtitle) = row.map_err(cache_err)?;
            let kind: FavoriteKind = serde_json::from_str(&kind_json)?;
            items.push(FavoriteItem {
                id,
                kind,
                title,
                subtitle,
            });
        }
        Ok(items)
    }

    /// Inserts or updates a [`FavoriteItem`] for `instance_id`.
    pub fn upsert_favorite(&self, instance_id: &InstanceId, item: &FavoriteItem) -> SnResult<()> {
        let kind_json = serde_json::to_string(&item.kind)?;
        let now_str = format_ts(Utc::now());

        let conn = self.inner.conn.lock();
        conn.execute(
            "INSERT INTO favorites (instance_id, id, kind_json, title, subtitle, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(instance_id, id) DO UPDATE SET
                kind_json = excluded.kind_json,
                title = excluded.title,
                subtitle = excluded.subtitle",
            params![
                instance_id.as_str(),
                item.id,
                kind_json,
                item.title,
                item.subtitle,
                now_str,
            ],
        )
        .map_err(cache_err)?;
        Ok(())
    }

    /// Removes a favorite by `fav_id` for `instance_id`.
    pub fn remove_favorite(&self, instance_id: &InstanceId, fav_id: &str) -> SnResult<()> {
        let conn = self.inner.conn.lock();
        conn.execute(
            "DELETE FROM favorites WHERE instance_id = ?1 AND id = ?2",
            params![instance_id.as_str(), fav_id],
        )
        .map_err(cache_err)?;
        Ok(())
    }

    /// Records a table/record visit, deduplicating repeat visits and capping history at 50 per instance.
    pub fn record_recent(&self, instance_id: &InstanceId, item: &RecentItem) -> SnResult<()> {
        let target_key = match &item.sys_id {
            Some(sys_id) if !sys_id.is_empty() => format!("record:{}:{}", item.table, sys_id),
            _ => format!("table:{}", item.table),
        };
        let visited_str = format_ts(item.visited_at);

        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        tx.execute(
            "INSERT INTO recent_items
                (instance_id, target_key, table_name, sys_id, title, subtitle, visited_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(instance_id, target_key) DO UPDATE SET
                title = excluded.title,
                subtitle = excluded.subtitle,
                visited_at = excluded.visited_at",
            params![
                instance_id.as_str(),
                target_key,
                item.table,
                item.sys_id,
                item.title,
                item.subtitle,
                visited_str,
            ],
        )
        .map_err(cache_err)?;

        tx.execute(
            "DELETE FROM recent_items
             WHERE instance_id = ?1
               AND target_key NOT IN (
                 SELECT target_key
                 FROM recent_items
                 WHERE instance_id = ?1
                 ORDER BY visited_at DESC, target_key ASC
                 LIMIT ?2
               )",
            params![instance_id.as_str(), MAX_RECENT_ITEMS],
        )
        .map_err(cache_err)?;

        tx.commit().map_err(cache_err)?;
        Ok(())
    }

    /// Returns the most recently visited items for `instance_id` (`visited_at DESC`, up to `limit`).
    pub fn list_recent(&self, instance_id: &InstanceId, limit: usize) -> SnResult<Vec<RecentItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT table_name, sys_id, title, subtitle, visited_at
                 FROM recent_items
                 WHERE instance_id = ?1
                 ORDER BY visited_at DESC, target_key ASC
                 LIMIT ?2",
            )
            .map_err(cache_err)?;

        let rows = stmt
            .query_map(params![instance_id.as_str(), limit as i64], |r| {
                let visited_raw: String = r.get(4)?;
                Ok(RecentItem {
                    table: r.get(0)?,
                    sys_id: r.get(1)?,
                    title: r.get(2)?,
                    subtitle: r.get(3)?,
                    visited_at: parse_ts(&visited_raw),
                })
            })
            .map_err(cache_err)?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(cache_err)?);
        }
        Ok(items)
    }

    /// Retrieves a user-customized [`FormGridLayout`] via `moka` hot cache -> SQLite fallback.
    pub fn get_form_layout(
        &self,
        instance_id: &InstanceId,
        table: &str,
    ) -> SnResult<Option<FormGridLayout>> {
        let key = (instance_id.clone(), table.to_string());
        if let Some(cached) = self.inner.layout_cache.get(&key) {
            return Ok(cached);
        }

        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT layout_json FROM form_layouts
                 WHERE instance_id = ?1 AND table_name = ?2",
            )
            .map_err(cache_err)?;

        let layout_json: Option<String> = stmt
            .query_row(params![instance_id.as_str(), table], |r| r.get(0))
            .optional()
            .map_err(cache_err)?;

        drop(stmt);
        drop(conn);

        let parsed = match layout_json {
            Some(json) => Some(serde_json::from_str::<FormGridLayout>(&json)?),
            None => None,
        };

        self.inner.layout_cache.insert(key, parsed.clone());
        Ok(parsed)
    }

    /// Saves a customized [`FormGridLayout`] for `(instance_id, layout.table)` and updates `moka`.
    pub fn save_form_layout(
        &self,
        instance_id: &InstanceId,
        layout: &FormGridLayout,
    ) -> SnResult<()> {
        self.save_form_layout_for_view(instance_id, layout, "")
    }

    /// Persists an override without sharing it with another account or named view.
    pub fn save_form_layout_for_view(
        &self,
        instance_id: &InstanceId,
        layout: &FormGridLayout,
        view: &str,
    ) -> SnResult<()> {
        let key = sn_core::classic::view_cache_key(&layout.table, view);
        let layout_json = serde_json::to_string(layout)?;
        let updated_str = format_ts(layout.updated_at);

        let conn = self.inner.conn.lock();
        conn.execute(
            "INSERT INTO form_layouts (instance_id, table_name, layout_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(instance_id, table_name) DO UPDATE SET
                layout_json = excluded.layout_json,
                updated_at = excluded.updated_at",
            params![instance_id.as_str(), key, layout_json, updated_str],
        )
        .map_err(cache_err)?;
        drop(conn);

        self.inner
            .layout_cache
            .insert((instance_id.clone(), key), Some(layout.clone()));
        Ok(())
    }

    pub fn delete_list_columns(&self, instance_id: &InstanceId, key: &str) -> SnResult<()> {
        self.inner
            .conn
            .lock()
            .execute(
                "DELETE FROM list_columns WHERE instance_id=?1 AND table_name=?2",
                params![instance_id.as_str(), key],
            )
            .map_err(cache_err)?;
        self.inner
            .list_columns_cache
            .invalidate(&(instance_id.clone(), key.to_string()));
        Ok(())
    }

    /// Cached server metadata is kept apart from local overrides and credential storage.
    pub fn get_account_metadata<T: serde::de::DeserializeOwned>(
        &self,
        id: &InstanceId,
        key: &str,
    ) -> SnResult<Option<T>> {
        let json: Option<String> = self
            .inner
            .conn
            .lock()
            .query_row(
                "SELECT value_json FROM account_metadata WHERE instance_id=?1 AND key=?2",
                params![id.as_str(), key],
                |row| row.get(0),
            )
            .optional()
            .map_err(cache_err)?;
        json.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub fn save_account_metadata<T: serde::Serialize>(
        &self,
        id: &InstanceId,
        key: &str,
        value: &T,
    ) -> SnResult<()> {
        let json = serde_json::to_string(value)?;
        self.inner
            .conn
            .lock()
            .execute(
                "INSERT INTO account_metadata(instance_id,key,value_json) VALUES (?1,?2,?3)
             ON CONFLICT(instance_id,key) DO UPDATE SET value_json=excluded.value_json",
                params![id.as_str(), key, json],
            )
            .map_err(cache_err)?;
        Ok(())
    }

    /// Removes any saved [`FormGridLayout`] override for `(instance_id, table)` (resetting to smart auto-grid).
    pub fn delete_form_layout(&self, instance_id: &InstanceId, table: &str) -> SnResult<()> {
        let conn = self.inner.conn.lock();
        conn.execute(
            "DELETE FROM form_layouts WHERE instance_id = ?1 AND table_name = ?2",
            params![instance_id.as_str(), table],
        )
        .map_err(cache_err)?;
        drop(conn);

        self.inner
            .layout_cache
            .insert((instance_id.clone(), table.to_string()), None);
        Ok(())
    }

    /// Retrieves user-customized list columns for `(instance_id, table)` if configured.
    pub fn get_list_columns(
        &self,
        instance_id: &InstanceId,
        table: &str,
    ) -> SnResult<Option<Vec<String>>> {
        let key = (instance_id.clone(), table.to_string());
        if let Some(cached) = self.inner.list_columns_cache.get(&key) {
            return Ok(cached);
        }

        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT columns_json FROM list_columns
                 WHERE instance_id = ?1 AND table_name = ?2",
            )
            .map_err(cache_err)?;

        let json: Option<String> = stmt
            .query_row(params![instance_id.as_str(), table], |r| r.get(0))
            .optional()
            .map_err(cache_err)?;

        drop(stmt);
        drop(conn);

        let parsed = match json {
            Some(raw) => Some(serde_json::from_str::<Vec<String>>(&raw)?),
            None => None,
        };

        self.inner.list_columns_cache.insert(key, parsed.clone());
        Ok(parsed)
    }

    /// Saves user-customized list columns for `(instance_id, table)`.
    pub fn save_list_columns(
        &self,
        instance_id: &InstanceId,
        table: &str,
        columns: &[String],
    ) -> SnResult<()> {
        let cols_vec = columns.to_vec();
        let cols_json = serde_json::to_string(&cols_vec)?;
        let now_str = format_ts(Utc::now());

        let conn = self.inner.conn.lock();
        conn.execute(
            "INSERT INTO list_columns (instance_id, table_name, columns_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(instance_id, table_name) DO UPDATE SET
                columns_json = excluded.columns_json,
                updated_at = excluded.updated_at",
            params![instance_id.as_str(), table, cols_json, now_str],
        )
        .map_err(cache_err)?;
        drop(conn);

        self.inner
            .list_columns_cache
            .insert((instance_id.clone(), table.to_string()), Some(cols_vec));
        Ok(())
    }
}
