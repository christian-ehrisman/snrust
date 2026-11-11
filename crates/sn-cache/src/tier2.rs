//! Tier 2: Normalized record store, SWR `ListPage` index, journals, and reference candidate FTS5.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sn_core::{
    FieldType, InstanceId, JournalEntry, ListPage, ReferenceCandidate, SnRecord, SnResult,
    TableSchema,
};

use crate::SnCache;
use crate::schema_sql::{build_fts_prefix_query, cache_err, escape_like, format_ts, parse_ts};
use crate::types::CachedItem;

impl SnCache {
    /// Looks up a single normalized [`SnRecord`] via sub-microsecond `moka` cache -> SQLite fallback.
    pub fn get_record(
        &self,
        instance_id: &InstanceId,
        table: &str,
        sys_id: &str,
    ) -> SnResult<Option<CachedItem<SnRecord>>> {
        let key = (instance_id.clone(), table.to_string(), sys_id.to_string());
        if let Some((record, cached_at)) = self.inner.record_cache.get(&key) {
            return Ok(Some(CachedItem::from_timestamp(
                record,
                cached_at,
                self.inner.ttls.read().record_ttl,
            )));
        }

        let row: Option<(String, String)> = {
            let conn = self.inner.conn.lock();
            let mut stmt = conn
                .prepare_cached(
                    "SELECT record_json, cached_at
                     FROM records
                     WHERE instance_id = ?1 AND table_name = ?2 AND sys_id = ?3",
                )
                .map_err(cache_err)?;

            stmt.query_row(params![instance_id.as_str(), table, sys_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(cache_err)?
        };

        let Some((record_json, cached_at_raw)) = row else {
            return Ok(None);
        };

        let record: SnRecord = serde_json::from_str(&record_json)?;
        let cached_at = parse_ts(&cached_at_raw);
        self.inner
            .record_cache
            .insert(key, (record.clone(), cached_at));

        Ok(Some(CachedItem::from_timestamp(
            record,
            cached_at,
            self.inner.ttls.read().record_ttl,
        )))
    }

    /// Finds a cached [`SnRecord`] by matching `field`'s raw or display value (e.g. `number = "INC0010042"`).
    pub fn find_record_by_field(
        &self,
        instance_id: &InstanceId,
        table: &str,
        field: &str,
        raw_or_display: &str,
    ) -> SnResult<Option<SnRecord>> {
        let needle = raw_or_display.trim();
        if needle.is_empty() {
            return Ok(None);
        }

        if field == "sys_id" {
            return Ok(self.get_record(instance_id, table, needle)?.map(|c| c.data));
        }

        if field == "number" {
            let hit: Option<(String, String)> = {
                let conn = self.inner.conn.lock();
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT record_json, cached_at
                         FROM records
                         WHERE instance_id = ?1 AND table_name = ?2 AND LOWER(number) = LOWER(?3)
                         ORDER BY (number = ?3) DESC, cached_at DESC
                         LIMIT 1",
                    )
                    .map_err(cache_err)?;

                stmt.query_row(params![instance_id.as_str(), table, needle], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .optional()
                .map_err(cache_err)?
            };

            if let Some((json, ts)) = hit {
                let rec: SnRecord = serde_json::from_str(&json)?;
                self.inner.record_cache.insert(
                    (instance_id.clone(), rec.table.clone(), rec.sys_id.clone()),
                    (rec.clone(), parse_ts(&ts)),
                );
                return Ok(Some(rec));
            }
        }

        let is_simple_ident =
            !field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

        if is_simple_ident {
            let raw_path = format!("$.fields.{field}.value");
            let disp_path = format!("$.fields.{field}.display_value");
            let hit: Option<(String, String)> = {
                let conn = self.inner.conn.lock();
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT record_json, cached_at
                         FROM records
                         WHERE instance_id = ?1
                           AND table_name = ?2
                           AND (
                             LOWER(json_extract(record_json, ?3)) = LOWER(?5)
                             OR LOWER(json_extract(record_json, ?4)) = LOWER(?5)
                           )
                         ORDER BY
                           (json_extract(record_json, ?3) = ?5 OR json_extract(record_json, ?4) = ?5) DESC,
                           cached_at DESC
                         LIMIT 1",
                    )
                    .map_err(cache_err)?;

                stmt.query_row(
                    params![instance_id.as_str(), table, raw_path, disp_path, needle],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(cache_err)?
            };

            if let Some((json, ts)) = hit {
                let rec: SnRecord = serde_json::from_str(&json)?;
                self.inner.record_cache.insert(
                    (instance_id.clone(), rec.table.clone(), rec.sys_id.clone()),
                    (rec.clone(), parse_ts(&ts)),
                );
                return Ok(Some(rec));
            }
            return Ok(None);
        }

        // Fallback for non-standard field identifiers containing punctuation.
        let matched: Option<(SnRecord, String)> = {
            let conn = self.inner.conn.lock();
            let mut stmt = conn
                .prepare_cached(
                    "SELECT record_json, cached_at
                     FROM records
                     WHERE instance_id = ?1 AND table_name = ?2
                     ORDER BY cached_at DESC",
                )
                .map_err(cache_err)?;

            let rows = stmt
                .query_map(params![instance_id.as_str(), table], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(cache_err)?;

            let mut found = None;
            for row in rows {
                let (json, ts) = row.map_err(cache_err)?;
                let rec: SnRecord = serde_json::from_str(&json)?;
                if rec.get_raw(field).eq_ignore_ascii_case(needle)
                    || rec.get_display(field).eq_ignore_ascii_case(needle)
                {
                    found = Some((rec, ts));
                    break;
                }
            }
            found
        };

        if let Some((rec, ts)) = matched {
            self.inner.record_cache.insert(
                (instance_id.clone(), rec.table.clone(), rec.sys_id.clone()),
                (rec.clone(), parse_ts(&ts)),
            );
            return Ok(Some(rec));
        }

        Ok(None)
    }

    /// Upserts a normalized [`SnRecord`], preserving previously cached full-form fields when
    /// `record.is_partial` is `true`, and indexes discoverable reference display values.
    pub fn upsert_record(&self, instance_id: &InstanceId, record: &SnRecord) -> SnResult<()> {
        if record.sys_id.is_empty() {
            return Ok(());
        }
        self.upsert_records_batch(instance_id, std::slice::from_ref(record))
    }

    /// Upserts multiple [`SnRecord`]s inside a single SQLite transaction with partial-safe field merging.
    pub fn upsert_records_batch(
        &self,
        instance_id: &InstanceId,
        records: &[SnRecord],
    ) -> SnResult<()> {
        if records.is_empty() {
            return Ok(());
        }

        let now = Utc::now();
        let now_str = format_ts(now);

        // Pre-resolve cached schemas for any tables in this batch to harvest Reference display values.
        let mut schemas_by_table: HashMap<String, TableSchema> = HashMap::new();
        for rec in records {
            if !schemas_by_table.contains_key(&rec.table)
                && let Some((schema, _)) = self
                    .inner
                    .schema_cache
                    .get(&(instance_id.clone(), rec.table.clone()))
            {
                schemas_by_table.insert(rec.table.clone(), schema);
            }
        }

        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        let warm_entries = upsert_records_in_tx(
            &tx,
            self,
            instance_id,
            records,
            &schemas_by_table,
            now,
            &now_str,
        )?;

        tx.commit().map_err(cache_err)?;
        drop(conn);

        for (key, val) in warm_entries {
            self.inner.record_cache.insert(key, val);
        }

        Ok(())
    }

    /// Persists a [`ListPage`]'s records into the normalized store and indexes the ordered `sys_id` list.
    pub fn save_list_page(&self, instance_id: &InstanceId, page: &ListPage) -> SnResult<()> {
        let now = Utc::now();
        let now_str = format_ts(now);

        let mut schemas_by_table: HashMap<String, TableSchema> = HashMap::new();
        if let Some((schema, _)) = self
            .inner
            .schema_cache
            .get(&(instance_id.clone(), page.table.clone()))
        {
            schemas_by_table.insert(page.table.clone(), schema);
        }

        let sys_ids: Vec<&str> = page
            .records
            .iter()
            .filter(|r| !r.sys_id.is_empty())
            .map(|r| r.sys_id.as_str())
            .collect();
        let sys_ids_json = serde_json::to_string(&sys_ids)?;

        let max_updated_on = page.max_updated_on.clone().or_else(|| {
            page.records
                .iter()
                .filter_map(|r| r.sys_updated_on.clone())
                .max()
        });

        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        let warm_entries = upsert_records_in_tx(
            &tx,
            self,
            instance_id,
            &page.records,
            &schemas_by_table,
            now,
            &now_str,
        )?;

        tx.execute(
            "INSERT INTO list_pages
                (instance_id, table_name, encoded_query, page_offset, page_limit,
                 sys_ids_json, total_count, max_updated_on, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(instance_id, table_name, encoded_query, page_offset, page_limit) DO UPDATE SET
                sys_ids_json = excluded.sys_ids_json,
                total_count = COALESCE(excluded.total_count, list_pages.total_count),
                max_updated_on = excluded.max_updated_on,
                cached_at = excluded.cached_at",
            params![
                instance_id.as_str(),
                page.table,
                page.encoded_query,
                page.offset as i64,
                page.limit as i64,
                sys_ids_json,
                page.total_count.map(|n| n as i64),
                max_updated_on,
                now_str,
            ],
        )
        .map_err(cache_err)?;

        tx.commit().map_err(cache_err)?;
        drop(conn);

        for (key, val) in warm_entries {
            self.inner.record_cache.insert(key, val);
        }

        Ok(())
    }

    /// Reconstructs a cached [`ListPage`] (`from_cache = true`) from the normalized record store.
    pub fn get_list_page(
        &self,
        instance_id: &InstanceId,
        table: &str,
        encoded_query: &str,
        offset: usize,
        limit: usize,
    ) -> SnResult<Option<CachedItem<ListPage>>> {
        let row: Option<(String, Option<i64>, Option<String>, String)> = {
            let conn = self.inner.conn.lock();
            let mut stmt = conn
                .prepare_cached(
                    "SELECT sys_ids_json, total_count, max_updated_on, cached_at
                     FROM list_pages
                     WHERE instance_id = ?1
                       AND table_name = ?2
                       AND encoded_query = ?3
                       AND page_offset = ?4
                       AND page_limit = ?5",
                )
                .map_err(cache_err)?;

            stmt.query_row(
                params![
                    instance_id.as_str(),
                    table,
                    encoded_query,
                    offset as i64,
                    limit as i64
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(cache_err)?
        };

        let Some((sys_ids_json, total_count, stored_max_updated_on, cached_at_raw)) = row else {
            return Ok(None);
        };

        let sys_ids: Vec<String> = serde_json::from_str(&sys_ids_json)?;
        let mut records = Vec::with_capacity(sys_ids.len());

        for sys_id in &sys_ids {
            if let Some(cached_rec) = self.get_record(instance_id, table, sys_id)? {
                records.push(cached_rec.data);
            }
        }

        // Advance `max_updated_on` if any normalized record was updated via form edit or delta poll.
        let derived_max = records
            .iter()
            .filter_map(|r| r.sys_updated_on.clone())
            .chain(stored_max_updated_on)
            .max();

        let cached_at = parse_ts(&cached_at_raw);
        let page = ListPage {
            table: table.to_string(),
            encoded_query: encoded_query.to_string(),
            offset,
            limit,
            total_count: total_count.and_then(|n| usize::try_from(n).ok()),
            records,
            max_updated_on: derived_max,
            from_cache: true,
        };

        Ok(Some(CachedItem::from_timestamp(
            page,
            cached_at,
            self.inner.ttls.read().list_ttl,
        )))
    }

    /// Applies incremental REST delta records into the normalized store and advances cached list watermarks.
    pub fn apply_list_delta(
        &self,
        instance_id: &InstanceId,
        table: &str,
        updated_records: &[SnRecord],
    ) -> SnResult<()> {
        if updated_records.is_empty() {
            return Ok(());
        }

        let now = Utc::now();
        let now_str = format_ts(now);

        let mut schemas_by_table: HashMap<String, TableSchema> = HashMap::new();
        if let Some((schema, _)) = self
            .inner
            .schema_cache
            .get(&(instance_id.clone(), table.to_string()))
        {
            schemas_by_table.insert(table.to_string(), schema);
        }

        let delta_max = updated_records
            .iter()
            .filter_map(|r| r.sys_updated_on.clone())
            .max();

        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        let warm_entries = upsert_records_in_tx(
            &tx,
            self,
            instance_id,
            updated_records,
            &schemas_by_table,
            now,
            &now_str,
        )?;

        if let Some(watermark) = delta_max {
            tx.execute(
                "UPDATE list_pages
                 SET max_updated_on = CASE
                        WHEN max_updated_on IS NULL OR ?3 > max_updated_on THEN ?3
                        ELSE max_updated_on
                     END,
                     cached_at = ?4
                 WHERE instance_id = ?1 AND table_name = ?2",
                params![instance_id.as_str(), table, watermark, now_str],
            )
            .map_err(cache_err)?;
        }

        tx.commit().map_err(cache_err)?;
        drop(conn);

        for (key, val) in warm_entries {
            self.inner.record_cache.insert(key, val);
        }

        Ok(())
    }

    /// Loads cached `sys_journal_field` Activity Stream entries ordered newest-first (`sys_created_on DESC`).
    pub fn get_journals(
        &self,
        instance_id: &InstanceId,
        table: &str,
        record_sys_id: &str,
    ) -> SnResult<Vec<JournalEntry>> {
        let conn = self.inner.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT sys_id, element, value, sys_created_on, sys_created_by
                 FROM journals
                 WHERE instance_id = ?1 AND table_name = ?2 AND record_sys_id = ?3
                 ORDER BY sys_created_on DESC, sys_id ASC",
            )
            .map_err(cache_err)?;

        let rows = stmt
            .query_map(params![instance_id.as_str(), table, record_sys_id], |r| {
                Ok(JournalEntry {
                    sys_id: r.get(0)?,
                    element: r.get(1)?,
                    element_id: record_sys_id.to_string(),
                    value: r.get(2)?,
                    sys_created_on: r.get(3)?,
                    sys_created_by: r.get(4)?,
                })
            })
            .map_err(cache_err)?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(row.map_err(cache_err)?);
        }
        Ok(entries)
    }

    /// Upserts `sys_journal_field` entries for `(instance_id, table, record_sys_id)`.
    pub fn save_journals(
        &self,
        instance_id: &InstanceId,
        table: &str,
        record_sys_id: &str,
        entries: &[JournalEntry],
    ) -> SnResult<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO journals
                        (instance_id, table_name, record_sys_id, sys_id, element, value, sys_created_on, sys_created_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(instance_id, table_name, record_sys_id, sys_id) DO UPDATE SET
                        element = excluded.element,
                        value = excluded.value,
                        sys_created_on = excluded.sys_created_on,
                        sys_created_by = excluded.sys_created_by",
                )
                .map_err(cache_err)?;

            for entry in entries {
                if entry.sys_id.is_empty() {
                    continue;
                }
                stmt.execute(params![
                    instance_id.as_str(),
                    table,
                    record_sys_id,
                    entry.sys_id,
                    entry.element,
                    entry.value,
                    entry.sys_created_on,
                    entry.sys_created_by,
                ])
                .map_err(cache_err)?;
            }
        }

        tx.commit().map_err(cache_err)?;
        Ok(())
    }

    /// Stores reference picker candidates and indexes their display/secondary text in FTS5.
    pub fn save_reference_candidates(
        &self,
        instance_id: &InstanceId,
        candidates: &[ReferenceCandidate],
    ) -> SnResult<()> {
        if candidates.is_empty() {
            return Ok(());
        }

        let now_str = format_ts(Utc::now());
        let mut conn = self.inner.conn.lock();
        let tx = conn.transaction().map_err(cache_err)?;

        upsert_reference_candidates_in_tx(&tx, instance_id, candidates, &now_str)?;

        tx.commit().map_err(cache_err)?;
        Ok(())
    }

    /// Searches cached [`ReferenceCandidate`]s for `target_table` using FTS5 prefix + `LIKE` fallback.
    pub fn search_reference_candidates(
        &self,
        instance_id: &InstanceId,
        target_table: &str,
        query: &str,
        limit: usize,
    ) -> SnResult<Vec<ReferenceCandidate>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let trimmed = query.trim();
        let conn = self.inner.conn.lock();

        if trimmed.is_empty() {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT target_table, sys_id, display_value, secondary
                     FROM reference_candidates
                     WHERE instance_id = ?1 AND target_table = ?2
                     ORDER BY display_value COLLATE NOCASE ASC
                     LIMIT ?3",
                )
                .map_err(cache_err)?;

            let rows = stmt
                .query_map(
                    params![instance_id.as_str(), target_table, limit as i64],
                    row_to_ref_candidate,
                )
                .map_err(cache_err)?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(cache_err)?);
            }
            return Ok(out);
        }

        let mut results = Vec::new();
        let mut seen = HashSet::new();
        let exact_lower = trimmed.to_lowercase();
        let prefix_like = format!("{}%", escape_like(&exact_lower));

        if let Some(fts_query) = build_fts_prefix_query(trimmed) {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT r.target_table, r.sys_id, r.display_value, r.secondary
                     FROM reference_candidates_fts f
                     JOIN reference_candidates r
                       ON r.instance_id = f.instance_id
                      AND r.target_table = f.target_table
                      AND r.sys_id = f.sys_id
                     WHERE reference_candidates_fts MATCH ?1
                       AND f.instance_id = ?2
                       AND f.target_table = ?3
                     ORDER BY
                       (LOWER(r.display_value) = ?4) DESC,
                       (LOWER(r.display_value) LIKE ?5 ESCAPE '\\') DESC,
                       bm25(reference_candidates_fts, 0.0, 0.0, 0.0, 10.0, 3.0),
                       r.display_value COLLATE NOCASE ASC
                     LIMIT ?6",
                )
                .map_err(cache_err)?;

            let rows = stmt
                .query_map(
                    params![
                        fts_query,
                        instance_id.as_str(),
                        target_table,
                        exact_lower,
                        prefix_like,
                        limit as i64
                    ],
                    row_to_ref_candidate,
                )
                .map_err(cache_err)?;

            for row in rows {
                let cand = row.map_err(cache_err)?;
                if seen.insert(cand.sys_id.clone()) {
                    results.push(cand);
                }
            }
        }

        if results.len() < limit {
            let remaining = limit - results.len();
            let contains_like = format!("%{}%", escape_like(&exact_lower));
            let mut stmt = conn
                .prepare_cached(
                    "SELECT target_table, sys_id, display_value, secondary
                     FROM reference_candidates
                     WHERE instance_id = ?1
                       AND target_table = ?2
                       AND (
                         LOWER(display_value) LIKE ?3 ESCAPE '\\'
                         OR LOWER(COALESCE(secondary, '')) LIKE ?3 ESCAPE '\\'
                       )
                     ORDER BY
                       (LOWER(display_value) = ?4) DESC,
                       (LOWER(display_value) LIKE ?5 ESCAPE '\\') DESC,
                       display_value COLLATE NOCASE ASC
                     LIMIT ?6",
                )
                .map_err(cache_err)?;

            let rows = stmt
                .query_map(
                    params![
                        instance_id.as_str(),
                        target_table,
                        contains_like,
                        exact_lower,
                        prefix_like,
                        (remaining + seen.len()) as i64
                    ],
                    row_to_ref_candidate,
                )
                .map_err(cache_err)?;

            for row in rows {
                if results.len() >= limit {
                    break;
                }
                let cand = row.map_err(cache_err)?;
                if seen.insert(cand.sys_id.clone()) {
                    results.push(cand);
                }
            }
        }

        Ok(results)
    }
}

type WarmRecordEntry = ((InstanceId, String, String), (SnRecord, DateTime<Utc>));

fn upsert_records_in_tx(
    tx: &Transaction<'_>,
    cache: &SnCache,
    instance_id: &InstanceId,
    records: &[SnRecord],
    schemas_by_table: &HashMap<String, TableSchema>,
    now: DateTime<Utc>,
    now_str: &str,
) -> SnResult<Vec<WarmRecordEntry>> {
    let mut warm_entries = Vec::with_capacity(records.len());
    let mut harvested_refs: HashMap<(String, String), ReferenceCandidate> = HashMap::new();

    // Load any missing table schemas from SQLite if not already in `moka`.
    let mut local_schemas = schemas_by_table.clone();
    {
        let mut schema_stmt = tx
            .prepare_cached(
                "SELECT schema_json FROM table_schemas WHERE instance_id = ?1 AND table_name = ?2",
            )
            .map_err(cache_err)?;

        for rec in records {
            if !local_schemas.contains_key(&rec.table)
                && let Some(json) = schema_stmt
                    .query_row(params![instance_id.as_str(), rec.table], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()
                    .map_err(cache_err)?
                && let Ok(parsed) = serde_json::from_str::<TableSchema>(&json)
            {
                local_schemas.insert(rec.table.clone(), parsed);
            }
        }
    }

    {
        let mut select_existing = tx
            .prepare_cached(
                "SELECT record_json FROM records
                 WHERE instance_id = ?1 AND table_name = ?2 AND sys_id = ?3",
            )
            .map_err(cache_err)?;

        let mut upsert_stmt = tx
            .prepare_cached(
                "INSERT INTO records
                    (instance_id, table_name, sys_id, number, primary_id,
                     sys_updated_on, sys_mod_count, is_partial, record_json, cached_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(instance_id, table_name, sys_id) DO UPDATE SET
                    number = excluded.number,
                    primary_id = excluded.primary_id,
                    sys_updated_on = excluded.sys_updated_on,
                    sys_mod_count = excluded.sys_mod_count,
                    is_partial = excluded.is_partial,
                    record_json = excluded.record_json,
                    cached_at = excluded.cached_at",
            )
            .map_err(cache_err)?;

        // Track records already merged within the same batch so duplicate sys_ids compose cleanly.
        let mut batch_staged: HashMap<(String, String), SnRecord> = HashMap::new();

        for incoming in records {
            if incoming.sys_id.is_empty() {
                continue;
            }

            let cache_key = (
                instance_id.clone(),
                incoming.table.clone(),
                incoming.sys_id.clone(),
            );

            let resolved = if incoming.is_partial {
                let existing = batch_staged
                    .get(&(incoming.table.clone(), incoming.sys_id.clone()))
                    .cloned()
                    .or_else(|| cache.inner.record_cache.get(&cache_key).map(|(r, _)| r))
                    .or_else(|| {
                        select_existing
                            .query_row(
                                params![instance_id.as_str(), incoming.table, incoming.sys_id],
                                |r| r.get::<_, String>(0),
                            )
                            .optional()
                            .ok()
                            .flatten()
                            .and_then(|json| serde_json::from_str::<SnRecord>(&json).ok())
                    });

                if let Some(mut merged) = existing {
                    merged.merge_from(incoming);
                    merged
                } else {
                    incoming.clone()
                }
            } else {
                incoming.clone()
            };

            let number = {
                let disp = resolved.get_display("number");
                if !disp.is_empty() {
                    Some(disp.to_string())
                } else {
                    let raw = resolved.get_raw("number");
                    (!raw.is_empty()).then(|| raw.to_string())
                }
            };
            let primary_id = resolved.primary_identifier();
            let record_json = serde_json::to_string(&resolved)?;

            upsert_stmt
                .execute(params![
                    instance_id.as_str(),
                    resolved.table,
                    resolved.sys_id,
                    number,
                    primary_id,
                    resolved.sys_updated_on,
                    resolved.sys_mod_count as i64,
                    i32::from(resolved.is_partial),
                    record_json,
                    now_str,
                ])
                .map_err(cache_err)?;

            harvest_record_references(
                &resolved,
                local_schemas.get(&resolved.table),
                &mut harvested_refs,
            );

            batch_staged.insert(
                (resolved.table.clone(), resolved.sys_id.clone()),
                resolved.clone(),
            );
            warm_entries.push((cache_key, (resolved, now)));
        }
    }

    if !harvested_refs.is_empty() {
        let candidates: Vec<ReferenceCandidate> = harvested_refs.into_values().collect();
        upsert_reference_candidates_in_tx(tx, instance_id, &candidates, now_str)?;
    }

    Ok(warm_entries)
}

fn harvest_record_references(
    record: &SnRecord,
    schema: Option<&TableSchema>,
    out: &mut HashMap<(String, String), ReferenceCandidate>,
) {
    // 1. Index the record itself if it carries a meaningful human display label.
    let self_display = ["name", "number", "u_name", "title", "short_description"]
        .into_iter()
        .map(|f| record.get_display(f))
        .find(|v| !v.is_empty());

    if let Some(display_val) = self_display {
        let secondary = ["email", "title", "department", "short_description"]
            .into_iter()
            .map(|f| record.get_display(f))
            .find(|v| !v.is_empty() && *v != display_val)
            .map(str::to_string);

        out.insert(
            (record.table.clone(), record.sys_id.clone()),
            ReferenceCandidate {
                target_table: record.table.clone(),
                sys_id: record.sys_id.clone(),
                display_value: display_val.to_string(),
                secondary,
            },
        );
    }

    // 2. Index populated Reference field values when `TableSchema` knows their `target_table`.
    if let Some(schema) = schema {
        for (field_name, dict_entry) in &schema.fields {
            if let FieldType::Reference { target_table } = &dict_entry.field_type
                && let Some(fv) = record.fields.get(field_name)
                && !fv.value.is_empty()
                && !fv.display_value.is_empty()
                && !fv.value.contains(',')
            {
                let key = (target_table.clone(), fv.value.clone());
                out.entry(key).or_insert_with(|| ReferenceCandidate {
                    target_table: target_table.clone(),
                    sys_id: fv.value.clone(),
                    display_value: fv.display_value.clone(),
                    secondary: None,
                });
            }
        }
    }
}

fn upsert_reference_candidates_in_tx(
    tx: &Transaction<'_>,
    instance_id: &InstanceId,
    candidates: &[ReferenceCandidate],
    now_str: &str,
) -> SnResult<()> {
    let mut upsert_main = tx
        .prepare_cached(
            "INSERT INTO reference_candidates
                (instance_id, target_table, sys_id, display_value, secondary, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(instance_id, target_table, sys_id) DO UPDATE SET
                display_value = excluded.display_value,
                secondary = COALESCE(excluded.secondary, reference_candidates.secondary),
                updated_at = excluded.updated_at",
        )
        .map_err(cache_err)?;

    let mut select_merged = tx
        .prepare_cached(
            "SELECT display_value, secondary
             FROM reference_candidates
             WHERE instance_id = ?1 AND target_table = ?2 AND sys_id = ?3",
        )
        .map_err(cache_err)?;

    let mut del_fts = tx
        .prepare_cached(
            "DELETE FROM reference_candidates_fts
             WHERE instance_id = ?1 AND target_table = ?2 AND sys_id = ?3",
        )
        .map_err(cache_err)?;

    let mut ins_fts = tx
        .prepare_cached(
            "INSERT INTO reference_candidates_fts
                (instance_id, target_table, sys_id, display_value, secondary)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(cache_err)?;

    for cand in candidates {
        if cand.sys_id.is_empty() || cand.target_table.is_empty() || cand.display_value.is_empty() {
            continue;
        }

        upsert_main
            .execute(params![
                instance_id.as_str(),
                cand.target_table,
                cand.sys_id,
                cand.display_value,
                cand.secondary,
                now_str,
            ])
            .map_err(cache_err)?;

        let (disp, sec): (String, Option<String>) = select_merged
            .query_row(
                params![instance_id.as_str(), cand.target_table, cand.sys_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(cache_err)?;

        del_fts
            .execute(params![
                instance_id.as_str(),
                cand.target_table,
                cand.sys_id
            ])
            .map_err(cache_err)?;

        ins_fts
            .execute(params![
                instance_id.as_str(),
                cand.target_table,
                cand.sys_id,
                disp,
                sec.unwrap_or_default(),
            ])
            .map_err(cache_err)?;
    }

    Ok(())
}

fn row_to_ref_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReferenceCandidate> {
    Ok(ReferenceCandidate {
        target_table: row.get(0)?,
        sys_id: row.get(1)?,
        display_value: row.get(2)?,
        secondary: row.get(3)?,
    })
}

pub(crate) fn compute_db_bytes(conn: &Connection, db_path: Option<&std::path::Path>) -> u64 {
    let mut disk_bytes = 0_u64;
    if let Some(path) = db_path {
        if let Ok(meta) = std::fs::metadata(path) {
            disk_bytes = disk_bytes.saturating_add(meta.len());
        }
        let appended_wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        if let Ok(meta) = std::fs::metadata(&appended_wal) {
            disk_bytes = disk_bytes.saturating_add(meta.len());
        }
    }

    let page_count: u64 = conn
        .query_row("PRAGMA page_count;", [], |r| r.get::<_, i64>(0))
        .ok()
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(0);
    let page_size: u64 = conn
        .query_row("PRAGMA page_size;", [], |r| r.get::<_, i64>(0))
        .ok()
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(4096);

    disk_bytes.max(page_count.saturating_mul(page_size))
}
