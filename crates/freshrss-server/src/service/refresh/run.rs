//! Running a refresh over many feeds, and reporting what happened.
//!
//! [`refresh_feed`](super::refresh_feed) is one feed and one database write. This is the run around
//! it: which feeds are due, how many are in flight at once, and what the caller is told afterwards.
//! The two are kept apart because a run's shape depends on who asked — the scheduler wants every
//! user's due feeds, the refresh button wants one user's, and `POST /feeds/{id}/refresh` wants a
//! specific feed whether or not it is due — while none of that changes what refreshing a feed means.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use super::{refresh_feed, Outcome, Settings, Status};
use crate::db::dao::feed as feed_dao;
use crate::db::Db;
use crate::domain::feed::Feed;
use crate::error::{Error, Result};
use freshrss_feed::Fetcher;

/// Which feeds a run covers.
#[derive(Debug, Clone)]
pub enum Scope {
    /// Every user's due feeds, oldest-refreshed first. The scheduler's tick.
    Due,
    /// One user's due feeds. The refresh button, which must not touch anyone else's subscriptions.
    User(i64),
    /// One named feed, due or not.
    ///
    /// Boxed because a `Feed` carries its attributes map and is an order of magnitude larger than
    /// the other two variants, which would make every `Scope` that size.
    Feed(Box<Feed>),
}

/// One feed's failure, as the job report and the problems dialog see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedFailure {
    pub feed_id: i64,
    pub name: String,
    pub message: String,
    /// The HTTP status behind the failure, when there was one.
    pub http_status: Option<u16>,
}

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Feeds that were fetched, whether or not anything changed.
    pub feeds_done: usize,
    pub new_entries: usize,
    pub updated_entries: usize,
    /// Feeds that answered 410 and were muted by this run.
    pub muted: usize,
    /// Feeds that could not be refreshed, in the order they finished.
    ///
    /// Kept rather than summed: one broken subscription out of a hundred must not turn the whole run
    /// into an error, and the user needs to know *which* one broke.
    pub errors: Vec<FeedFailure>,
}

impl Report {
    fn record(&mut self, feed: &Feed, outcome: &Outcome) {
        match &outcome.status {
            Status::Failed {
                message,
                http_status,
            } => self.errors.push(FeedFailure {
                feed_id: feed.id,
                name: feed.display_name().to_owned(),
                message: message.clone(),
                http_status: *http_status,
            }),
            Status::Muted => {
                self.muted += 1;
                self.feeds_done += 1;
            }
            _ => self.feeds_done += 1,
        }
        self.new_entries += outcome.new_entries;
        self.updated_entries += outcome.updated_entries;
    }
}

/// Refreshes every feed in `scope`, at most [`Settings::concurrency`] at a time.
///
/// # Errors
///
/// Only when the run could not be carried out at all — the due-feed selection failed, or a task
/// panicked. Individual feeds failing is not an error; it comes back in [`Report::errors`].
pub async fn run(
    db: &Db,
    fetcher: &Arc<dyn Fetcher>,
    settings: &Settings,
    scope: Scope,
    now: i64,
) -> Result<Report> {
    run_until(db, fetcher, settings, scope, now, None).await
}

/// Like [`run`], but stops handing out feeds once `cancel` is set.
///
/// Feeds already in flight are left to finish rather than aborted. A refresh's write is a sequence
/// of statements under the writer lock, not one transaction, so cancelling one mid-way would leave
/// staged rows or an advanced `lastSeen` without the `lastUpdate` to match — all of it recoverable
/// by the next refresh, but pointless when waiting costs a few hundred milliseconds. What
/// cancelling does buy is not *starting* the remaining ninety feeds during a shutdown.
pub async fn run_until(
    db: &Db,
    fetcher: &Arc<dyn Fetcher>,
    settings: &Settings,
    scope: Scope,
    now: i64,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<Report> {
    let feeds = match scope {
        Scope::Due => {
            db.with(|mut conn| async move {
                feed_dao::list_due(&mut conn, now, settings.max_feeds).await
            })
            .await?
        }
        Scope::User(id_user) => {
            db.with(move |mut conn| async move {
                feed_dao::list_due_for_user(&mut conn, id_user, now, settings.max_feeds).await
            })
            .await?
        }
        Scope::Feed(feed) => vec![*feed],
    };

    let mut report = Report::default();
    if feeds.is_empty() {
        return Ok(report);
    }

    // A semaphore rather than an unbounded `JoinSet`: a user with a thousand subscriptions must not
    // open a thousand sockets at once, and the limit is the operator's, not the feed's.
    let permits = Arc::new(Semaphore::new(settings.concurrency.max(1)));
    let mut tasks: JoinSet<(Feed, Result<Outcome>)> = JoinSet::new();
    for feed in feeds {
        if cancel.as_ref().is_some_and(|rx| *rx.borrow()) {
            tracing::info!(
                started = tasks.len(),
                "cancelling a refresh run; feeds already in flight will finish"
            );
            break;
        }
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Internal("the refresh semaphore closed mid-run".to_owned()))?;
        let (db, fetcher, settings) = (db.clone(), Arc::clone(fetcher), settings.clone());
        tasks.spawn(async move {
            let outcome = refresh_feed(&db, &*fetcher, &feed, &settings, now).await;
            // Held until the write is committed, so the count in flight is real.
            drop(permit);
            (feed, outcome)
        });
    }

    while let Some(joined) = tasks.join_next().await {
        let (feed, outcome) =
            joined.map_err(|err| Error::Internal(format!("a refresh task failed: {err}")))?;
        match outcome {
            Ok(outcome) => report.record(&feed, &outcome),
            Err(err) => report.errors.push(FeedFailure {
                feed_id: feed.id,
                name: feed.display_name().to_owned(),
                message: err.to_string(),
                http_status: None,
            }),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::reqwest::FixtureError;
    use freshrss_feed::http::{FixtureFetcher, FixtureResponse};

    use crate::db::dao::{entry as entry_dao, user as user_dao};
    use crate::db::support::db;
    use crate::domain::ttl::Ttl;

    use super::*;

    const NOW: i64 = 1_791_000_000;

    fn rss(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    fn feed_body(name: &str, count: usize) -> String {
        let items: String = (0..count)
            .map(|n| {
                format!(
                    "<item><title>{name} {n}</title><link>https://{name}.example/{n}</link>\
                     <guid>{name}-{n}</guid><description>Body</description></item>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>{name}</title>{items}</channel></rss>"
        )
    }

    /// `count` users, each with one feed that is due now.
    async fn seed(db: &Db, count: usize) -> Vec<Feed> {
        db.with(move |mut conn| async move {
            let mut feeds = Vec::new();
            for n in 0..count {
                let username = format!("user{n}");
                let id_user = user_dao::insert(
                    &mut conn,
                    &user_dao::NewUser {
                        username: &username,
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )
                .await?;
                let mut feed = Feed::new(
                    id_user,
                    format!("https://u{n}.example/feed.xml"),
                    format!("Feed {n}"),
                );
                feed.category = user_dao::default_category_id(&mut conn, id_user).await?;
                feed.id = feed_dao::insert(&mut conn, &feed).await?;
                feeds.push(feed);
            }
            Ok(feeds)
        })
        .await
        .expect("seeded")
    }

    fn settings() -> Settings {
        Settings {
            concurrency: 4,
            max_feeds: 50,
            ..Settings::default()
        }
    }

    /// A fixture behind the trait object [`run`] takes, so the tests stay readable.
    fn fixture(fetcher: FixtureFetcher) -> Arc<dyn Fetcher> {
        Arc::new(fetcher)
    }

    #[tokio::test]
    async fn a_run_refreshes_every_due_feed() {
        let db = db().await;
        let feeds = seed(&db, 3).await;
        let mut fetcher = FixtureFetcher::new();
        for (n, _) in feeds.iter().enumerate() {
            fetcher = fetcher.with(
                format!("https://u{n}.example/feed.xml"),
                rss(&feed_body(&format!("u{n}"), 5)),
            );
        }

        let report = run(&db, &fixture(fetcher), &settings(), Scope::Due, NOW)
            .await
            .expect("ran");
        assert_eq!(report.feeds_done, 3);
        assert_eq!(report.new_entries, 15);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let total: i64 = db
            .with(|mut conn| async move {
                Ok(sqlx::query_scalar("SELECT COUNT(*) FROM entry")
                    .fetch_one(&mut *conn)
                    .await?)
            })
            .await
            .expect("counted");
        assert_eq!(total, 15, "every user's articles landed, and only theirs");
    }

    #[tokio::test]
    async fn a_second_run_over_quiet_feeds_adds_nothing() {
        let db = db().await;
        let feeds = seed(&db, 2).await;
        let mut fetcher = FixtureFetcher::new();
        for (n, _) in feeds.iter().enumerate() {
            fetcher = fetcher.with(
                format!("https://u{n}.example/feed.xml"),
                rss(&feed_body(&format!("u{n}"), 4)),
            );
        }
        let fetcher = fixture(fetcher);
        run(&db, &fetcher, &settings(), Scope::Due, NOW)
            .await
            .expect("first");

        // A day later: the bodies are identical, so the fingerprint short-circuits every feed.
        let again = run(&db, &fetcher, &settings(), Scope::Due, NOW + 86_400)
            .await
            .expect("second");
        assert_eq!(again.feeds_done, 2);
        assert_eq!(again.new_entries, 0);
        assert_eq!(again.updated_entries, 0);
        assert!(again.errors.is_empty(), "{:?}", again.errors);
    }

    #[tokio::test]
    async fn one_broken_feed_does_not_abort_the_run() {
        let db = db().await;
        let feeds = seed(&db, 3).await;
        let fetcher = fixture(
            FixtureFetcher::new()
                .with("https://u0.example/feed.xml", rss(&feed_body("u0", 3)))
                // u1 answers 503; u2 is not registered at all, so the fixture fetcher 404s.
                .with(
                    "https://u1.example/feed.xml",
                    FixtureResponse::Error(FixtureError::Http(503)),
                )
                .with("https://u2.example/feed.xml", rss(&feed_body("u2", 2))),
        );

        let report = run(&db, &fetcher, &settings(), Scope::Due, NOW)
            .await
            .expect("ran");
        assert_eq!(report.feeds_done, 2, "the two that worked");
        assert_eq!(report.new_entries, 5);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(report.errors[0].feed_id, feeds[1].id);
        assert_eq!(report.errors[0].name, "Feed 1");
        assert!(
            report.errors[0].message.contains("503"),
            "{}",
            report.errors[0].message
        );
    }

    #[tokio::test]
    async fn a_gone_feed_is_counted_as_muted_rather_than_as_an_error() {
        let db = db().await;
        seed(&db, 1).await;
        let fetcher = fixture(FixtureFetcher::new().with(
            "https://u0.example/feed.xml",
            FixtureResponse::Error(FixtureError::Gone),
        ));
        let report = run(&db, &fetcher, &settings(), Scope::Due, NOW)
            .await
            .expect("ran");
        assert_eq!(report.muted, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.feeds_done, 1,
            "muting is a completed refresh, not a failure"
        );
    }

    #[tokio::test]
    async fn a_user_scope_refreshes_only_that_user() {
        let db = db().await;
        let feeds = seed(&db, 2).await;
        let fetcher = fixture(
            FixtureFetcher::new()
                .with("https://u0.example/feed.xml", rss(&feed_body("u0", 3)))
                .with("https://u1.example/feed.xml", rss(&feed_body("u1", 3))),
        );
        let report = run(
            &db,
            &fetcher,
            &settings(),
            Scope::User(feeds[0].id_user),
            NOW,
        )
        .await
        .expect("ran");
        assert_eq!(report.feeds_done, 1);
        assert_eq!(report.new_entries, 3);

        let other: i64 = db
            .with(move |mut conn| async move {
                Ok(
                    sqlx::query_scalar("SELECT COUNT(*) FROM entry WHERE id_user = ?1")
                        .bind(feeds[1].id_user)
                        .fetch_one(&mut *conn)
                        .await?,
                )
            })
            .await
            .expect("counted");
        assert_eq!(other, 0, "the other user's feed was left alone");
    }

    #[tokio::test]
    async fn a_muted_feed_is_not_due() {
        let db = db().await;
        let feeds = seed(&db, 1).await;
        let (feed_id, id_user) = (feeds[0].id, feeds[0].id_user);
        db.with(move |mut conn| async move {
            feed_dao::update_ttl(&mut conn, feed_id, id_user, Ttl::new(900, true))
                .await
                .map(|_| ())
        })
        .await
        .expect("muted");

        // A fetcher that would succeed, so a selection bug shows up as articles appearing.
        let fetcher = fixture(
            FixtureFetcher::new().with("https://u0.example/feed.xml", rss(&feed_body("u0", 3))),
        );
        let report = run(&db, &fetcher, &settings(), Scope::Due, NOW)
            .await
            .expect("ran");
        assert_eq!(
            report.feeds_done, 0,
            "a muted feed is not refreshed by a tick"
        );
        assert_eq!(report.new_entries, 0);

        // …but it can still be refreshed by name, which is how a user unmutes-and-retries.
        let muted =
            db.with(move |mut conn| async move {
                feed_dao::find_by_id(&mut conn, feed_id, id_user).await
            })
            .await
            .expect("read")
            .expect("still there");
        let report = run(
            &db,
            &fetcher,
            &settings(),
            Scope::Feed(Box::new(muted)),
            NOW,
        )
        .await
        .expect("ran");
        assert_eq!(report.new_entries, 3);
    }

    #[tokio::test]
    async fn the_feed_limit_caps_a_run() {
        let db = db().await;
        seed(&db, 5).await;
        let mut fetcher = FixtureFetcher::new();
        for n in 0..5 {
            fetcher = fetcher.with(
                format!("https://u{n}.example/feed.xml"),
                rss(&feed_body(&format!("u{n}"), 1)),
            );
        }
        let settings = Settings {
            max_feeds: 2,
            ..settings()
        };
        let report = run(&db, &fixture(fetcher), &settings, Scope::Due, NOW)
            .await
            .expect("ran");
        assert_eq!(
            report.feeds_done, 2,
            "one tick is bounded; the rest wait for the next"
        );
    }

    #[tokio::test]
    async fn an_empty_run_is_not_an_error() {
        let db = db().await;
        let report = run(
            &db,
            &fixture(FixtureFetcher::new()),
            &settings(),
            Scope::Due,
            NOW,
        )
        .await
        .expect("ran");
        assert_eq!(report, Report::default());
    }

    #[tokio::test]
    async fn a_cancelled_run_starts_nothing_new() {
        let db = db().await;
        seed(&db, 3).await;
        let fetcher = fixture(
            FixtureFetcher::new()
                .with("https://u0.example/feed.xml", rss(&feed_body("u0", 2)))
                .with("https://u1.example/feed.xml", rss(&feed_body("u1", 2)))
                .with("https://u2.example/feed.xml", rss(&feed_body("u2", 2))),
        );
        let (stop, rx) = watch::channel(false);
        stop.send_replace(true);

        let report = run_until(&db, &fetcher, &settings(), Scope::Due, NOW, Some(rx))
            .await
            .expect("ran");
        assert_eq!(report.feeds_done, 0, "the flag was already set");
        assert_eq!(report.new_entries, 0);
    }

    #[tokio::test]
    async fn a_single_feed_scope_reports_just_that_feed() {
        let db = db().await;
        let feeds = seed(&db, 1).await;
        let (feed_id, id_user) = (feeds[0].id, feeds[0].id_user);
        let fetcher = fixture(
            FixtureFetcher::new().with("https://u0.example/feed.xml", rss(&feed_body("u0", 6))),
        );
        let report = run(
            &db,
            &fetcher,
            &settings(),
            Scope::Feed(Box::new(feeds[0].clone())),
            NOW,
        )
        .await
        .expect("ran");
        assert_eq!(report.feeds_done, 1);
        assert_eq!(report.new_entries, 6);

        // And a second one, immediately, finds nothing new but still advances lastSeen.
        let after =
            db.with(move |mut conn| async move {
                feed_dao::find_by_id(&mut conn, feed_id, id_user).await
            })
            .await
            .expect("read")
            .expect("still there");
        let again = run(
            &db,
            &fetcher,
            &settings(),
            Scope::Feed(Box::new(after)),
            NOW + 60,
        )
        .await
        .expect("ran");
        assert_eq!(again.new_entries, 0);
        let seen: i64 = db
            .with(move |mut conn| async move {
                let entry = entry_dao::find_by_guid(&mut conn, feed_id, "u0-0")
                    .await?
                    .expect("an article");
                Ok(entry.last_seen)
            })
            .await
            .expect("read");
        assert_eq!(seen, NOW + 60);
    }
}
