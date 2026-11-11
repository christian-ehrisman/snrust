//! Auxiliary models for cache envelopes, favorites, recents, TTLs, and diagnostics.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Envelope returned by cache lookups carrying freshness metadata for stale-while-revalidate (SWR) flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedItem<T> {
    pub data: T,
    pub cached_at: DateTime<Utc>,
    pub is_stale: bool,
}

impl<T> CachedItem<T> {
    /// Creates a `CachedItem` and computes `is_stale` relative to `ttl`.
    pub fn from_timestamp(data: T, cached_at: DateTime<Utc>, ttl: Duration) -> Self {
        let is_stale = Utc::now()
            .signed_duration_since(cached_at)
            .to_std()
            .is_ok_and(|elapsed| elapsed > ttl);
        Self {
            data,
            cached_at,
            is_stale,
        }
    }
}

/// Target payload stored for a pinned sidebar favorite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FavoriteKind {
    Table {
        table: String,
        label: String,
        query: String,
    },
    Record {
        table: String,
        sys_id: String,
        title: String,
    },
}

/// User-saved sidebar bookmark scoped to an [`sn_core::InstanceId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteItem {
    pub id: String,
    pub kind: FavoriteKind,
    pub title: String,
    pub subtitle: Option<String>,
}

impl FavoriteItem {
    /// Convenience constructor for bookmarking a filtered or unfiltered table list.
    pub fn for_table(
        id: impl Into<String>,
        table: impl Into<String>,
        label: impl Into<String>,
        query: impl Into<String>,
        subtitle: Option<String>,
    ) -> Self {
        let label_str = label.into();
        Self {
            id: id.into(),
            title: label_str.clone(),
            kind: FavoriteKind::Table {
                table: table.into(),
                label: label_str,
                query: query.into(),
            },
            subtitle,
        }
    }

    /// Convenience constructor for bookmarking a specific ServiceNow record.
    pub fn for_record(
        id: impl Into<String>,
        table: impl Into<String>,
        sys_id: impl Into<String>,
        title: impl Into<String>,
        subtitle: Option<String>,
    ) -> Self {
        let title_str = title.into();
        Self {
            id: id.into(),
            title: title_str.clone(),
            kind: FavoriteKind::Record {
                table: table.into(),
                sys_id: sys_id.into(),
                title: title_str,
            },
            subtitle,
        }
    }
}

/// Recently visited table or record entry (capped at 50 per instance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentItem {
    pub table: String,
    pub sys_id: Option<String>,
    pub title: String,
    pub subtitle: Option<String>,
    pub visited_at: DateTime<Utc>,
}

impl RecentItem {
    /// Builds a recent entry for a specific record visit stamped with `Utc::now()`.
    pub fn for_record(
        table: impl Into<String>,
        sys_id: impl Into<String>,
        title: impl Into<String>,
        subtitle: Option<String>,
    ) -> Self {
        Self {
            table: table.into(),
            sys_id: Some(sys_id.into()),
            title: title.into(),
            subtitle,
            visited_at: Utc::now(),
        }
    }

    /// Builds a recent entry for a table list visit stamped with `Utc::now()`.
    pub fn for_table(
        table: impl Into<String>,
        title: impl Into<String>,
        subtitle: Option<String>,
    ) -> Self {
        Self {
            table: table.into(),
            sys_id: None,
            title: title.into(),
            subtitle,
            visited_at: Utc::now(),
        }
    }
}

/// Storage footprint and row counts for diagnostics and cache settings UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub record_count: usize,
    pub schema_count: usize,
    pub db_bytes: u64,
}

/// Freshness windows used to mark [`CachedItem::is_stale`] for SWR background revalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTtlConfig {
    pub schema_ttl: Duration,
    pub record_ttl: Duration,
    pub list_ttl: Duration,
}

impl Default for CacheTtlConfig {
    fn default() -> Self {
        Self {
            schema_ttl: Duration::from_secs(24 * 60 * 60),
            record_ttl: Duration::from_secs(5 * 60),
            list_ttl: Duration::from_secs(60),
        }
    }
}
