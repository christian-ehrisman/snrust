//! SQLite PRAGMA tuning, schema migrations, and FTS5/LIKE query sanitization helpers.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Connection;
use sn_core::{SnError, SnResult};

pub(crate) fn cache_err(err: impl std::fmt::Display) -> SnError {
    SnError::Cache(err.to_string())
}

/// Configures high-throughput WAL PRAGMAs and ensures all Tier 1/2/3 tables and FTS5 indexes exist.
pub(crate) fn initialize_database(conn: &Connection) -> SnResult<()> {
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(cache_err)?;

    // `execute_batch` steps and discards PRAGMA result rows cleanly for both file and `:memory:` DBs.
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA temp_store = MEMORY;
        PRAGMA mmap_size = 268435456;

        -- Tier 3: Instance profiles & workspace preferences
        CREATE TABLE IF NOT EXISTS instances (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            base_url TEXT NOT NULL,
            client_id TEXT NOT NULL,
            has_client_secret INTEGER NOT NULL DEFAULT 0,
            callback_port INTEGER NOT NULL,
            scopes TEXT NOT NULL,
            accent_hex TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_connected_at TEXT,
            account_json TEXT
        );

        CREATE TABLE IF NOT EXISTS workspace_kv (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS account_metadata (
            instance_id TEXT NOT NULL,
            key TEXT NOT NULL,
            value_json TEXT NOT NULL,
            PRIMARY KEY (instance_id, key)
        );

        CREATE TABLE IF NOT EXISTS favorites (
            instance_id TEXT NOT NULL,
            id TEXT NOT NULL,
            kind_json TEXT NOT NULL,
            title TEXT NOT NULL,
            subtitle TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, id)
        );

        CREATE TABLE IF NOT EXISTS recent_items (
            instance_id TEXT NOT NULL,
            target_key TEXT NOT NULL,
            table_name TEXT NOT NULL,
            sys_id TEXT,
            title TEXT NOT NULL,
            subtitle TEXT,
            visited_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, target_key)
        );
        CREATE INDEX IF NOT EXISTS idx_recent_items_visited
            ON recent_items (instance_id, visited_at DESC);

        CREATE TABLE IF NOT EXISTS form_layouts (
            instance_id TEXT NOT NULL,
            table_name TEXT NOT NULL,
            layout_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, table_name)
        );

        CREATE TABLE IF NOT EXISTS list_columns (
            instance_id TEXT NOT NULL,
            table_name TEXT NOT NULL,
            columns_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, table_name)
        );

        -- Tier 1: Table catalog + FTS5 + merged TableSchema cache
        CREATE TABLE IF NOT EXISTS table_catalog (
            instance_id TEXT NOT NULL,
            name TEXT NOT NULL,
            sys_id TEXT NOT NULL,
            label TEXT NOT NULL,
            super_class TEXT,
            number_prefix TEXT,
            is_extendable INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (instance_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_table_catalog_label
            ON table_catalog (instance_id, label COLLATE NOCASE);

        CREATE VIRTUAL TABLE IF NOT EXISTS table_catalog_fts USING fts5(
            instance_id UNINDEXED,
            name,
            label,
            number_prefix,
            tokenize = 'unicode61'
        );

        CREATE TABLE IF NOT EXISTS table_schemas (
            instance_id TEXT NOT NULL,
            table_name TEXT NOT NULL,
            schema_json TEXT NOT NULL,
            cached_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, table_name)
        );

        -- Tier 2: Normalized records, SWR list pages, journals, and reference candidate FTS5
        CREATE TABLE IF NOT EXISTS records (
            instance_id TEXT NOT NULL,
            table_name TEXT NOT NULL,
            sys_id TEXT NOT NULL,
            number TEXT,
            primary_id TEXT NOT NULL,
            sys_updated_on TEXT,
            sys_mod_count INTEGER NOT NULL DEFAULT 0,
            is_partial INTEGER NOT NULL DEFAULT 0,
            record_json TEXT NOT NULL,
            cached_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, table_name, sys_id)
        );
        CREATE INDEX IF NOT EXISTS idx_records_number
            ON records (instance_id, table_name, number COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_records_primary_id
            ON records (instance_id, table_name, primary_id COLLATE NOCASE);

        CREATE TABLE IF NOT EXISTS list_pages (
            instance_id TEXT NOT NULL,
            table_name TEXT NOT NULL,
            encoded_query TEXT NOT NULL,
            page_offset INTEGER NOT NULL,
            page_limit INTEGER NOT NULL,
            sys_ids_json TEXT NOT NULL,
            total_count INTEGER,
            max_updated_on TEXT,
            cached_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, table_name, encoded_query, page_offset, page_limit)
        );
        CREATE INDEX IF NOT EXISTS idx_list_pages_table
            ON list_pages (instance_id, table_name);

        CREATE TABLE IF NOT EXISTS journals (
            instance_id TEXT NOT NULL,
            table_name TEXT NOT NULL,
            record_sys_id TEXT NOT NULL,
            sys_id TEXT NOT NULL,
            element TEXT NOT NULL,
            value TEXT NOT NULL,
            sys_created_on TEXT NOT NULL,
            sys_created_by TEXT NOT NULL,
            PRIMARY KEY (instance_id, table_name, record_sys_id, sys_id)
        );
        CREATE INDEX IF NOT EXISTS idx_journals_record_time
            ON journals (instance_id, table_name, record_sys_id, sys_created_on DESC);

        CREATE TABLE IF NOT EXISTS reference_candidates (
            instance_id TEXT NOT NULL,
            target_table TEXT NOT NULL,
            sys_id TEXT NOT NULL,
            display_value TEXT NOT NULL,
            secondary TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, target_table, sys_id)
        );
        CREATE INDEX IF NOT EXISTS idx_ref_candidates_display
            ON reference_candidates (instance_id, target_table, display_value COLLATE NOCASE);

        CREATE VIRTUAL TABLE IF NOT EXISTS reference_candidates_fts USING fts5(
            instance_id UNINDEXED,
            target_table UNINDEXED,
            sys_id UNINDEXED,
            display_value,
            secondary,
            tokenize = 'unicode61'
        );
        ",
    )
    .map_err(cache_err)?;

    let has_account: bool = conn
        .prepare("PRAGMA table_info(instances)")
        .map_err(cache_err)?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(cache_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(cache_err)?
        .iter()
        .any(|name| name == "account_json");
    if !has_account {
        conn.execute("ALTER TABLE instances ADD COLUMN account_json TEXT", [])
            .map_err(cache_err)?;
        // Older schemas omitted global audit fields and could use a truncated parent catalog.
        conn.execute("DELETE FROM table_schemas", [])
            .map_err(cache_err)?;
    }

    Ok(())
}

/// Fixed-width UTC RFC3339 representation so SQLite `ORDER BY` preserves chronological order.
pub(crate) fn format_ts(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(crate) fn parse_ts(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Sanitizes arbitrary user input into an FTS5 prefix expression (`"inc"* AND "serv"*`).
///
/// Splitting on non-alphanumeric boundaries makes `sc_req` match `sc_req_item` under `unicode61`
/// while stripping all FTS5 operator metacharacters.
pub(crate) fn build_fts_prefix_query(raw: &str) -> Option<String> {
    let tokens: Vec<String> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{}\"*", tok.to_lowercase()))
        .collect();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" AND "))
    }
}

/// Escapes SQL `LIKE` wildcards for `LIKE ? ESCAPE '\'`.
pub(crate) fn escape_like(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    for ch in raw.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}
