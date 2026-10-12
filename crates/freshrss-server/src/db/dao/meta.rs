//! `meta` table access, including the global entry-id allocator.

use sqlx::SqliteConnection;

use crate::domain::ids::now_micros;
use crate::error::Result;

/// Key holding the highest entry id handed out so far.
pub const ENTRY_ID_SEQ: &str = "entry_id_seq";

pub async fn get(conn: &mut SqliteConnection, key: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = ?1")
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn set(conn: &mut SqliteConnection, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Reserves `count` consecutive entry ids and returns the first.
///
/// This replaces FreshRSS's bulk-commit formula, `rowid + (SELECT MAX(id) - COUNT(*) FROM tmp)`.
/// That formula is only collision-free because FreshRSS gives each user their own database file, so
/// `MAX(id)` over the staging table is always far above anything previously committed. With one
/// shared database, two users refreshing at the same instant both stage ids near `now_micros` and
/// both target overlapping ranges — and because the commit is `INSERT OR IGNORE`, the loser's
/// articles are **silently dropped** rather than raising an error.
///
/// The fix keeps every property the renumbering bought (dense, chronological, microsecond-scale so
/// `date:` searches against `id` still work) while making cross-user collision impossible:
///
/// * `next = max(last_allocated + 1, now_micros)` — monotonic, and still tracks the wall clock so
///   an id remains a usable timestamp;
/// * ids are handed out in a contiguous run, so a batch of N articles gets N sequential ids;
/// * the read-modify-write happens inside the caller's transaction, and every caller reaches it
///   through [`crate::db::Db::with_writer`], which serializes on a process-wide mutex.
///
/// # Panics
///
/// Panics if `count` is zero, because there is no first id to return. Callers must skip the call
/// when a refresh produced nothing.
pub async fn reserve_entry_ids(conn: &mut SqliteConnection, count: usize) -> Result<i64> {
    assert!(count > 0, "reserving zero ids has no first id to return");
    let count = count as i64;

    let last: i64 = get(conn, ENTRY_ID_SEQ)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // The clock can go backwards (NTP correction, a VM restore), so never allocate below what has
    // already been handed out.
    let next = (last + 1).max(now_micros());
    set(conn, ENTRY_ID_SEQ, &(next + count - 1).to_string()).await?;
    Ok(next)
}

/// The highest entry id allocated so far, or 0 when none has been.
pub async fn entry_id_high_water(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(get(conn, ENTRY_ID_SEQ)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}
