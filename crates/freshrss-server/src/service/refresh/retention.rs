//! When a refresh cleans up after itself.
//!
//! Three policies, all of them "the feed stopped publishing this, so act": delete what is no longer
//! kept, read what is gone, and cap the unread backlog. They run after a successful refresh and only
//! then — a feed that could not be fetched tells us nothing about what it still publishes, and
//! acting on silence would punish the reader for the publisher's outage.

use rand::RngExt as _;

use crate::config::ArchivingOptions;
use crate::db::dao::{
    category as cat_dao, entry as entry_dao, entry_retention, feed as feed_dao, user as user_dao,
};
use crate::db::Db;
use crate::domain::attributes::keys;
use crate::domain::feed::Feed;
use crate::domain::ids::{EntryId, MICROS_PER_SEC};
use crate::error::Result;

/// Whether this refresh runs the deletion pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// PHP's `rand(0, 30) === 1`: one refresh in thirty-one pays for the sweep. Deleting is the
    /// expensive part of a refresh — it walks every article of a feed — and nothing about it is
    /// urgent, so spreading it out is worth more than running it promptly.
    Roll,
    /// Run it whatever the roll says. What tests and an operator's manual trigger use.
    Force,
    /// Do not run it.
    Skip,
}

impl Cleanup {
    fn runs(self) -> bool {
        match self {
            Cleanup::Roll => rand::rng().random_range(0..=30) == 1,
            Cleanup::Force => true,
            Cleanup::Skip => false,
        }
    }
}

/// What the retention pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retained {
    pub deleted: usize,
    /// Articles read because the feed stopped publishing them.
    pub read_as_gone: usize,
    /// Articles read because the unread backlog exceeded its cap.
    pub read_over_cap: usize,
}

impl Retained {
    pub fn is_empty(&self) -> bool {
        self.deleted == 0 && self.read_as_gone == 0 && self.read_over_cap == 0
    }
}

/// Applies the retention policies to one feed after a successful refresh.
///
/// `upstream_is_empty` is whether the feed published no articles at all. That distinction is the
/// whole of `read_upon_gone`: a feed that still publishes but dropped one article dropped that
/// article, while a feed that publishes *nothing* has usually broken rather than retired everything,
/// and PHP reads the two cases differently — the empty one marks the whole feed read, on the theory
/// that there is nothing left to keep unread.
///
/// # Errors
///
/// Only storage failures. A policy that is switched off is not an error and does nothing.
pub async fn apply(
    db: &Db,
    feed: &Feed,
    now: i64,
    upstream_is_empty: bool,
    cleanup: Cleanup,
) -> Result<Retained> {
    let (id_user, category_id) = (feed.id_user, feed.category);
    let (config, category) = db
        .with(move |mut conn| async move {
            let config = user_dao::find_by_id(&mut conn, id_user)
                .await?
                .map(|user| user.config);
            let category = match category_id {
                Some(id) => cat_dao::find_by_id(&mut conn, id, id_user).await?,
                None => None,
            };
            Ok((config, category))
        })
        .await?;
    let config = config.unwrap_or_default();

    // Feed attribute, then folder attribute, then the user's config: the same chain as the filters,
    // and for the same reason — a feed's operator knows its publishing rhythm better than anyone.
    let archiving = feed
        .attributes
        .get_typed::<ArchivingOptions>(keys::ARCHIVING)
        .or_else(|| {
            category
                .as_ref()
                .and_then(|c| c.attributes.get_typed::<ArchivingOptions>(keys::ARCHIVING))
        })
        .unwrap_or_else(|| config.archiving.clone());

    let read_upon_gone = feed
        .attributes
        .get_bool(keys::READ_UPON_GONE)
        .unwrap_or(config.mark_when.gone);
    let keep_max_unread = feed
        .attributes
        .get_i64(keys::KEEP_MAX_N_UNREAD)
        .or(config.mark_when.max_n_unread.map(i64::from))
        .filter(|n| *n >= 0);

    let id = feed.id;
    let runs = cleanup.runs();
    db.with_writer(move |mut conn| async move {
        let mut retained = Retained::default();

        if runs {
            retained.deleted =
                entry_retention::clean_old_entries(&mut conn, id, &archiving, now).await?;
        }
        if read_upon_gone {
            retained.read_as_gone = if upstream_is_empty {
                entry_dao::mark_read_feed(
                    &mut conn,
                    id,
                    EntryId::from_micros(now.saturating_mul(MICROS_PER_SEC)),
                    now,
                )
                .await?
            } else {
                entry_retention::mark_read_not_seen(&mut conn, id, now).await?
            };
        }
        if let Some(keep) = keep_max_unread {
            retained.read_over_cap =
                entry_retention::mark_read_max_unread(&mut conn, id, keep as usize).await?;
        }

        if !retained.is_empty() {
            feed_dao::update_cached_values(&mut conn, &[id]).await?;
        }
        Ok(retained)
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::config::MarkWhen;
    use crate::db::dao::{entry as entry_dao, feed as feed_dao, user as user_dao};
    use crate::db::support::db;
    use crate::domain::attributes::Attributes;
    use crate::domain::entry::Entry;
    use crate::domain::feed::Feed;
    use crate::domain::ids::EntryId;

    use super::*;

    const NOW: i64 = 1_791_000_000;
    const DAY: i64 = 86_400;

    /// One user, one feed, `count` read articles a day apart, oldest first.
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

    /// The guids still stored, optionally only the unread ones.
    async fn guids(db: &Db, id_feed: i64, unread_only: bool) -> Vec<String> {
        let db = db.clone();
        db.with(move |mut conn| async move {
            let rows: Vec<(String, bool)> =
                sqlx::query_as("SELECT guid, is_read FROM entry WHERE id_feed = ?1 ORDER BY id")
                    .bind(id_feed)
                    .fetch_all(&mut *conn)
                    .await?;
            Ok(rows
                .into_iter()
                .filter(|(_, is_read)| !unread_only || !*is_read)
                .map(|(guid, _)| guid)
                .collect())
        })
        .await
        .expect("read")
    }

    /// Stores an attribute change and hands back the feed as the store now has it, which is what a
    /// real refresh reads: passing the pre-change value in would test nothing.
    async fn set_feed_attribute(
        db: &Db,
        feed: &Feed,
        mutate: impl FnOnce(&mut Attributes) + Send,
    ) -> Feed {
        let (id, id_user) = (feed.id, feed.id_user);
        let db = db.clone();
        db.with(move |mut conn| async move {
            let mut stored = feed_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .expect("feed");
            mutate(&mut stored.attributes);
            feed_dao::update_attributes(&mut conn, id, id_user, &stored.attributes).await?;
            feed_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .ok_or_else(|| crate::error::Error::Internal("the feed vanished".to_owned()))
        })
        .await
        .expect("attribute stored")
    }

    #[tokio::test]
    async fn a_feed_with_no_policies_on_changes_nothing() {
        let (db, feed) = seeded(6).await;
        let retained = apply(&db, &feed, NOW, false, Cleanup::Force)
            .await
            .expect("applied");
        assert_eq!(
            retained.deleted, 0,
            "the default archiving keeps 50 and 200"
        );
        assert_eq!(retained.read_as_gone, 0);
        assert_eq!(retained.read_over_cap, 0);
        assert_eq!(guids(&db, feed.id, false).await.len(), 6);
    }

    #[tokio::test]
    async fn the_users_archiving_applies_when_the_feed_sets_none() {
        let (db, feed) = seeded(6).await;
        let id_user = feed.id_user;
        db.with(move |mut conn| async move {
            let mut user = user_dao::find_by_id(&mut conn, id_user)
                .await?
                .expect("user");
            user.config.archiving = ArchivingOptions {
                keep_period: Some("P2D".into()),
                keep_max: Some(3),
                keep_min: Some(1),
                keep_favourites: true,
                keep_labels: true,
                keep_unreads: false,
            };
            user_dao::update_config(&mut conn, id_user, &user.config).await
        })
        .await
        .expect("config stored");

        let retained = apply(&db, &feed, NOW, false, Cleanup::Force)
            .await
            .expect("applied");
        assert!(retained.deleted > 0, "the user's bounds now bite");
        assert!(guids(&db, feed.id, false).await.len() < 6);
    }

    #[tokio::test]
    async fn a_feed_attribute_overrides_the_user() {
        let (db, feed) = seeded(6).await;
        let id_user = feed.id_user;
        db.with(move |mut conn| async move {
            let mut user = user_dao::find_by_id(&mut conn, id_user)
                .await?
                .expect("user");
            user.config.archiving = ArchivingOptions {
                keep_period: Some("P2D".into()),
                keep_max: None,
                keep_min: None,
                keep_favourites: true,
                keep_labels: true,
                keep_unreads: false,
            };
            user_dao::update_config(&mut conn, id_user, &user.config).await
        })
        .await
        .expect("config stored");
        // The feed keeps everything, overriding the user's two-day bound.
        let feed = set_feed_attribute(&db, &feed, |attributes| {
            let _ = attributes.set_typed(
                keys::ARCHIVING,
                &ArchivingOptions {
                    keep_period: None,
                    keep_max: None,
                    keep_min: None,
                    keep_favourites: true,
                    keep_labels: true,
                    keep_unreads: false,
                },
            );
        })
        .await;

        let retained = apply(&db, &feed, NOW, false, Cleanup::Force)
            .await
            .expect("applied");
        assert_eq!(retained.deleted, 0, "the feed's own archiving wins");
    }

    #[tokio::test]
    async fn read_upon_gone_reads_what_the_feed_dropped() {
        let (db, feed) = seeded(3).await;
        // g0 was last seen two refreshes ago; the others were seen now.
        db.with(|mut conn| async move {
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM entry WHERE id_feed = ?1 ORDER BY id")
                    .bind(feed.id)
                    .fetch_all(&mut *conn)
                    .await?;
            sqlx::query("UPDATE entry SET is_read = 0, lastSeen = ?2 WHERE id = ?1")
                .bind(ids[0])
                .bind(NOW - 100)
                .execute(&mut *conn)
                .await?;
            // g1 was seen at this refresh, so the ten-second grace must spare it.
            sqlx::query("UPDATE entry SET is_read = 0, lastSeen = ?2 WHERE id = ?1")
                .bind(ids[1])
                .bind(NOW)
                .execute(&mut *conn)
                .await?;
            Ok(())
        })
        .await
        .expect("prepared");
        let feed = set_feed_attribute(&db, &feed, |attributes| {
            attributes.set_bool(keys::READ_UPON_GONE, Some(true));
        })
        .await;

        let retained = apply(&db, &feed, NOW, false, Cleanup::Skip)
            .await
            .expect("applied");
        assert_eq!(retained.read_as_gone, 1, "the ten-second grace spares g1");
        assert_eq!(guids(&db, feed.id, true).await, vec!["g1"]);
    }

    #[tokio::test]
    async fn an_empty_upstream_reads_the_whole_feed() {
        let (db, feed) = seeded(3).await;
        // "Everything up to now" is an id bound at a second's granularity, and the allocator stamps
        // ids with the microsecond, so an article ingested earlier in this same second sits *above*
        // `now * 1_000_000`. One second ahead is the honest bound for "everything so far".
        let now = crate::domain::ids::now_unix() + 1;
        db.with(|mut conn| async move {
            sqlx::query("UPDATE entry SET is_read = 0 WHERE id_feed = ?1")
                .bind(feed.id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        })
        .await
        .expect("all unread");
        let feed = set_feed_attribute(&db, &feed, |attributes| {
            attributes.set_bool(keys::READ_UPON_GONE, Some(true));
        })
        .await;

        let retained = apply(&db, &feed, now, true, Cleanup::Skip)
            .await
            .expect("applied");
        assert_eq!(retained.read_as_gone, 3, "an empty feed reads everything");
        assert!(guids(&db, feed.id, true).await.is_empty());
    }

    #[tokio::test]
    async fn the_unread_cap_reads_the_oldest_over_the_cap() {
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
        let feed = set_feed_attribute(&db, &feed, |attributes| {
            attributes.set(keys::KEEP_MAX_N_UNREAD, 2);
        })
        .await;

        let retained = apply(&db, &feed, NOW, false, Cleanup::Skip)
            .await
            .expect("applied");
        assert_eq!(retained.read_over_cap, 3);
        assert_eq!(guids(&db, feed.id, true).await, vec!["g3", "g4"]);
    }

    #[tokio::test]
    async fn a_negative_cap_is_ignored_rather_than_read_as_unlimited() {
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
        let feed = set_feed_attribute(&db, &feed, |attributes| {
            attributes.set(keys::KEEP_MAX_N_UNREAD, -1);
        })
        .await;

        let retained = apply(&db, &feed, NOW, false, Cleanup::Skip)
            .await
            .expect("applied");
        assert_eq!(retained.read_over_cap, 0);
    }

    #[tokio::test]
    async fn a_failed_refresh_is_not_a_reason_to_clean() {
        // `apply` is only called after a successful refresh; this test pins the contract from the
        // other side, by asserting that skipping the cleanup leaves everything in place.
        let (db, feed) = seeded(6).await;
        let id_user = feed.id_user;
        db.with(move |mut conn| async move {
            let mut user = user_dao::find_by_id(&mut conn, id_user)
                .await?
                .expect("user");
            user.config.archiving.keep_period = Some("P1D".into());
            user.config.mark_when = MarkWhen {
                gone: true,
                ..MarkWhen::default()
            };
            user_dao::update_config(&mut conn, id_user, &user.config).await
        })
        .await
        .expect("config stored");

        let retained = apply(&db, &feed, NOW, false, Cleanup::Skip)
            .await
            .expect("applied");
        assert!(retained.is_empty());
        assert_eq!(guids(&db, feed.id, false).await.len(), 6);
    }
}
