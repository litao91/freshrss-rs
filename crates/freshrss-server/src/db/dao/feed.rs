//! `feed` table access, including the due-feed selection query that drives the refresh scheduler.

use sqlx::sqlite::SqliteRow;
use sqlx::{Connection as _, Row as _, SqliteConnection};

use crate::db::dao::row_attributes;
use crate::db::sql::{in_clause, sql, MAX_BIND_PARAMS};
use crate::domain::attributes::Attributes;
use crate::domain::feed::Feed;
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;
use crate::error::Result;

pub const COLUMNS: &str = "id, id_user, url, kind, category, name, website, description, \
     lastUpdate, priority, pathEntries, pathEntriesFilter, httpAuth, error, ttl, attributes, \
     cache_nbEntries, cache_nbUnreads";

/// The same list qualified for the join in [`list_due`].
const COLUMNS_ALIASED: &str = "f.id, f.id_user, f.url, f.kind, f.category, f.name, f.website, \
     f.description, f.lastUpdate, f.priority, f.pathEntries, f.pathEntriesFilter, f.httpAuth, \
     f.error, f.ttl, f.attributes, f.cache_nbEntries, f.cache_nbUnreads";

fn from_row(row: SqliteRow) -> std::result::Result<Feed, sqlx::Error> {
    let kind_raw: i64 = row.try_get("kind")?;
    let priority_raw: i64 = row.try_get("priority")?;
    let ttl_raw: i64 = row.try_get("ttl")?;
    Ok(Feed {
        id: row.try_get("id")?,
        id_user: row.try_get("id_user")?,
        url: row.try_get("url")?,
        kind: FeedKind::from_i16(kind_raw as i16).unwrap_or(FeedKind::Rss),
        category: row.try_get("category")?,
        name: row.try_get("name")?,
        website: row.try_get("website")?,
        description: row.try_get("description")?,
        last_update: row.try_get("lastUpdate")?,
        priority: Priority::new(priority_raw as i8),
        path_entries: row.try_get("pathEntries")?,
        path_entries_filter: row.try_get("pathEntriesFilter")?,
        http_auth: row.try_get("httpAuth")?,
        error: row.try_get("error")?,
        ttl: Ttl::from_raw(ttl_raw as i32),
        attributes: row_attributes(&row, "attributes")?,
        cache_nb_entries: row.try_get("cache_nbEntries")?,
        cache_nb_unreads: row.try_get("cache_nbUnreads")?,
    })
}

pub async fn insert(conn: &mut SqliteConnection, feed: &Feed) -> Result<i64> {
    let id = sqlx::query(
        "INSERT INTO feed (id_user, url, kind, category, name, website, description, lastUpdate,
                            priority, pathEntries, pathEntriesFilter, httpAuth, error, ttl,
                            attributes, cache_nbEntries, cache_nbUnreads)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
    )
    .bind(feed.id_user)
    .bind(&feed.url)
    .bind(i64::from(feed.kind.as_i16()))
    .bind(feed.category)
    .bind(&feed.name)
    .bind(&feed.website)
    .bind(&feed.description)
    .bind(feed.last_update)
    .bind(i64::from(feed.priority.get()))
    .bind(&feed.path_entries)
    .bind(&feed.path_entries_filter)
    .bind(&feed.http_auth)
    .bind(feed.error)
    .bind(feed.ttl.raw())
    .bind(feed.attributes.to_db())
    .bind(feed.cache_nb_entries)
    .bind(feed.cache_nb_unreads)
    .execute(&mut *conn)
    .await?
    .last_insert_rowid();
    Ok(id)
}

pub async fn find_by_id(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
) -> Result<Option<Feed>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM feed WHERE id = ?1 AND id_user = ?2"
    )))
    .bind(id)
    .bind(id_user)
    .try_map(from_row)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Duplicate check before subscribing. Scoped by user, so two people may follow the same feed.
pub async fn find_by_url(
    conn: &mut SqliteConnection,
    id_user: i64,
    url: &str,
) -> Result<Option<Feed>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM feed WHERE id_user = ?1 AND url = ?2 LIMIT 1"
    )))
    .bind(id_user)
    .bind(url)
    .try_map(from_row)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn list(conn: &mut SqliteConnection, id_user: i64) -> Result<Vec<Feed>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM feed WHERE id_user = ?1 ORDER BY name COLLATE NOCASE"
    )))
    .bind(id_user)
    .try_map(from_row)
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn list_by_category(
    conn: &mut SqliteConnection,
    id_user: i64,
    category: i64,
) -> Result<Vec<Feed>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM feed WHERE id_user = ?1 AND category = ?2
         ORDER BY name COLLATE NOCASE"
    )))
    .bind(id_user)
    .bind(category)
    .try_map(from_row)
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn count(conn: &mut SqliteConnection, id_user: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM feed WHERE id_user = ?1")
            .bind(id_user)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Feeds whose refresh is due, oldest first.
///
/// Ports `FreshRSS_FeedDAO::listFeedsOrderUpdate`, with one change forced by the shared database:
/// `ttl = 0` means "use the owner's default", and that default lives in *that user's* config, so
/// the query has to join `user` and read it per row. FreshRSS takes the value as a scalar because
/// its database only ever has one user in it; passing a global 3600 here would silently ignore
/// per-user configuration.
///
/// `ttl >= 0` is the mute filter — a negative ttl means muted, not "very often".
///
/// The `CASE WHEN error > lastUpdate` term means a feed that keeps failing is retried on its own
/// error timestamp rather than being hammered every tick.
pub async fn list_due(conn: &mut SqliteConnection, now: i64, limit: usize) -> Result<Vec<Feed>> {
    // `json_extract` reads `ttl_default` out of the user's config blob. It stays there rather than
    // being promoted to a column so there is exactly one copy of the value; the query runs once per
    // scheduler tick over an indexed candidate set, so the extraction is not on a hot path.
    let statement = sql(format!(
        "SELECT {COLUMNS_ALIASED} FROM feed f
         INNER JOIN user u ON u.id = f.id_user
         WHERE f.ttl >= 0 AND u.enabled = 1
           AND (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END)
               < (?1 + 60 - COALESCE(
                     NULLIF(f.ttl, 0),
                     CAST(json_extract(u.config, '$.ttl_default') AS INTEGER),
                     3600))
         ORDER BY (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END) ASC
         LIMIT ?2"
    ));
    Ok(sqlx::query(statement)
        .bind(now)
        .bind(limit as i64)
        .try_map(from_row)
        .fetch_all(&mut *conn)
        .await?)
}

/// Feeds belonging to one user whose refresh is due. Same selection as [`list_due`], scoped.
pub async fn list_due_for_user(
    conn: &mut SqliteConnection,
    id_user: i64,
    now: i64,
    limit: usize,
) -> Result<Vec<Feed>> {
    let statement = sql(format!(
        "SELECT {COLUMNS_ALIASED} FROM feed f
         INNER JOIN user u ON u.id = f.id_user
         WHERE f.id_user = ?3 AND f.ttl >= 0
           AND (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END)
               < (?1 + 60 - COALESCE(
                     NULLIF(f.ttl, 0),
                     CAST(json_extract(u.config, '$.ttl_default') AS INTEGER),
                     3600))
         ORDER BY (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END) ASC
         LIMIT ?2"
    ));
    Ok(sqlx::query(statement)
        .bind(now)
        .bind(limit as i64)
        .bind(id_user)
        .try_map(from_row)
        .fetch_all(&mut *conn)
        .await?)
}

/// Feeds with a recorded failure or muted, for the reader's "Feeds with problems" dialog.
pub async fn list_with_problems(conn: &mut SqliteConnection, id_user: i64) -> Result<Vec<Feed>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM feed
         WHERE id_user = ?1 AND (error > 0 OR ttl < 0)
         ORDER BY error DESC, name COLLATE NOCASE"
    )))
    .bind(id_user)
    .try_map(from_row)
    .fetch_all(&mut *conn)
    .await?)
}

// --- updates ---------------------------------------------------------------

pub async fn update_url(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    url: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET url = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(url)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_name(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    name: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET name = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(name)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_category(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    category: Option<i64>,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET category = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(category)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_priority(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    priority: Priority,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET priority = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(i64::from(priority.get()))
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_ttl(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    ttl: Ttl,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET ttl = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(ttl.raw())
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_kind(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    kind: FeedKind,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET kind = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(i64::from(kind.as_i16()))
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_scraping(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    path_entries: Option<&str>,
    path_entries_filter: Option<&str>,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE feed SET pathEntries = ?3, pathEntriesFilter = ?4 WHERE id = ?1 AND id_user = ?2",
    )
    .bind(id)
    .bind(id_user)
    .bind(path_entries)
    .bind(path_entries_filter)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0)
}

pub async fn update_http_auth(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    http_auth: Option<&str>,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET httpAuth = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(http_auth)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_attributes(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    attributes: &Attributes,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET attributes = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(attributes.to_db())
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_website_description(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    website: &str,
    description: &str,
) -> Result<bool> {
    Ok(
        sqlx::query(
            "UPDATE feed SET website = ?3, description = ?4 WHERE id = ?1 AND id_user = ?2",
        )
        .bind(id)
        .bind(id_user)
        .bind(website)
        .bind(description)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            > 0,
    )
}

/// Records a successful refresh. Clears `error` unconditionally — FreshRSS does the same, so a
/// feed that recovers stops showing in the problems dialog without a separate step.
pub async fn update_last_update(conn: &mut SqliteConnection, id: i64, mtime: i64) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET lastUpdate = ?2, error = 0 WHERE id = ?1")
            .bind(id)
            .bind(mtime)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Records a failed refresh as a timestamp.
pub async fn update_last_error(conn: &mut SqliteConnection, id: i64, error: i64) -> Result<bool> {
    Ok(sqlx::query("UPDATE feed SET error = ?2 WHERE id = ?1")
        .bind(id)
        .bind(error)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

/// Recomputes the denormalized entry counters.
///
/// FreshRSS keeps these because its sidebar is rendered server-side and cannot afford a count per
/// feed; we keep them for the same reason — `GET /tree` builds the whole sidebar from one query.
/// The correlated subqueries replace the `UPDATE … LEFT JOIN` form, which SQLite does not support.
pub async fn update_cached_values(conn: &mut SqliteConnection, ids: &[i64]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut total = 0;
    for chunk in ids.chunks(MAX_BIND_PARAMS) {
        let placeholders = in_clause(1, chunk.len());
        let mut query = sqlx::query(sql(format!(
            "UPDATE feed SET
                 cache_nbEntries = (SELECT COUNT(*) FROM entry e WHERE e.id_feed = feed.id),
                 cache_nbUnreads = (SELECT COUNT(*) FROM entry e
                                    WHERE e.id_feed = feed.id AND e.is_read = 0)
             WHERE id IN {placeholders}"
        )));
        for id in chunk {
            query = query.bind(*id);
        }
        total += query.execute(&mut *conn).await?.rows_affected() as usize;
    }
    Ok(total)
}

/// Recomputes the counters for every feed a user owns.
pub async fn update_all_cached_values(conn: &mut SqliteConnection, id_user: i64) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE feed SET
             cache_nbEntries = (SELECT COUNT(*) FROM entry e WHERE e.id_feed = feed.id),
             cache_nbUnreads = (SELECT COUNT(*) FROM entry e
                                WHERE e.id_feed = feed.id AND e.is_read = 0)
         WHERE id_user = ?1",
    )
    .bind(id_user)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

/// Adjusts one feed's unread counter by a delta, clamped at zero.
///
/// Used by the single-entry read/star paths, where recomputing from `entry` would be a full scan
/// for a change of one.
pub async fn bump_cache_unreads(
    conn: &mut SqliteConnection,
    id_feed: i64,
    delta: i64,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE feed SET cache_nbUnreads = MAX(0, cache_nbUnreads + ?2) WHERE id = ?1")
            .bind(id_feed)
            .bind(delta)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Deletes a feed and, via `ON DELETE CASCADE`, its entries and their label links.
pub async fn delete(conn: &mut SqliteConnection, id: i64, id_user: i64) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM feed WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Empties a feed's articles but keeps the subscription. Resets `lastUpdate` so the next refresh
/// treats the feed as new and re-fetches everything, matching FreshRSS's `truncate`.
pub async fn truncate_entries(conn: &mut SqliteConnection, id: i64, id_user: i64) -> Result<usize> {
    let mut tx = conn.begin().await?;
    let deleted = sqlx::query("DELETE FROM entry WHERE id_feed = ?1 AND id_user = ?2")
        .bind(id)
        .bind(id_user)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;
    sqlx::query(
        "UPDATE feed SET cache_nbEntries = 0, cache_nbUnreads = 0, lastUpdate = 0
         WHERE id = ?1 AND id_user = ?2",
    )
    .bind(id)
    .bind(id_user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(deleted)
}
