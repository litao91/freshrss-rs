//! Retention: deleting the articles a feed no longer keeps, and auto-reading the ones it dropped.
//!
//! These are the statements behind FreshRSS's `cleanOldEntries`, `markAsReadNotSeen` and
//! `markAsReadMaxUnread`. They are destructive and they run unattended, so every one of them is
//! written to fail safe: with no bound configured nothing is deleted, and the "keep at least what
//! the last refresh saw" clause is unconditional, so a feed that truncates its window to ten items
//! cannot empty a reader's archive.

use sqlx::{QueryBuilder, Sqlite, SqliteConnection};

use crate::config::ArchivingOptions;
use crate::error::Result;

/// Deletes the articles one feed no longer keeps, and returns how many went.
///
/// The shape is PHP's, clause for clause: exclusions first (favourites, unreads, labelled articles,
/// the `keep_min` most recently seen), then the unconditional "keep everything seen at the most
/// recent refresh", then an `OR`-group of the age and count bounds. That group starts at `1=0`, so
/// with neither bound configured the whole predicate is false and nothing is deleted — a feed whose
/// operator turned both bounds off keeps everything, which is the only safe reading of "no limit".
///
/// `now` is the refresh's instant, so the age bound is measured from the same second as everything
/// else this refresh writes.
pub async fn clean_old_entries(
    conn: &mut SqliteConnection,
    id_feed: i64,
    options: &ArchivingOptions,
    now: i64,
) -> Result<usize> {
    let mut builder = QueryBuilder::<Sqlite>::new("DELETE FROM entry WHERE id_feed = ");
    builder.push_bind(id_feed);

    if options.keep_favourites {
        builder.push(" AND is_favorite = 0");
    }
    if options.keep_unreads {
        builder.push(" AND is_read = 1");
    }
    if options.keep_labels {
        builder.push(" AND NOT EXISTS (SELECT 1 FROM entrytag WHERE id_entry = entry.id)");
    }
    if options.keep_min.is_some_and(|n| n > 0) {
        // The `keep_min`-th most recently seen article's `lastSeen`, as a bound: everything seen
        // strictly before it is old enough to consider. The nested select is PHP's workaround for
        // MySQL's refusal to read the table it is deleting from; SQLite accepts it too.
        builder.push(
            " AND lastSeen < (SELECT lastSeen FROM (SELECT e2.lastSeen FROM entry e2 \
             WHERE e2.id_feed = ",
        );
        builder
            .push_bind(id_feed)
            .push(" ORDER BY e2.lastSeen DESC LIMIT 1 OFFSET ");
        builder
            .push_bind(i64::from(options.keep_min.unwrap_or(0)))
            .push("))");
    }
    // Unconditional: whatever the most recent refresh still saw upstream stays, however old it is.
    // A feed that publishes once a year must not lose its only article to a three-month age bound.
    builder.push(
        " AND lastSeen < (SELECT maxlastseen FROM (SELECT MAX(e3.lastSeen) AS maxlastseen \
         FROM entry e3 WHERE e3.id_feed = ",
    );
    builder.push_bind(id_feed).push("))");

    builder.push(" AND (1=0");
    let mut bounded = false;
    if let Some(period) = options
        .keep_period
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match age_cutoff(period, now) {
            Some(cutoff) => {
                builder.push(" OR lastSeen < ");
                builder.push_bind(cutoff);
                bounded = true;
            }
            None => tracing::warn!(
                feed = id_feed,
                keep_period = period,
                "unparseable keep_period; the age bound is ignored rather than guessed"
            ),
        }
    }
    if options.keep_max.is_some_and(|n| n > 0) {
        builder.push(
            " OR lastSeen <= (SELECT lastSeen FROM (SELECT e4.lastSeen FROM entry e4 \
             WHERE e4.id_feed = ",
        );
        builder
            .push_bind(id_feed)
            .push(" ORDER BY e4.lastSeen DESC LIMIT 1 OFFSET ");
        builder
            .push_bind(i64::from(options.keep_max.unwrap_or(0)))
            .push("))");
        bounded = true;
    }
    builder.push(')');

    if !bounded {
        return Ok(0);
    }
    Ok(builder.build().execute(&mut *conn).await?.rows_affected() as usize)
}

/// `now` minus `period`, via the same ISO-8601 parser the `date:` search operator uses.
///
/// `None` when the period does not parse. The caller logs and drops the bound: an unparseable
/// `keep_period` is a config typo, and reading it as "keep nothing" would delete the archive.
fn age_cutoff(period: &str, now: i64) -> Option<i64> {
    freshrss_search::dateinterval::parse(period, now)
        .ok()
        .and_then(|range| range.min)
}

/// Marks read the unread articles a feed stopped publishing.
///
/// `lastSeen + 10 < now` rather than `lastSeen < now`: the ten seconds absorb a refresh whose
/// `lastSeen` write and this statement land on either side of a second boundary, so an article that
/// *was* upstream a moment ago is not read as gone.
pub async fn mark_read_not_seen(
    conn: &mut SqliteConnection,
    id_feed: i64,
    now: i64,
) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE entry SET is_read = 1
         WHERE id_feed = ?1 AND is_read = 0 AND lastSeen + 10 < ?2",
    )
    .bind(id_feed)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

/// Marks read all but the `keep` most recent unread articles of one feed.
///
/// When the feed has fewer unread articles than `keep`, the offset subquery returns no row, the
/// comparison is NULL, and nothing is marked — which is the whole point of the policy.
pub async fn mark_read_max_unread(
    conn: &mut SqliteConnection,
    id_feed: i64,
    keep: usize,
) -> Result<usize> {
    Ok(sqlx::query(
        "UPDATE entry SET is_read = 1
         WHERE id_feed = ?1 AND is_read = 0
           AND id <= (SELECT id FROM (SELECT e2.id FROM entry e2
                      WHERE e2.id_feed = ?2 AND e2.is_read = 0
                      ORDER BY e2.id DESC LIMIT 1 OFFSET ?3))",
    )
    .bind(id_feed)
    .bind(id_feed)
    .bind(keep as i64)
    .execute(&mut *conn)
    .await?
    .rows_affected() as usize)
}

#[cfg(test)]
mod tests {
    use crate::db::dao::{entry as entry_dao, feed as feed_dao, tag as tag_dao, user as user_dao};
    use crate::db::support::db;
    use crate::db::Db;
    use crate::domain::entry::Entry;
    use crate::domain::feed::Feed;
    use crate::domain::ids::EntryId;

    use super::*;

    const NOW: i64 = 1_791_000_000;
    const DAY: i64 = 86_400;

    /// One user, one feed, and `count` articles whose `lastSeen` steps back a day each, oldest
    /// first in id order. Article `n` is `n` days old.
    async fn seeded(count: usize) -> (Db, Feed) {
        let store = db().await;
        let (feed, id_user) = store
            .with(|mut conn| async move {
                let id_user = user_dao::insert(
                    &mut conn,
                    &user_dao::NewUser {
                        username: "alice",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )
                .await?;
                let mut feed = Feed::new(id_user, "https://e.example/f", "Example");
                feed.category = user_dao::default_category_id(&mut conn, id_user).await?;
                feed.id = feed_dao::insert(&mut conn, &feed).await?;
                Ok((feed, id_user))
            })
            .await
            .expect("seeded");

        for n in 0..count {
            let mut entry = Entry::new(id_user, feed.id, format!("g{n}"));
            // Ids ascend with recency, which is what the count bounds order by.
            entry.id = EntryId::from_micros((NOW - (count as i64 - n as i64) * DAY) * 1_000_000);
            entry.last_seen = NOW - (count as i64 - n as i64) * DAY;
            entry.is_read = true;
            store
                .with(move |mut conn| async move { entry_dao::stage(&mut conn, &entry).await })
                .await
                .expect("staged");
            store
                .with(move |mut conn| async move {
                    entry_dao::commit_staged(&mut conn, id_user).await
                })
                .await
                .expect("committed");
        }
        (store, feed)
    }

    async fn remaining(db: &Db, id_feed: i64) -> Vec<String> {
        let db = db.clone();
        db.with(move |mut conn| async move {
            Ok(
                sqlx::query_scalar("SELECT guid FROM entry WHERE id_feed = ?1 ORDER BY id")
                    .bind(id_feed)
                    .fetch_all(&mut *conn)
                    .await?,
            )
        })
        .await
        .expect("read")
    }

    fn options() -> ArchivingOptions {
        ArchivingOptions {
            keep_period: None,
            keep_max: None,
            keep_min: None,
            keep_favourites: true,
            keep_labels: true,
            keep_unreads: false,
        }
    }

    #[tokio::test]
    async fn with_no_bound_configured_nothing_is_deleted() {
        let (db, feed) = seeded(5).await;
        let deleted =
            db.with(|mut conn| async move {
                clean_old_entries(&mut conn, feed.id, &options(), NOW).await
            })
            .await
            .expect("cleaned");
        assert_eq!(
            deleted, 0,
            "no age bound and no count bound means keep everything"
        );
        assert_eq!(remaining(&db, feed.id).await.len(), 5);
    }

    #[tokio::test]
    async fn the_age_bound_deletes_only_what_is_old_enough() {
        let (db, feed) = seeded(5).await;
        let options = ArchivingOptions {
            keep_period: Some("P3D".into()),
            ..options()
        };
        let deleted =
            db.with(|mut conn| async move {
                clean_old_entries(&mut conn, feed.id, &options, NOW).await
            })
            .await
            .expect("cleaned");
        // The interval parser floors the far bound to the first instant *inside* the period, so an
        // article exactly three days old is old enough; one a second younger is not.
        assert_eq!(deleted, 3, "{:?}", remaining(&db, feed.id).await);
        assert_eq!(remaining(&db, feed.id).await, vec!["g3", "g4"]);
    }

    #[tokio::test]
    async fn the_count_bound_keeps_the_newest_n() {
        let (db, feed) = seeded(6).await;
        let options = ArchivingOptions {
            keep_max: Some(3),
            ..options()
        };
        let deleted =
            db.with(|mut conn| async move {
                clean_old_entries(&mut conn, feed.id, &options, NOW).await
            })
            .await
            .expect("cleaned");
        assert_eq!(deleted, 3);
        assert_eq!(remaining(&db, feed.id).await, vec!["g3", "g4", "g5"]);
    }

    #[tokio::test]
    async fn keep_min_protects_the_newest_n_even_past_the_age_bound() {
        let (db, feed) = seeded(6).await;
        let options = ArchivingOptions {
            keep_period: Some("P1D".into()),
            keep_min: Some(2),
            ..options()
        };
        db.with(
            |mut conn| async move { clean_old_entries(&mut conn, feed.id, &options, NOW).await },
        )
        .await
        .expect("cleaned");
        assert_eq!(
            remaining(&db, feed.id).await,
            vec!["g3", "g4", "g5"],
            "the age bound reaches five articles, but keep_min holds the newest ones back"
        );
    }

    #[tokio::test]
    async fn nothing_seen_at_the_most_recent_refresh_is_deleted() {
        let (db, feed) = seeded(5).await;
        // One refresh saw every article at this instant, so the unconditional clause protects all
        // of them however old the age bound says they are.
        let guids: Vec<String> = (0..5).map(|n| format!("g{n}")).collect();
        db.with(move |mut conn| async move {
            entry_dao::update_last_seen(&mut conn, feed.id, &guids, NOW).await
        })
        .await
        .expect("seen");
        let options = ArchivingOptions {
            keep_period: Some("P1D".into()),
            ..options()
        };
        let deleted =
            db.with(|mut conn| async move {
                clean_old_entries(&mut conn, feed.id, &options, NOW).await
            })
            .await
            .expect("cleaned");
        assert_eq!(deleted, 0, "the last refresh's articles are untouchable");
    }

    #[tokio::test]
    async fn favourites_and_labels_survive() {
        let (db, feed) = seeded(5).await;
        let options = ArchivingOptions {
            keep_period: Some("P1D".into()),
            ..options()
        };
        // Star the oldest, label the next.
        db.with(|mut conn| async move {
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM entry WHERE id_feed = ?1 ORDER BY id")
                    .bind(feed.id)
                    .fetch_all(&mut *conn)
                    .await?;
            entry_dao::mark_favorite(
                &mut conn,
                feed.id_user,
                &[EntryId::from_micros(ids[0])],
                true,
            )
            .await?;
            let tag = tag_dao::find_or_create(&mut conn, feed.id_user, "keepme").await?;
            tag_dao::tag_entries(&mut conn, tag, &[ids[1]]).await?;
            Ok(())
        })
        .await
        .expect("marked");

        db.with(
            |mut conn| async move { clean_old_entries(&mut conn, feed.id, &options, NOW).await },
        )
        .await
        .expect("cleaned");
        assert_eq!(
            remaining(&db, feed.id).await,
            vec!["g0", "g1", "g4"],
            "the starred and the labelled survive; g4 was seen at the last refresh"
        );
    }

    #[tokio::test]
    async fn unread_articles_survive_when_keep_unreads_is_on() {
        let (db, feed) = seeded(5).await;
        db.with(|mut conn| async move {
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM entry WHERE id_feed = ?1 ORDER BY id")
                    .bind(feed.id)
                    .fetch_all(&mut *conn)
                    .await?;
            entry_dao::mark_read(
                &mut conn,
                feed.id_user,
                &[EntryId::from_micros(ids[0])],
                false,
            )
            .await?;
            Ok(())
        })
        .await
        .expect("unread");

        let options = ArchivingOptions {
            keep_period: Some("P1D".into()),
            keep_unreads: true,
            ..options()
        };
        db.with(
            |mut conn| async move { clean_old_entries(&mut conn, feed.id, &options, NOW).await },
        )
        .await
        .expect("cleaned");
        assert_eq!(
            remaining(&db, feed.id).await,
            vec!["g0", "g4"],
            "g4 was seen at the most recent refresh, which protects it too"
        );
    }

    #[tokio::test]
    async fn an_unparseable_keep_period_deletes_nothing() {
        let (db, feed) = seeded(5).await;
        let options = ArchivingOptions {
            keep_period: Some("BANANA".into()),
            ..options()
        };
        let deleted =
            db.with(|mut conn| async move {
                clean_old_entries(&mut conn, feed.id, &options, NOW).await
            })
            .await
            .expect("cleaned");
        assert_eq!(deleted, 0, "a typo must not read as 'keep nothing'");
    }

    // --- the read-upon-gone and unread-cap statements ------------------------

    #[tokio::test]
    async fn articles_not_seen_for_ten_seconds_are_read_as_gone() {
        let (db, feed) = seeded(3).await;
        db.with(|mut conn| async move {
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM entry WHERE id_feed = ?1 ORDER BY id")
                    .bind(feed.id)
                    .fetch_all(&mut *conn)
                    .await?;
            // All three unread again; two of them were last seen long ago.
            entry_dao::mark_read(
                &mut conn,
                feed.id_user,
                &[EntryId::from_micros(ids[0])],
                false,
            )
            .await?;
            entry_dao::mark_read(
                &mut conn,
                feed.id_user,
                &[EntryId::from_micros(ids[1])],
                false,
            )
            .await?;
            sqlx::query("UPDATE entry SET lastSeen = ?2 WHERE id = ?1")
                .bind(ids[0])
                .bind(NOW - 100)
                .execute(&mut *conn)
                .await?;
            sqlx::query("UPDATE entry SET lastSeen = ?2 WHERE id = ?1")
                .bind(ids[1])
                .bind(NOW - 9)
                .execute(&mut *conn)
                .await?;
            Ok(())
        })
        .await
        .expect("prepared");

        let affected = db
            .with(|mut conn| async move { mark_read_not_seen(&mut conn, feed.id, NOW).await })
            .await
            .expect("marked");
        assert_eq!(affected, 1, "the ten-second grace spares the recent one");
    }

    #[tokio::test]
    async fn the_unread_cap_reads_all_but_the_newest_n() {
        let (db, feed) = seeded(5).await;
        db.with(|mut conn| async move {
            sqlx::query("UPDATE entry SET is_read = 0 WHERE id_feed = ?1")
                .bind(feed.id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        })
        .await
        .expect("all unread");

        let affected = db
            .with(|mut conn| async move { mark_read_max_unread(&mut conn, feed.id, 2).await })
            .await
            .expect("marked");
        assert_eq!(affected, 3);
        let unread: Vec<String> = db
            .with(move |mut conn| async move {
                Ok(sqlx::query_scalar(
                    "SELECT guid FROM entry WHERE id_feed = ?1 AND is_read = 0 ORDER BY id",
                )
                .bind(feed.id)
                .fetch_all(&mut *conn)
                .await?)
            })
            .await
            .expect("read");
        assert_eq!(unread, vec!["g3", "g4"], "the two newest stay unread");
    }

    #[tokio::test]
    async fn an_unread_count_under_the_cap_marks_nothing() {
        let (db, feed) = seeded(2).await;
        db.with(|mut conn| async move {
            sqlx::query("UPDATE entry SET is_read = 0 WHERE id_feed = ?1")
                .bind(feed.id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        })
        .await
        .expect("all unread");
        let affected = db
            .with(|mut conn| async move { mark_read_max_unread(&mut conn, feed.id, 5).await })
            .await
            .expect("marked");
        assert_eq!(
            affected, 0,
            "two unread against a cap of five is nothing to do"
        );
    }
}
