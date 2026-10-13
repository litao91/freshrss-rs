//! The background refresh tick.
//!
//! FreshRSS leaves this to an external cron hitting `?c=feed&a=actualize`; here it is a task in the
//! server process, because a self-hosted reader that needs a crontab entry to stay current is a
//! reader that silently stops being current. The tick and the refresh button run the same code —
//! [`run`](crate::service::refresh::run) over a [`Scope`](crate::service::refresh::run::Scope) —
//! and differ only in scope: the tick takes every user's due feeds, the button takes one user's.
//!
//! Due-ness is the feed's own `ttl` against its `lastUpdate`, so a tick that finds nothing is a
//! single indexed query and no network traffic. The interval is how often that query runs, not how
//! often feeds are fetched.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::db::Db;
use crate::domain::ids::now_unix;
use crate::service::refresh::run::{self, Scope};
use crate::service::refresh::Settings;
use freshrss_feed::Fetcher;

/// A running tick loop.
///
/// Dropping the `Scheduler` does **not** stop the loop — the task is detached, and would keep
/// fetching feeds for a process that is shutting down. Call [`Scheduler::shutdown`].
#[derive(Debug)]
pub struct Scheduler {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Scheduler {
    /// Starts ticking every `interval`.
    ///
    /// An interval of zero disables the tick: the task starts, logs that it is disabled, and waits
    /// to be shut down. That is a supported deployment — one process behind a load balancer with
    /// the tick on a single instance — and expressing it as "no scheduler at all" would push an
    /// `Option` through every call site for one config value.
    pub fn spawn(
        db: Db,
        fetcher: Arc<dyn Fetcher>,
        settings: Settings,
        interval: Duration,
    ) -> Self {
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(tick(db, fetcher, settings, interval, stopped));
        Self { stop, task }
    }

    /// Stops the loop and waits for it to finish the tick it is in the middle of.
    pub async fn shutdown(self) {
        // `send` fails only when the receiver is gone, which means the loop already ended.
        let _ = self.stop.send(true);
        if let Err(err) = self.task.await {
            warn!(%err, "the refresh scheduler task ended abnormally");
        }
    }
}

async fn tick(
    db: Db,
    fetcher: Arc<dyn Fetcher>,
    settings: Settings,
    interval: Duration,
    mut stopped: watch::Receiver<bool>,
) {
    if interval.is_zero() {
        info!("background refresh disabled (scheduler_tick_secs = 0)");
        let _ = stopped.changed().await;
        return;
    }
    info!(secs = interval.as_secs(), "background refresh started");

    loop {
        tokio::select! {
            // Sleep first: whatever was due at startup is either already being refreshed by a user's
            // own request or can wait one interval, and ticking immediately on boot would make every
            // restart a thundering herd against every subscribed host.
            _ = tokio::time::sleep(interval) => {}
            _ = stopped.changed() => break,
        }

        let now = now_unix();
        let started = tokio::time::Instant::now();
        // The run is awaited rather than raced against the stop signal. Racing it would drop the
        // `JoinSet` it holds, and dropping a `JoinSet` aborts every task in it — mid-write, since a
        // refresh's database work is a sequence of statements rather than one transaction. Handing
        // the run the receiver instead means it stops *starting* feeds and lets the ones already in
        // flight commit, so a shutdown costs at most one fetch timeout.
        let result = run::run_until(
            &db,
            &fetcher,
            &settings,
            Scope::Due,
            now,
            Some(stopped.clone()),
        )
        .await;
        match result {
            Ok(report) => info!(
                feeds = report.feeds_done,
                new = report.new_entries,
                updated = report.updated_entries,
                muted = report.muted,
                failed = report.errors.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "refresh tick finished"
            ),
            Err(err) => warn!(%err, "the refresh tick could not run"),
        }
        if *stopped.borrow() {
            break;
        }
    }
    info!("background refresh stopped");
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::{FixtureFetcher, FixtureResponse};

    use crate::db::dao::{feed as feed_dao, user as user_dao};
    use crate::db::support::db;
    use crate::domain::feed::Feed;

    use super::*;

    const URL: &str = "https://tick.example/feed.xml";

    fn rss(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    const BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Tick</title>
<item><title>One</title><link>https://tick.example/1</link><guid>g1</guid><description>Body</description></item>
</channel></rss>"#;

    async fn seeded(db: &Db) -> Feed {
        db.with(|mut conn| async move {
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
            let mut feed = Feed::new(id_user, URL, "Tick");
            feed.category = user_dao::default_category_id(&mut conn, id_user).await?;
            feed.id = feed_dao::insert(&mut conn, &feed).await?;
            Ok(feed)
        })
        .await
        .expect("seeded")
    }

    async fn entry_count(db: &Db) -> i64 {
        db.with(|mut conn| async move {
            Ok(sqlx::query_scalar("SELECT COUNT(*) FROM entry")
                .fetch_one(&mut *conn)
                .await?)
        })
        .await
        .expect("counted")
    }

    /// Polls until `count` articles exist, or fails after roughly a second.
    async fn wait_for_entries(db: &Db, count: i64) {
        for _ in 0..200 {
            if entry_count(db).await >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "the tick never produced {count} articles (found {})",
            entry_count(db).await
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tick_refreshes_due_feeds_without_being_asked() {
        let db = db().await;
        seeded(&db).await;
        let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(URL, rss(BODY)));

        let scheduler = Scheduler::spawn(
            db.clone(),
            fetcher,
            Settings::default(),
            Duration::from_millis(10),
        );
        wait_for_entries(&db, 1).await;
        scheduler.shutdown().await;
        assert_eq!(entry_count(&db).await, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zero_interval_never_fetches() {
        let db = db().await;
        seeded(&db).await;
        let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(URL, rss(BODY)));

        let scheduler = Scheduler::spawn(db.clone(), fetcher, Settings::default(), Duration::ZERO);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(entry_count(&db).await, 0, "the tick is off");
        // Shutting down a disabled scheduler must still return, or the process would hang on exit.
        scheduler.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_returns_promptly_even_with_a_long_interval() {
        let db = db().await;
        let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new());
        let scheduler =
            Scheduler::spawn(db, fetcher, Settings::default(), Duration::from_secs(3600));
        // Fails by hanging rather than by asserting, which is the point: a scheduler that ignores
        // the stop signal would make the server unkillable by anything but SIGKILL.
        tokio::time::timeout(Duration::from_secs(5), scheduler.shutdown())
            .await
            .expect("shutdown did not return");
    }
}
