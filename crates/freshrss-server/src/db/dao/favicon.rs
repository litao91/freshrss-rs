//! `favicon` table access — the on-disk icon cache behind the feed-icon endpoint.
//!
//! Keyed by feed, so `ON DELETE CASCADE` reclaims a blob when its subscription is removed. Every
//! read is by `id_feed`, which is already user-scoped through the `feed` row it references; the
//! handler that calls this has loaded that feed under the caller's own id.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row as _, SqliteConnection};

use crate::error::Result;

/// One cached icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedIcon {
    /// The address the bytes came from, kept so a feed whose icon moved is treated as a cache miss.
    pub url: String,
    pub content_type: String,
    pub body: Vec<u8>,
    /// Unix seconds of the fetch; the endpoint compares it against the TTL to decide freshness.
    pub fetched_at: i64,
}

fn from_row(row: SqliteRow) -> std::result::Result<CachedIcon, sqlx::Error> {
    Ok(CachedIcon {
        url: row.try_get("url")?,
        content_type: row.try_get("content_type")?,
        body: row.try_get("body")?,
        fetched_at: row.try_get("fetched_at")?,
    })
}

pub async fn find(conn: &mut SqliteConnection, id_feed: i64) -> Result<Option<CachedIcon>> {
    Ok(
        sqlx::query("SELECT url, content_type, body, fetched_at FROM favicon WHERE id_feed = ?1")
            .bind(id_feed)
            .try_map(from_row)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// Stores or replaces a feed's cached icon.
pub async fn upsert(
    conn: &mut SqliteConnection,
    id_feed: i64,
    url: &str,
    content_type: &str,
    body: &[u8],
    fetched_at: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO favicon (id_feed, url, content_type, body, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (id_feed) DO UPDATE SET
             url = excluded.url,
             content_type = excluded.content_type,
             body = excluded.body,
             fetched_at = excluded.fetched_at",
    )
    .bind(id_feed)
    .bind(url)
    .bind(content_type)
    .bind(body)
    .bind(fetched_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
