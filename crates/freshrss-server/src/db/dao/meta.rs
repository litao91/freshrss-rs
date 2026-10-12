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
/// * the read-modify-write is a **single statement**, so SQLite's write lock makes it atomic.
///
/// That last point is the one that matters. Callers do reach this through
/// [`crate::db::Db::with_writer`], but that mutex is per *process*: two server instances sharing one
/// database file have nothing between them, and a SELECT-then-UPDATE here would hand both the same
/// range — after which `INSERT OR IGNORE` drops one of them's articles without a word. Doing it in
/// one statement makes the allocation correct regardless of who is calling.
///
/// # Panics
///
/// Panics if `count` is zero, because there is no first id to return. Callers must skip the call
/// when a refresh produced nothing.
pub async fn reserve_entry_ids(conn: &mut SqliteConnection, count: usize) -> Result<i64> {
    assert!(count > 0, "reserving zero ids has no first id to return");
    let count = count as i64;
    // The clock can go backwards (an NTP correction, a VM restore), so `max()` against the stored
    // high-water mark is what keeps ids monotonic; the floor only ever raises them.
    let floor = now_micros();

    // `max()` here is SQLite's two-argument scalar function, not the aggregate, and `value` in the
    // RETURNING clause is the row's *new* value in both branches.
    Ok(sqlx::query_scalar(
        "INSERT INTO meta (key, value) VALUES (?1, CAST(?2 + ?3 - 1 AS TEXT))
         ON CONFLICT (key) DO UPDATE SET
             value = CAST(max(CAST(meta.value AS INTEGER) + 1, ?2) + ?3 - 1 AS TEXT)
         RETURNING CAST(value AS INTEGER) - ?3 + 1",
    )
    .bind(ENTRY_ID_SEQ)
    .bind(floor)
    .bind(count)
    .fetch_one(&mut *conn)
    .await?)
}

/// The highest entry id allocated so far, or 0 when none has been.
pub async fn entry_id_high_water(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(get(conn, ENTRY_ID_SEQ)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[tokio::test]
    async fn reservations_are_dense_and_monotonic() {
        let db = crate::db::support::db().await;
        let ranges: Vec<(i64, usize)> = vec![(0, 1), (0, 5), (0, 200)];
        let mut previous_end = 0;
        for (_, count) in ranges {
            let first = db
                .with(move |mut conn| async move { reserve_entry_ids(&mut conn, count).await })
                .await
                .expect("reserved");
            assert!(first > previous_end, "{first} after {previous_end}");
            previous_end = first + count as i64 - 1;
        }
        let high = db
            .with(|mut conn| async move { entry_id_high_water(&mut conn).await })
            .await
            .expect("read");
        assert_eq!(
            high, previous_end,
            "the high-water mark is the last id handed out"
        );
    }

    // Two OS threads, and deliberately *not* `Db::with_writer`: the allocation has to be correct
    // because of its own statement, not because a per-process mutex happened to serialize it. Two
    // server instances on one database file have no such mutex between them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_never_hand_out_the_same_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, db) = crate::db::support::file_db(dir.path()).await;

        const TASKS: usize = 8;
        const EACH: usize = 50;
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                let mut ranges = Vec::new();
                for _ in 0..EACH {
                    let first = db
                        .with(|mut conn| async move { reserve_entry_ids(&mut conn, 10).await })
                        .await
                        .expect("reserved");
                    ranges.push((first, first + 9));
                }
                ranges
            }));
        }

        let mut taken: Vec<(i64, i64)> = Vec::new();
        for handle in handles {
            taken.extend(handle.await.expect("joined"));
        }
        assert_eq!(taken.len(), TASKS * EACH);
        taken.sort_unstable();
        for window in taken.windows(2) {
            let (a, b) = (window[0], window[1]);
            assert!(
                a.1 < b.0,
                "two reservations overlap: {a:?} and {b:?} — articles would be silently dropped"
            );
        }

        let high = db
            .with(|mut conn| async move { entry_id_high_water(&mut conn).await })
            .await
            .expect("read");
        assert_eq!(high, taken.last().expect("non-empty").1);
    }

    #[tokio::test]
    async fn a_stored_high_water_mark_beats_a_clock_that_went_backwards() {
        let db = crate::db::support::db().await;
        // A restore from a snapshot, or an NTP correction, can put the wall clock behind ids that
        // were already handed out. Allocating from the clock then would reuse them.
        let far_future = now_micros() + 10_000_000_000;
        db.with(move |mut conn| async move {
            set(&mut conn, ENTRY_ID_SEQ, &far_future.to_string()).await
        })
        .await
        .expect("set");

        let first = db
            .with(|mut conn| async move { reserve_entry_ids(&mut conn, 3).await })
            .await
            .expect("reserved");
        assert_eq!(first, far_future + 1, "monotonic, not wall-clock");
    }

    #[tokio::test]
    #[should_panic(expected = "reserving zero ids")]
    async fn reserving_nothing_is_a_programming_error() {
        let db = crate::db::support::db().await;
        let _ = db
            .with(|mut conn| async move { reserve_entry_ids(&mut conn, 0).await })
            .await;
    }

    #[tokio::test]
    async fn get_and_set_round_trip_and_a_missing_key_is_absent() {
        let db = Db::in_memory().await.expect("db");
        let value = db
            .with(|mut conn| async move { get(&mut conn, "nope").await })
            .await
            .expect("read");
        assert_eq!(value, None);
        db.with(|mut conn| async move { set(&mut conn, "k", "v").await })
            .await
            .expect("set");
        let value = db
            .with(|mut conn| async move { get(&mut conn, "k").await })
            .await
            .expect("read");
        assert_eq!(value.as_deref(), Some("v"));
        // An upsert, not a duplicate.
        db.with(|mut conn| async move { set(&mut conn, "k", "w").await })
            .await
            .expect("set");
        let value = db
            .with(|mut conn| async move { get(&mut conn, "k").await })
            .await
            .expect("read");
        assert_eq!(value.as_deref(), Some("w"));
    }
}
