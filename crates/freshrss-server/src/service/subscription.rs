//! Subscribing: recording a feed, and the first fetch that decides whether it is one.
//!
//! The order matters and is PHP's: `feedController::addFeed` loads the feed *before* inserting it,
//! and throws `FreshRSS_FeedNotAdded_Exception` if the load fails. So an address that turns out not
//! to be a feed leaves no row behind. That is worth the extra step — the alternative is a sidebar
//! that slowly fills with subscriptions the user typed once, wrongly, and then has to find and
//! delete one at a time.

use freshrss_feed::http::Fetcher;
use url::Url;

use crate::db::dao::{category as cat_dao, feed as feed_dao, user as user_dao};
use crate::db::Db;
use crate::domain::feed::Feed;
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;
use crate::error::{Error, Result};
use crate::service::refresh::{refresh_feed, Outcome, Settings, Status};

/// Normalizes and validates a feed address.
///
/// Ports `FreshRSS_http_Util::checkUrl`: a bare `example.com` is what people actually paste into the
/// add dialog, so a missing scheme is filled in rather than rejected.
///
/// Only `http` and `https` are accepted. Rejecting the rest here rather than at fetch time keeps
/// `file:`, `gopher:` and friends out of the database entirely, so a later code path that forgets to
/// check cannot be talked into reading a local file.
pub fn normalize_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::BadUrl {
            url: raw.to_string(),
            reason: "the address is empty".into(),
        });
    }

    // Fill in the scheme the way a browser's address bar would.
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    let parsed = Url::parse(&candidate).map_err(|err| Error::BadUrl {
        url: raw.to_string(),
        reason: err.to_string(),
    })?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::BadUrl {
                url: raw.to_string(),
                reason: format!("only http and https addresses are supported, got {other:?}"),
            });
        }
    }
    if parsed.host_str().is_none() || parsed.host_str().is_some_and(str::is_empty) {
        return Err(Error::BadUrl {
            url: raw.to_string(),
            reason: "the address has no host".into(),
        });
    }

    Ok(parsed.to_string())
}

/// What the add-feed form supplies.
///
/// `url` is expected to have been through [`normalize_url`]; nothing here re-checks it, because the
/// same struct is built by the OPML importer, whose addresses come from a file rather than a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSubscription {
    pub id_user: i64,
    pub url: String,
    /// `None` or blank leaves the stored name empty, and the first refresh fills in the title the
    /// feed publishes. Storing the host as a fallback here instead would look friendlier and would
    /// permanently block that, since the refresh only adopts a title over an empty name.
    pub name: Option<String>,
    /// Folder to file it under. `None` means the user's `Uncategorized`.
    pub category: Option<i64>,
    pub kind: FeedKind,
    pub priority: Priority,
    pub ttl: Ttl,
}

impl NewSubscription {
    /// A plain RSS subscription with everything else left to its default.
    pub fn new(id_user: i64, url: impl Into<String>) -> Self {
        NewSubscription {
            id_user,
            url: url.into(),
            name: None,
            category: None,
            kind: FeedKind::default(),
            priority: Priority::default(),
            ttl: Ttl::default(),
        }
    }
}

/// A recorded subscription and what its first fetch did.
#[derive(Debug, Clone, PartialEq)]
pub struct Added {
    /// The feed as stored *after* the fetch, so it carries the title and website the feed published.
    pub feed: Feed,
    pub outcome: Outcome,
}

/// Records a subscription without fetching it.
///
/// This is the half the OPML importer uses, where fetching a hundred feeds inline would be the wrong
/// shape; the caller schedules refreshes afterwards instead.
///
/// # Errors
///
/// [`Error::Conflict`] when the user already subscribes to that address, [`Error::NotFound`] when the
/// folder does not belong to them.
pub async fn create(db: &Db, spec: &NewSubscription) -> Result<Feed> {
    let spec = spec.clone();
    // The writer lock, even though this touches no entries: the duplicate check and the insert have
    // to be one indivisible step, and there is no unique index on `feed(id_user, url)` to fall back
    // on. Without it, two adds of the same address racing from two browser tabs both pass the check
    // and both insert.
    db.with_writer(move |mut conn| async move {
        if feed_dao::find_by_url(&mut conn, spec.id_user, &spec.url)
            .await?
            .is_some()
        {
            return Err(Error::Conflict(
                "you are already subscribed to that address".into(),
            ));
        }

        // An explicit folder must belong to this user, or the request would file the feed under
        // someone else's — or fail the foreign key, which is the same leak with a worse message.
        let category = match spec.category {
            Some(id) => {
                let _ = cat_dao::find_by_id(&mut conn, id, spec.id_user)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("folder {id}")))?;
                Some(id)
            }
            None => user_dao::default_category_id(&mut conn, spec.id_user).await?,
        };

        let feed = Feed {
            category,
            kind: spec.kind,
            priority: spec.priority,
            ttl: spec.ttl,
            ..Feed::new(
                spec.id_user,
                spec.url.clone(),
                spec.name.clone().unwrap_or_default(),
            )
        };
        let id = feed_dao::insert(&mut conn, &feed).await?;
        feed_dao::find_by_id(&mut conn, id, spec.id_user)
            .await?
            .ok_or_else(|| Error::Internal("the feed vanished after insert".into()))
    })
    .await
}

/// Subscribes and pulls the first articles in one step.
///
/// # Errors
///
/// Anything [`create`] can fail with, plus [`Error::Http`] when the address could not be read as a
/// feed — in which case the row is removed again and the caller sees no trace of the attempt.
pub async fn add(
    db: &Db,
    fetcher: &(dyn Fetcher + '_),
    settings: &Settings,
    spec: &NewSubscription,
    now: i64,
) -> Result<Added> {
    let feed = create(db, spec).await?;
    let outcome = refresh_feed(db, fetcher, &feed, settings, now).await?;

    match &outcome.status {
        Status::NotModified | Status::Unchanged | Status::Fetched => {}
        Status::Muted => {
            discard(db, &feed).await?;
            return Err(Error::Http {
                status: 410,
                message: format!(
                    "{} answered 410 Gone; it is not worth subscribing to",
                    feed.url
                ),
            });
        }
        Status::Failed {
            message,
            http_status,
        } => {
            discard(db, &feed).await?;
            return Err(Error::Http {
                status: http_status.unwrap_or(0),
                message: message.clone(),
            });
        }
    }

    // Re-read rather than returned: the refresh adopted the feed's own title and website, and the
    // caller is about to render the sidebar.
    let (id, id_user) = (feed.id, feed.id_user);
    let feed = db
        .with(move |mut conn| async move { feed_dao::find_by_id(&mut conn, id, id_user).await })
        .await?
        .ok_or_else(|| Error::Internal("the feed vanished after its first refresh".into()))?;
    Ok(Added { feed, outcome })
}

/// Removes a subscription that could not be read, along with anything its first fetch managed to
/// store before failing.
async fn discard(db: &Db, feed: &Feed) -> Result<()> {
    let (id, id_user) = (feed.id, feed.id_user);
    db.with_writer(move |mut conn| async move { feed_dao::delete(&mut conn, id, id_user).await })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::reqwest::FixtureError;
    use freshrss_feed::http::{FixtureFetcher, FixtureResponse};

    use crate::db::support::db;

    use super::*;

    const URL: &str = "https://e.example/feed.xml";
    const NOW: i64 = 1_791_000_000;

    const V1: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>The Example Blog</title><link>https://e.example/blog</link>
<item><title>One</title><link>https://e.example/1</link><guid>g1</guid><description>Body</description></item>
<item><title>Two</title><link>https://e.example/2</link><guid>g2</guid><description>Body</description></item>
</channel></rss>"#;

    fn rss(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    async fn user(db: &Db) -> i64 {
        db.with(|mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: "alice",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await
        })
        .await
        .expect("created")
    }

    async fn feed_count(db: &Db) -> i64 {
        db.with(|mut conn| async move {
            Ok(sqlx::query_scalar("SELECT COUNT(*) FROM feed")
                .fetch_one(&mut *conn)
                .await?)
        })
        .await
        .expect("counted")
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

    // --- normalize_url ------------------------------------------------------

    #[test]
    fn a_bare_host_gains_a_scheme() {
        assert_eq!(
            normalize_url("example.com/feed.xml").expect("normalized"),
            "https://example.com/feed.xml"
        );
        assert_eq!(
            normalize_url("  https://example.com/f  ").expect("normalized"),
            "https://example.com/f",
            "surrounding whitespace is what a paste arrives with"
        );
        assert_eq!(
            normalize_url("http://example.com/f").expect("normalized"),
            "http://example.com/f",
            "an explicit http is kept rather than upgraded"
        );
    }

    #[test]
    fn a_scheme_that_is_not_http_is_refused_before_it_is_stored() {
        for bad in [
            "file:///etc/passwd",
            "gopher://example.com/",
            "ftp://example.com/f",
            "javascript:alert(1)",
            "",
            "   ",
        ] {
            assert!(
                matches!(normalize_url(bad), Err(Error::BadUrl { .. })),
                "{bad:?} must be refused as a bad url"
            );
        }
    }

    // --- create -------------------------------------------------------------

    #[tokio::test]
    async fn create_files_a_feed_under_the_users_default_folder() {
        let db = db().await;
        let id_user = user(&db).await;
        let feed = create(&db, &NewSubscription::new(id_user, URL))
            .await
            .expect("created");

        assert!(feed.id > 0);
        assert_eq!(feed.url, URL);
        assert_eq!(
            feed.name, "",
            "an empty name is left for the feed to fill in"
        );
        let default =
            db.with(move |mut conn| async move {
                user_dao::default_category_id(&mut conn, id_user).await
            })
            .await
            .expect("read");
        assert_eq!(feed.category, default);
    }

    #[tokio::test]
    async fn subscribing_twice_to_one_address_is_a_conflict() {
        let db = db().await;
        let id_user = user(&db).await;
        create(&db, &NewSubscription::new(id_user, URL))
            .await
            .expect("first");
        let again = create(&db, &NewSubscription::new(id_user, URL)).await;
        assert!(
            matches!(again, Err(Error::Conflict(_))),
            "expected a conflict, got {again:?}"
        );
        assert_eq!(feed_count(&db).await, 1);
    }

    #[tokio::test]
    async fn the_same_address_may_be_subscribed_by_two_users() {
        let db = db().await;
        let alice = user(&db).await;
        let bob = db
            .with(|mut conn| async move {
                user_dao::insert(
                    &mut conn,
                    &user_dao::NewUser {
                        username: "bob",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )
                .await
            })
            .await
            .expect("created");

        create(&db, &NewSubscription::new(alice, URL))
            .await
            .expect("alice");
        create(&db, &NewSubscription::new(bob, URL))
            .await
            .expect("bob is not subscribing to alice's copy");
        assert_eq!(feed_count(&db).await, 2);
    }

    #[tokio::test]
    async fn another_users_folder_is_not_a_destination() {
        let db = db().await;
        let alice = user(&db).await;
        let bob = db
            .with(|mut conn| async move {
                user_dao::insert(
                    &mut conn,
                    &user_dao::NewUser {
                        username: "bob",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )
                .await
            })
            .await
            .expect("created");
        let bobs_folder = db
            .with(
                move |mut conn| async move { user_dao::default_category_id(&mut conn, bob).await },
            )
            .await
            .expect("read")
            .expect("bob has a default folder");

        let spec = NewSubscription {
            category: Some(bobs_folder),
            ..NewSubscription::new(alice, URL)
        };
        assert!(
            matches!(create(&db, &spec).await, Err(Error::NotFound(_))),
            "alice must not be able to file a feed in bob's folder"
        );
        assert_eq!(feed_count(&db).await, 0);
    }

    // --- add ----------------------------------------------------------------

    #[tokio::test]
    async fn add_subscribes_and_pulls_the_first_articles() {
        let db = db().await;
        let id_user = user(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));

        let added = add(
            &db,
            &fetcher,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW,
        )
        .await
        .expect("added");

        assert_eq!(added.outcome.new_entries, 2);
        assert_eq!(added.outcome.status, Status::Fetched);
        assert_eq!(entry_count(&db).await, 2);
        assert_eq!(
            added.feed.name, "The Example Blog",
            "the returned feed carries the title it published, not the host"
        );
        assert_eq!(added.feed.website, "https://e.example/blog");
        assert_eq!(added.feed.cache_nb_entries, 2);
    }

    #[tokio::test]
    async fn an_address_that_is_not_a_feed_leaves_no_row_behind() {
        let db = db().await;
        let id_user = user(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss("<html><body>nope</body></html>"));

        let result = add(
            &db,
            &fetcher,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW,
        )
        .await;
        assert!(
            matches!(result, Err(Error::Http { .. })),
            "expected a readable failure, got {result:?}"
        );
        assert_eq!(feed_count(&db).await, 0, "nothing to delete by hand later");
        assert_eq!(entry_count(&db).await, 0);
    }

    #[tokio::test]
    async fn a_feed_that_is_gone_is_not_subscribed_to() {
        let db = db().await;
        let id_user = user(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, FixtureResponse::Error(FixtureError::Gone));

        let result = add(
            &db,
            &fetcher,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW,
        )
        .await;
        match result {
            Err(Error::Http { status, .. }) => assert_eq!(status, 410),
            other => panic!("expected a 410, got {other:?}"),
        }
        assert_eq!(
            feed_count(&db).await,
            0,
            "a retired feed is not a subscription"
        );
    }

    #[tokio::test]
    async fn a_transient_failure_carries_the_upstream_status() {
        let db = db().await;
        let id_user = user(&db).await;
        let fetcher =
            FixtureFetcher::new().with(URL, FixtureResponse::Error(FixtureError::Http(503)));

        let result = add(
            &db,
            &fetcher,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW,
        )
        .await;
        match result {
            Err(Error::Http { status, message }) => {
                assert_eq!(status, 503, "so the dialog can say whose fault it is");
                assert!(message.contains("503"), "{message}");
            }
            other => panic!("expected an upstream failure, got {other:?}"),
        }
        assert_eq!(feed_count(&db).await, 0);
    }

    #[tokio::test]
    async fn a_refused_subscription_can_be_retried() {
        let db = db().await;
        let id_user = user(&db).await;
        let broken = FixtureFetcher::new().with(URL, rss("<html><body>nope</body></html>"));
        assert!(add(
            &db,
            &broken,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW,
        )
        .await
        .is_err());

        // The row was removed, so the duplicate check does not now refuse the address that the user
        // is entitled to try again.
        let working = FixtureFetcher::new().with(URL, rss(V1));
        let added = add(
            &db,
            &working,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW + 1,
        )
        .await
        .expect("added on the second attempt");
        assert_eq!(added.outcome.new_entries, 2);
        assert_eq!(feed_count(&db).await, 1);
    }

    #[tokio::test]
    async fn a_name_the_user_typed_is_kept() {
        let db = db().await;
        let id_user = user(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        let spec = NewSubscription {
            name: Some("  Mine  ".to_owned()),
            ..NewSubscription::new(id_user, URL)
        };

        let added = add(&db, &fetcher, &Settings::default(), &spec, NOW)
            .await
            .expect("added");
        assert_eq!(added.feed.name.trim(), "Mine");
    }

    #[tokio::test]
    async fn the_first_articles_are_committed_not_just_staged() {
        let db = db().await;
        let id_user = user(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        let added = add(
            &db,
            &fetcher,
            &Settings::default(),
            &NewSubscription::new(id_user, URL),
            NOW,
        )
        .await
        .expect("added");

        // Read back through the table the reader lists from, not by counting rows: staging into
        // `entrytmp` without committing would leave the same article count in the database and an
        // empty list in the UI.
        let titles: Vec<String> = db
            .with(|mut conn| async move {
                Ok(sqlx::query_scalar("SELECT title FROM entry ORDER BY id")
                    .fetch_all(&mut *conn)
                    .await?)
            })
            .await
            .expect("read");
        assert_eq!(titles, vec!["One".to_owned(), "Two".to_owned()]);
        let staged: i64 = db
            .with(|mut conn| async move {
                Ok(sqlx::query_scalar("SELECT COUNT(*) FROM entrytmp")
                    .fetch_one(&mut *conn)
                    .await?)
            })
            .await
            .expect("counted");
        assert_eq!(staged, 0, "nothing was left in the staging table");
        assert_eq!(added.feed.cache_nb_unreads, 2, "both arrive unread");
    }
}
