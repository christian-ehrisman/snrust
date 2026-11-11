//! Tier 1: `sys_db_object` table catalog, FTS5 table search, `TableHierarchy`, and `TableSchema` cache.

use std::collections::HashSet;

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use sn_core::{InstanceId, SnResult, TableDescriptor, TableHierarchy, TableSchema};

use crate::SnCache;
use crate::schema_sql::{build_fts_prefix_query, cache_err, escape_like, format_ts, parse_ts};
use crate::types::CachedItem;

struct DefaultItsmTable {
    name: &'static str,
    label: &'static str,
    super_class: Option<&'static str>,
    number_prefix: Option<&'static str>,
    is_extendable: bool,
}

/// Common ITSM tables surfaced in priority order when the table search input is empty.
const DEFAULT_ITSM_TABLES: &[DefaultItsmTable] = &[
    DefaultItsmTable {
        name: "incident",
        label: "Incident",
        super_class: Some("task"),
        number_prefix: Some("INC"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "sc_req_item",
        label: "Requested Item",
        super_class: Some("task"),
        number_prefix: Some("RITM"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "sc_task",
        label: "Catalog Task",
        super_class: Some("task"),
        number_prefix: Some("SCTASK"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "sc_request",
        label: "Request",
        super_class: Some("task"),
        number_prefix: Some("REQ"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "change_request",
        label: "Change Request",
        super_class: Some("task"),
        number_prefix: Some("CHG"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "problem",
        label: "Problem",
        super_class: Some("task"),
        number_prefix: Some("PRB"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "task",
        label: "Task",
        super_class: None,
        number_prefix: Some("TASK"),
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "sys_user",
        label: "User",
        super_class: None,
        number_prefix: None,
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "sys_user_group",
        label: "Group",
        super_class: None,
        number_prefix: None,
        is_extendable: false,
    },
    DefaultItsmTable {
        name: "cmdb_ci",
        label: "Configuration Item",
        super_class: None,
        number_prefix: None,
        is_extendable: true,
    },
    DefaultItsmTable {
        name: "kb_knowledge",
        label: "Knowledge",
        super_class: None,
        number_prefix: Some("KB"),
        is_extendable: true,
    },
];

impl SnCache {
    /// Persists `sys_db_object` descriptors and rebuilds the per-instance FTS5 table search index.
    pub fn save_table_catalog(
        &self,
        instance_id: &InstanceId,
        tables: &[TableDescriptor],
    ) -> SnResult<()> {
        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        {
            let mut upsert = tx
                .prepare_cached(
                    "INSERT INTO table_catalog
                        (instance_id, name, sys_id, label, super_class, number_prefix, is_extendable)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(instance_id, name) DO UPDATE SET
                        sys_id = excluded.sys_id,
                        label = excluded.label,
                        super_class = excluded.super_class,
                        number_prefix = excluded.number_prefix,
                        is_extendable = excluded.is_extendable",
                )
                .map_err(cache_err)?;

            for desc in tables {
                upsert
                    .execute(params![
                        instance_id.as_str(),
                        desc.name,
                        desc.sys_id,
                        desc.label,
                        desc.super_class,
                        desc.number_prefix,
                        i32::from(desc.is_extendable),
                    ])
                    .map_err(cache_err)?;
            }
        }

        // Rebuild this instance's FTS5 entries from `table_catalog` so incremental syncs never duplicate rows.
        tx.execute(
            "DELETE FROM table_catalog_fts WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )
        .map_err(cache_err)?;

        tx.execute(
            "INSERT INTO table_catalog_fts (instance_id, name, label, number_prefix)
             SELECT instance_id, name, label, COALESCE(number_prefix, '')
             FROM table_catalog
             WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )
        .map_err(cache_err)?;

        tx.commit().map_err(cache_err)?;

        self.inner.hierarchy_cache.invalidate(instance_id);
        Ok(())
    }

    /// Loads (or returns from sub-microsecond `moka` cache) the [`TableHierarchy`] graph for `instance_id`.
    pub fn get_table_hierarchy(&self, instance_id: &InstanceId) -> SnResult<TableHierarchy> {
        if let Some(cached) = self.inner.hierarchy_cache.get(instance_id) {
            return Ok(cached);
        }

        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT sys_id, name, label, super_class, number_prefix, is_extendable
                 FROM table_catalog
                 WHERE instance_id = ?1",
            )
            .map_err(cache_err)?;

        let rows = stmt
            .query_map(params![instance_id.as_str()], row_to_table_descriptor)
            .map_err(cache_err)?;

        let mut descriptors = Vec::new();
        for row in rows {
            descriptors.push(row.map_err(cache_err)?);
        }

        let hierarchy = TableHierarchy::from_descriptors(descriptors);
        self.inner
            .hierarchy_cache
            .insert(instance_id.clone(), hierarchy.clone());
        Ok(hierarchy)
    }

    /// Searches cached `sys_db_object` entries using FTS5 prefix matching with `LIKE` substring fallback.
    ///
    /// When `query` is empty/whitespace, returns common ITSM tables in priority order (using cached
    /// descriptors when synced, or built-in ITSM fallbacks on a cold instance).
    pub fn search_tables(
        &self,
        instance_id: &InstanceId,
        query: &str,
        limit: usize,
    ) -> SnResult<Vec<TableDescriptor>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let trimmed = query.trim();
        let conn = self.inner.conn.lock();

        if trimmed.is_empty() {
            return load_default_tables(&conn, instance_id, limit);
        }

        let mut results = Vec::new();
        let mut seen = HashSet::new();
        let exact_lower = trimmed.to_lowercase();
        let prefix_like = format!("{}%", escape_like(&exact_lower));

        if let Some(fts_query) = build_fts_prefix_query(trimmed) {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT c.sys_id, c.name, c.label, c.super_class, c.number_prefix, c.is_extendable
                     FROM table_catalog_fts f
                     JOIN table_catalog c
                       ON c.instance_id = f.instance_id AND c.name = f.name
                     WHERE table_catalog_fts MATCH ?1
                       AND f.instance_id = ?2
                     ORDER BY
                       (LOWER(c.name) = ?3) DESC,
                       (LOWER(c.number_prefix) = ?3) DESC,
                       (LOWER(c.label) = ?3) DESC,
                       (LOWER(c.name) LIKE ?4 ESCAPE '\\') DESC,
                       (LOWER(c.label) LIKE ?4 ESCAPE '\\') DESC,
                       bm25(table_catalog_fts, 0.0, 10.0, 5.0, 8.0),
                       c.name ASC
                     LIMIT ?5",
                )
                .map_err(cache_err)?;

            let rows = stmt
                .query_map(
                    params![
                        fts_query,
                        instance_id.as_str(),
                        exact_lower,
                        prefix_like,
                        limit as i64
                    ],
                    row_to_table_descriptor,
                )
                .map_err(cache_err)?;

            for row in rows {
                let desc = row.map_err(cache_err)?;
                if seen.insert(desc.name.clone()) {
                    results.push(desc);
                }
            }
        }

        // Substring fallback (e.g. searching "cident" or "_item") when FTS5 prefix hits < limit.
        if results.len() < limit {
            let remaining = limit - results.len();
            let contains_like = format!("%{}%", escape_like(&exact_lower));
            let mut stmt = conn
                .prepare_cached(
                    "SELECT sys_id, name, label, super_class, number_prefix, is_extendable
                     FROM table_catalog
                     WHERE instance_id = ?1
                       AND (
                         LOWER(name) LIKE ?2 ESCAPE '\\'
                         OR LOWER(label) LIKE ?2 ESCAPE '\\'
                         OR LOWER(COALESCE(number_prefix, '')) LIKE ?2 ESCAPE '\\'
                       )
                     ORDER BY
                       (LOWER(name) = ?3) DESC,
                       (LOWER(name) LIKE ?4 ESCAPE '\\') DESC,
                       (LOWER(label) LIKE ?4 ESCAPE '\\') DESC,
                       name ASC
                     LIMIT ?5",
                )
                .map_err(cache_err)?;

            let rows = stmt
                .query_map(
                    params![
                        instance_id.as_str(),
                        contains_like,
                        exact_lower,
                        prefix_like,
                        (remaining + seen.len()) as i64
                    ],
                    row_to_table_descriptor,
                )
                .map_err(cache_err)?;

            for row in rows {
                if results.len() >= limit {
                    break;
                }
                let desc = row.map_err(cache_err)?;
                if seen.insert(desc.name.clone()) {
                    results.push(desc);
                }
            }
        }

        Ok(results)
    }

    /// Retrieves a cached [`TableSchema`] from `moka` hot memory or SQLite, computing SWR staleness.
    pub fn get_table_schema(
        &self,
        instance_id: &InstanceId,
        table: &str,
    ) -> SnResult<Option<CachedItem<TableSchema>>> {
        let key = (instance_id.clone(), table.to_string());
        if let Some((schema, cached_at)) = self.inner.schema_cache.get(&key) {
            return Ok(Some(CachedItem::from_timestamp(
                schema,
                cached_at,
                self.inner.ttls.read().schema_ttl,
            )));
        }

        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT schema_json, cached_at
                 FROM table_schemas
                 WHERE instance_id = ?1 AND table_name = ?2",
            )
            .map_err(cache_err)?;

        let row: Option<(String, String)> = stmt
            .query_row(params![instance_id.as_str(), table], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(cache_err)?;

        drop(stmt);
        drop(conn);

        let Some((schema_json, cached_at_raw)) = row else {
            return Ok(None);
        };

        let schema: TableSchema = serde_json::from_str(&schema_json)?;
        let cached_at = parse_ts(&cached_at_raw);
        self.inner
            .schema_cache
            .insert(key, (schema.clone(), cached_at));

        Ok(Some(CachedItem::from_timestamp(
            schema,
            cached_at,
            self.inner.ttls.read().schema_ttl,
        )))
    }

    /// Stores a merged [`TableSchema`] in SQLite and warms the in-memory `moka` schema cache.
    pub fn save_table_schema(
        &self,
        instance_id: &InstanceId,
        schema: &TableSchema,
    ) -> SnResult<()> {
        let now = Utc::now();
        let now_str = format_ts(now);
        let schema_json = serde_json::to_string(schema)?;

        let conn = self.inner.conn.lock();
        conn.execute(
            "INSERT INTO table_schemas (instance_id, table_name, schema_json, cached_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(instance_id, table_name) DO UPDATE SET
                schema_json = excluded.schema_json,
                cached_at = excluded.cached_at",
            params![instance_id.as_str(), schema.table, schema_json, now_str],
        )
        .map_err(cache_err)?;
        drop(conn);

        self.inner.schema_cache.insert(
            (instance_id.clone(), schema.table.clone()),
            (schema.clone(), now),
        );
        Ok(())
    }
}

fn row_to_table_descriptor(row: &rusqlite::Row<'_>) -> rusqlite::Result<TableDescriptor> {
    let extendable_int: i32 = row.get(5)?;
    Ok(TableDescriptor {
        sys_id: row.get(0)?,
        name: row.get(1)?,
        label: row.get(2)?,
        super_class: row.get(3)?,
        number_prefix: row.get(4)?,
        is_extendable: extendable_int != 0,
    })
}

fn load_default_tables(
    conn: &rusqlite::Connection,
    instance_id: &InstanceId,
    limit: usize,
) -> SnResult<Vec<TableDescriptor>> {
    let catalog_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM table_catalog WHERE instance_id = ?1",
            params![instance_id.as_str()],
            |r| r.get(0),
        )
        .map_err(cache_err)?;

    if catalog_count == 0 {
        return Ok(DEFAULT_ITSM_TABLES
            .iter()
            .take(limit)
            .map(|entry| TableDescriptor {
                sys_id: entry.name.to_string(),
                name: entry.name.to_string(),
                label: entry.label.to_string(),
                super_class: entry.super_class.map(str::to_string),
                number_prefix: entry.number_prefix.map(str::to_string),
                is_extendable: entry.is_extendable,
            })
            .collect());
    }

    let mut results = Vec::new();
    let mut seen = HashSet::new();

    {
        let mut by_name = conn
            .prepare_cached(
                "SELECT sys_id, name, label, super_class, number_prefix, is_extendable
                 FROM table_catalog
                 WHERE instance_id = ?1 AND name = ?2",
            )
            .map_err(cache_err)?;

        for entry in DEFAULT_ITSM_TABLES {
            if results.len() >= limit {
                break;
            }
            if let Some(desc) = by_name
                .query_row(
                    params![instance_id.as_str(), entry.name],
                    row_to_table_descriptor,
                )
                .optional()
                .map_err(cache_err)?
                && seen.insert(desc.name.clone())
            {
                results.push(desc);
            }
        }
    }

    if results.len() < limit {
        let mut rest_stmt = conn
            .prepare_cached(
                "SELECT sys_id, name, label, super_class, number_prefix, is_extendable
                 FROM table_catalog
                 WHERE instance_id = ?1
                 ORDER BY label COLLATE NOCASE ASC, name ASC
                 LIMIT ?2",
            )
            .map_err(cache_err)?;

        let rows = rest_stmt
            .query_map(
                params![instance_id.as_str(), (limit + seen.len()) as i64],
                row_to_table_descriptor,
            )
            .map_err(cache_err)?;

        for row in rows {
            if results.len() >= limit {
                break;
            }
            let desc = row.map_err(cache_err)?;
            if seen.insert(desc.name.clone()) {
                results.push(desc);
            }
        }
    }

    Ok(results)
}
