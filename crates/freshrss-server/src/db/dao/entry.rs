//! `entry` and `entrytmp` access.
//!
//! This module holds the write side of the refresh pipeline (stage → allocate ids → commit →
//! reconcile) and the point mutations the reader makes (read, star). The read side — scopes, state,
//! search and keyset pagination — is a separate builder module, because it is the most intricate
//! SQL in the crate and deserves to be tested as a pure function.

use std::collections::HashMap;

use sqlx::sqlite::SqliteRow;
use sqlx::{Row as _, SqliteConnection};

use crate::db::dao::meta;
use crate::db::dao::row_attributes;
use crate::db::sql::{in_clause, sql, MAX_BIND_PARAMS};
use crate::domain::entry::Entry;
use crate::domain::ids::{now_unix, EntryId};
use crate::error::Result;

pub const COLUMNS: &str = "id, id_user, id_feed, guid, title, author, content, link, date, \
     lastSeen, lastModified, lastUserModified, hash, is_read, is_favorite, tags, attributes, \
     title_folded, content_folded, author_folded, link_folded, tags_folded";

/// The same list qualified for the late-row-lookup join in [`super::entry_query`], which fetches a
/// page of ids first and only then reads the wide columns for those ids.
pub fn columns_aliased(alias: &str) -> String {
    COLUMNS
        .split(", ")
        .map(|c| format!("{alias}.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(in crate::db::dao) fn from_row(row: SqliteRow) -> std::result::Result<Entry, sqlx::Error> {
    let id_raw: i64 = row.try_get("id")?;
    let author: String = row.try_get("author")?;
    let hash_blob: Option<Vec<u8>> = row.try_get("hash")?;
    let is_read: i64 = row.try_get("is_read")?;
    let is_favorite: i64 = row.try_get("is_favorite")?;
    let tags: String = row.try_get("tags")?;
    Ok(Entry {
        id: EntryId::from_micros(id_raw),
        id_user: row.try_get("id_user")?,
        id_feed: row.try_get("id_feed")?,
        guid: row.try_get("guid")?,
        title: row.try_get("title")?,
        authors: Entry::authors_from_db(&author),
        content: row.try_get("content")?,
        link: row.try_get("link")?,
        date: row.try_get("date")?,
        last_seen: row.try_get("lastSeen")?,
        last_modified: row.try_get("lastModified")?,
        last_user_modified: row.try_get("lastUserModified")?,
        // FreshRSS's SQLite DAO stores the raw 16-byte MD5 and hex-encodes on read; we keep it raw.
        hash: hash_blob.and_then(|b| <[u8; 16]>::try_from(b).ok()),
        is_read: is_read != 0,
        is_favorite: is_favorite != 0,
        tags: Entry::tags_from_db(&tags),
        attributes: row_attributes(&row, "attributes")?,
    })
}

// --- reads -----------------------------------------------------------------

pub async fn find_by_id(
    conn: &mut SqliteConnection,
    id: EntryId,
    id_user: i64,
) -> Result<Option<Entry>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM entry WHERE id = ?1 AND id_user = ?2"
    )))
    .bind(id.micros())
    .bind(id_user)
    .try_map(from_row)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn find_by_guid(
    conn: &mut SqliteConnection,
    id_feed: i64,
    guid: &str,
) -> Result<Option<Entry>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM entry WHERE id_feed = ?1 AND guid = ?2"
    )))
    .bind(id_feed)
    .bind(guid)
    .try_map(from_row)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn list_by_ids(
    conn: &mut SqliteConnection,
    id_user: i64,
    ids: &[EntryId],
) -> Result<Vec<Entry>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(MAX_BIND_PARAMS) {
        let placeholders = in_clause(2, chunk.len());
        let mut query = sqlx::query(sql(format!(
            "SELECT {COLUMNS} FROM entry WHERE id_user = ?1 AND id IN {placeholders} ORDER BY id"
        )))
        .bind(id_user);
        for id in chunk {
            query = query.bind(id.micros());
        }
        out.extend(query.try_map(from_row).fetch_all(&mut *conn).await?);
    }
    Ok(out)
}

/// `guid → hash` for the guids a refresh just saw, in one query.
///
/// This is the successor to FreshRSS's `findExistingGuids`: a single lookup that answers both
/// "have I seen this article?" and "has its content changed?", which is what the reconcile loop
/// needs to decide between insert, update and ignore.
pub async fn list_hashes_for_guids(
    conn: &mut SqliteConnection,
    id_feed: i64,
    guids: &[String],
) -> Result<HashMap<String, [u8; 16]>> {
    let mut out = HashMap::with_capacity(guids.len());
    for chunk in guids.chunks(MAX_BIND_PARAMS - 1) {
        let placeholders = in_clause(2, chunk.len());
        let mut query = sqlx::query(sql(format!(
            "SELECT guid, hash FROM entry WHERE id_feed = ?1 AND guid IN {placeholders}"
        )))
        .bind(id_feed);
        for guid in chunk {
            query = query.bind(guid.as_str());
        }
        let rows = query
            .try_map(
                |row: SqliteRow| -> std::result::Result<(String, Option<Vec<u8>>), sqlx::Error> {
                    Ok((row.try_get("guid")?, row.try_get("hash")?))
                },
            )
            .fetch_all(&mut *conn)
            .await?;
        for (guid, hash) in rows {
            // A NULL or wrong-length hash is treated as "unknown", so the reconcile loop will
            // re-write the article rather than deciding it is unchanged.
            let digest = hash.and_then(|bytes| <[u8; 16]>::try_from(bytes).ok());
            if let Some(digest) = digest {
                out.insert(guid, digest);
            }
        }
    }
    Ok(out)
}

/// The most recent N titles in a feed, for `read_when_same_title_in_feed`.
pub async fn list_recent_titles(
    conn: &mut SqliteConnection,
    id_feed: i64,
    limit: usize,
) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT title FROM entry WHERE id_feed = ?1 ORDER BY id DESC LIMIT ?2")
            .bind(id_feed)
            .bind(limit as i64)
            .fetch_all(&mut *conn)
            .await?,
    )
}

/// The most recent N titles across a folder's feeds, for `read_when_same_title_in_category`.
pub async fn list_recent_titles_in_category(
    conn: &mut SqliteConnection,
    id_user: i64,
    id_category: i64,
    limit: usize,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT e.title FROM entry e
         INNER JOIN feed f ON f.id = e.id_feed
         WHERE e.id_user = ?1 AND f.category = ?2
         ORDER BY e.id DESC LIMIT ?3",
    )
    .bind(id_user)
    .bind(id_category)
    .bind(limit as i64)
    .fetch_all(&mut *conn)
    .await?)
}

/// The most recent N guids across a folder's feeds, for `read_when_same_guid_in_category`.
pub async fn list_recent_guids_in_category(
    conn: &mut SqliteConnection,
    id_user: i64,
    id_category: i64,
    limit: usize,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT e.guid FROM entry e
         INNER JOIN feed f ON f.id = e.id_feed
         WHERE e.id_user = ?1 AND f.category = ?2
         ORDER BY e.id DESC LIMIT ?3",
    )
    .bind(id_user)
    .bind(id_category)
    .bind(limit as i64)
    .fetch_all(&mut *conn)
    .await?)
}

// --- staging and commit ----------------------------------------------------

/// Adds a new article to the staging table.
///
/// `INSERT OR IGNORE` because the same guid can appear twice in one refresh (a feed repeating an
/// item, or two feeds in one batch) and `UNIQUE(id_feed, guid)` makes the second a no-op rather
/// than an error.
pub async fn stage(conn: &mut SqliteConnection, entry: &Entry) -> Result<bool> {
    let folded = entry.folded();
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO entrytmp
             (id, id_user, guid, title, author, content, link, date, lastSeen, hash,
              is_read, is_favorite, id_feed, tags, attributes,
              title_folded, content_folded, author_folded, link_folded, tags_folded)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
    )
    .bind(entry.id.micros())
    .bind(entry.id_user)
    .bind(&entry.guid)
    .bind(&entry.title)
    .bind(entry.author_to_db())
    .bind(&entry.content)
    .bind(&entry.link)
    .bind(entry.date)
    .bind(entry.last_seen)
    .bind(entry.hash.map(|h| h.to_vec()))
    .bind(i64::from(entry.is_read))
    .bind(i64::from(entry.is_favorite))
    .bind(entry.id_feed)
    .bind(entry.tags_to_db())
    .bind(entry.attributes.to_db())
    .bind(folded.title)
    .bind(folded.content)
    .bind(folded.author)
    .bind(folded.link)
    .bind(folded.tags)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(inserted > 0)
}

pub async fn count_staged(conn: &mut SqliteConnection, id_user: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM entrytmp WHERE id_user = ?1")
            .bind(id_user)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Moves a user's staged articles into `entry` with ids from the global allocator.
///
/// Ids are assigned in `(date, staged id)` order so they stay chronological within the batch —
/// FreshRSS does the same, and it matters because `ORDER BY id` is the default listing order and a
/// feed that publishes out of order would otherwise show articles shuffled.
///
/// Must be called inside [`crate::db::Db::with_writer`]: it reads and writes the id sequence, and
/// two concurrent callers would hand out the same range.
pub async fn commit_staged(conn: &mut SqliteConnection, id_user: i64) -> Result<usize> {
    let staged = count_staged(conn, id_user).await?;
    if staged == 0 {
        return Ok(0);
    }
    let first_id = meta::reserve_entry_ids(conn, staged as usize).await?;

    // `INSERT OR IGNORE` is the last line of defence against a duplicate `(id_feed, guid)` that
    // slipped past staging — for example an article that arrived via a WebSub push while a pull
    // refresh was in flight.
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO entry
             (id, id_user, guid, title, author, content, link, date, lastSeen, hash,
              is_read, is_favorite, id_feed, tags, attributes,
              title_folded, content_folded, author_folded, link_folded, tags_folded)
         SELECT ?2 + (ROW_NUMBER() OVER (ORDER BY t.date, t.id) - 1),
                t.id_user, t.guid, t.title, t.author, t.content, t.link, t.date, t.lastSeen,
                t.hash, t.is_read, t.is_favorite, t.id_feed, t.tags, t.attributes,
                t.title_folded, t.content_folded, t.author_folded, t.link_folded, t.tags_folded
         FROM entrytmp t
         WHERE t.id_user = ?1",
    )
    .bind(id_user)
    .bind(first_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    sqlx::query("DELETE FROM entrytmp WHERE id_user = ?1")
        .bind(id_user)
        .execute(&mut *conn)
        .await?;
    Ok(inserted as usize)
}

/// Discards a user's staged rows, for a refresh that aborted before committing.
pub async fn discard_staged(conn: &mut SqliteConnection, id_user: i64) -> Result<usize> {
    Ok(sqlx::query("DELETE FROM entrytmp WHERE id_user = ?1")
        .bind(id_user)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}

// --- reconciliation --------------------------------------------------------

/// Updates an article whose content changed upstream.
///
/// Keyed on `(id_feed, guid)`, never on `id`: the id must not change, or the user's read and
/// starred state would be orphaned. `is_read` and `is_favorite` are only written when the caller
/// supplied a value, so an upstream edit does not silently clear either — FreshRSS uses
/// `COALESCE(:x, x)` for exactly this reason.
pub async fn update_existing(conn: &mut SqliteConnection, entry: &Entry) -> Result<bool> {
    let folded = entry.folded();
    let updated = sqlx::query(
        "UPDATE entry SET
             title = ?3,
             author = ?4,
             content = ?5,
             link = ?6,
             date = ?7,
             lastSeen = ?8,
             lastModified = COALESCE(?9, lastModified),
             lastUserModified = COALESCE(?10, lastUserModified),
             hash = ?11,
             is_read = COALESCE(?12, is_read),
             is_favorite = COALESCE(?13, is_favorite),
             tags = ?14,
             attributes = ?15,
             title_folded = ?16,
             content_folded = ?17,
             author_folded = ?18,
             link_folded = ?19,
             tags_folded = ?20
         WHERE id_feed = ?1 AND guid = ?2",
    )
    .bind(entry.id_feed)
    .bind(&entry.guid)
    .bind(&entry.title)
    .bind(entry.author_to_db())
    .bind(&entry.content)
    .bind(&entry.link)
    .bind(entry.date)
    .bind(entry.last_seen)
    .bind(entry.last_modified)
    .bind(entry.last_user_modified)
    .bind(entry.hash.map(|h| h.to_vec()))
    .bind(entry.is_read.then_some(1i64))
    .bind(entry.is_favorite.then_some(1i64))
    .bind(entry.tags_to_db())
    .bind(entry.attributes.to_db())
    .bind(folded.title)
    .bind(folded.content)
    .bind(folded.author)
    .bind(folded.link)
    .bind(folded.tags)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Stamps `lastSeen` on every guid a refresh saw upstream.
///
/// Retention keys off this column: an article that stops appearing keeps its old `lastSeen` and
/// eventually ages out, while one that is still published keeps sliding forward. Articles are
/// never deleted just because they vanished from the feed.
pub async fn update_last_seen(
    conn: &mut SqliteConnection,
    id_feed: i64,
    guids: &[String],
    mtime: i64,
) -> Result<usize> {
    let mut total = 0;
    for chunk in guids.chunks(MAX_BIND_PARAMS - 2) {
        let placeholders = in_clause(3, chunk.len());
        let mut query = sqlx::query(sql(format!(
            "UPDATE entry SET lastSeen = ?1 WHERE id_feed = ?2 AND guid IN {placeholders}"
        )))
        .bind(mtime)
        .bind(id_feed);
        for guid in chunk {
            query = query.bind(guid.as_str());
        }
        total += query.execute(&mut *conn).await?.rows_affected() as usize;
    }
    Ok(total)
}

/// Stamps `lastSeen` on a 304 / unchanged feed.
///
/// Nothing was re-parsed, so there is no guid list to work from; instead every article already
/// seen at the previous refresh is advanced. Without this, a feed that rarely changes would have
/// its whole backlog age out on the retention schedule even though it is perfectly healthy.
pub async fn update_last_seen_unchanged(
    conn: &mut SqliteConnection,
    id_feed: i64,
    mtime: i64,
) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE entry SET lastSeen = ?2
         WHERE id_feed = ?1
           AND lastSeen >= (SELECT lastUpdate FROM feed f WHERE f.id = ?1)",
    )
    .bind(id_feed)
    .bind(mtime)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

// --- point mutations -------------------------------------------------------

/// Marks articles read or unread. Returns how many actually changed.
///
/// Only rows whose flag differs are touched, so `lastUserModified` is not churned on an article
/// that was already read — the column means "when the user last acted", not "when the last request
/// mentioned it".
pub async fn mark_read(
    conn: &mut SqliteConnection,
    id_user: i64,
    ids: &[EntryId],
    read: bool,
) -> Result<usize> {
    let flag = i64::from(read);
    let now = now_unix();
    let mut total = 0;
    for chunk in ids.chunks(MAX_BIND_PARAMS - 3) {
        let placeholders = in_clause(4, chunk.len());
        let mut query = sqlx::query(sql(format!(
            "UPDATE entry SET is_read = ?1, lastUserModified = ?2
             WHERE id_user = ?3 AND is_read <> ?1 AND id IN {placeholders}"
        )))
        .bind(flag)
        .bind(now)
        .bind(id_user);
        for id in chunk {
            query = query.bind(id.micros());
        }
        total += query.execute(&mut *conn).await?.rows_affected() as usize;
    }
    Ok(total)
}

pub async fn mark_favorite(
    conn: &mut SqliteConnection,
    id_user: i64,
    ids: &[EntryId],
    favorite: bool,
) -> Result<usize> {
    let flag = i64::from(favorite);
    let now = now_unix();
    let mut total = 0;
    for chunk in ids.chunks(MAX_BIND_PARAMS - 3) {
        let placeholders = in_clause(4, chunk.len());
        let mut query = sqlx::query(sql(format!(
            "UPDATE entry SET is_favorite = ?1, lastUserModified = ?2
             WHERE id_user = ?3 AND is_favorite <> ?1 AND id IN {placeholders}"
        )))
        .bind(flag)
        .bind(now)
        .bind(id_user);
        for id in chunk {
            query = query.bind(id.micros());
        }
        total += query.execute(&mut *conn).await?.rows_affected() as usize;
    }
    Ok(total)
}

/// Marks every article in one feed read, up to `id_max`.
///
/// The `id <= ?` bound is what makes "mark all as read" mean "everything I had loaded", not
/// "everything including the articles that arrived while I was reading".
pub async fn mark_read_feed(
    conn: &mut SqliteConnection,
    id_feed: i64,
    id_max: EntryId,
    now: i64,
) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE entry SET is_read = 1, lastUserModified = ?3
         WHERE id_feed = ?1 AND is_read = 0 AND id <= ?2",
    )
    .bind(id_feed)
    .bind(id_max.micros())
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

pub async fn mark_read_category(
    conn: &mut SqliteConnection,
    id_user: i64,
    id_category: i64,
    id_max: EntryId,
    now: i64,
) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE entry SET is_read = 1, lastUserModified = ?4
         WHERE id_user = ?1 AND is_read = 0 AND id <= ?3
           AND id_feed IN (SELECT f.id FROM feed f WHERE f.category = ?2)",
    )
    .bind(id_user)
    .bind(id_category)
    .bind(id_max.micros())
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

pub async fn mark_read_all(
    conn: &mut SqliteConnection,
    id_user: i64,
    id_max: EntryId,
    now: i64,
) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE entry SET is_read = 1, lastUserModified = ?3
         WHERE id_user = ?1 AND is_read = 0 AND id <= ?2",
    )
    .bind(id_user)
    .bind(id_max.micros())
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

// --- aggregates ------------------------------------------------------------

/// Total, unread and starred counts for a user in one pass.
///
/// The three aggregates are aliased so they can be read by name: `COUNT(*)` has no column name of
/// its own, and a positional read of a wide select is exactly the bug the rest of this module
/// avoids.
pub async fn count_states(conn: &mut SqliteConnection, id_user: i64) -> Result<CountStates> {
    Ok(sqlx::query_as(
        "SELECT COUNT(*) AS total,
                COALESCE(SUM(CASE WHEN is_read = 0 THEN 1 ELSE 0 END), 0) AS unread,
                COALESCE(SUM(CASE WHEN is_favorite = 1 THEN 1 ELSE 0 END), 0) AS starred
         FROM entry WHERE id_user = ?1",
    )
    .bind(id_user)
    .fetch_one(&mut *conn)
    .await?)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct CountStates {
    pub total: i64,
    pub unread: i64,
    pub starred: i64,
}

impl CountStates {
    pub fn read(&self) -> i64 {
        self.total - self.unread
    }
}

/// The newest article id for a user, used as the default `id_max` snapshot fence.
pub async fn max_id(conn: &mut SqliteConnection, id_user: i64) -> Result<Option<EntryId>> {
    let raw: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM entry WHERE id_user = ?1")
        .bind(id_user)
        .fetch_one(&mut *conn)
        .await?;
    Ok(raw.map(EntryId::from_micros))
}

/// The publication time of the newest article, for the "last updated" display.
pub async fn newest_article_date(conn: &mut SqliteConnection, id_user: i64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT MAX(date) FROM entry WHERE id_user = ?1")
            .bind(id_user)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Deletes every article a user owns. Used by account deletion and by tests.
pub async fn delete_all_for_user(conn: &mut SqliteConnection, id_user: i64) -> Result<usize> {
    Ok(sqlx::query("DELETE FROM entry WHERE id_user = ?1")
        .bind(id_user)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}
