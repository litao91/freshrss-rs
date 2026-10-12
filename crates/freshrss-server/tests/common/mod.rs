//! Scaffolding the integration tests share.
//!
//! `freshrss_server::db::support` is `pub(crate)`, and an integration test is its own crate, so it
//! cannot reach it; this is the copy the test binaries get. It holds the two things more than one of
//! them needs identically: a migrated `:memory:` database, and an `entry` row seeded the way the DAOs
//! write one — folded search columns included. A harness that skipped those would be comparing the
//! search backends against empty strings.

use freshrss_server::db::Db;
use freshrss_server::Result;
use sqlx::SqliteConnection;

/// A migrated in-memory database.
pub(crate) async fn db() -> Db {
    Db::in_memory().await.expect("in-memory db")
}

/// One `entry` row to seed, named for the columns it fills.
///
/// The five folded columns are derived from these values by [`insert_entry`] rather than passed in,
/// so a seed can neither forget them nor disagree with the plain text stored beside them.
pub(crate) struct SeededEntry {
    pub id: i64,
    pub id_user: i64,
    pub guid: String,
    pub title: String,
    pub author: String,
    pub content: String,
    pub link: String,
    /// Publication time, and — in both corpora — the ingestion time recorded in `lastSeen`.
    pub date: i64,
    pub last_modified: Option<i64>,
    pub last_user_modified: Option<i64>,
    pub hash: Vec<u8>,
    pub is_read: bool,
    pub is_favorite: bool,
    pub id_feed: i64,
    pub tags: String,
}

/// Inserts `row` into `entry`, computing its folded search columns from the plain text.
pub(crate) async fn insert_entry(conn: &mut SqliteConnection, row: &SeededEntry) -> Result<()> {
    sqlx::query(
        "INSERT INTO entry
           (id, id_user, guid, title, author, content, link, date, lastSeen,
            lastModified, lastUserModified, hash, is_read, is_favorite, id_feed, tags,
            title_folded, content_folded, author_folded, link_folded, tags_folded)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(row.id)
    .bind(row.id_user)
    .bind(row.guid.as_str())
    .bind(row.title.as_str())
    .bind(row.author.as_str())
    .bind(row.content.as_str())
    .bind(row.link.as_str())
    .bind(row.date)
    .bind(row.date)
    .bind(row.last_modified)
    .bind(row.last_user_modified)
    .bind(row.hash.as_slice())
    .bind(row.is_read)
    .bind(row.is_favorite)
    .bind(row.id_feed)
    .bind(row.tags.as_str())
    .bind(freshrss_search::fold(&row.title))
    .bind(freshrss_search::fold(&row.content))
    .bind(freshrss_search::fold(&row.author))
    .bind(freshrss_search::fold(&row.link))
    .bind(freshrss_search::fold(&row.tags))
    .execute(&mut *conn)
    .await?;
    Ok(())
}
