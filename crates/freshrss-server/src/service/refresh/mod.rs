//! The refresh pipeline: one feed, from "it is due" to "the database agrees with upstream".
//!
//! The order of the steps is the design. Fetch, then compare a fingerprint of the body before
//! parsing anything, then reconcile what the feed published against what is stored, then write —
//! all of it inside one writer lock, because a commit that interleaves with another user's commit is
//! how two refreshes end up handing themselves the same entry ids.
//!
//! Three rules from FreshRSS are load-bearing and easy to lose:
//!
//! - **An article is never deleted because it vanished from the feed.** A feed truncating its window
//!   to the last ten items is normal; treating that as deletion would destroy the user's archive.
//!   Vanishing is recorded by *not* advancing `lastSeen`, and retention (S5) is what acts on age.
//! - **An edit updates in place.** The id never changes, so read and starred state survive — and
//!   those two are written with `COALESCE`, so an upstream edit cannot un-read an article.
//! - **A 304 still advances `lastSeen`.** Otherwise a healthy feed that rarely publishes would have
//!   its whole backlog age out on the retention schedule.

pub mod reconcile;
pub mod run;

use std::collections::BTreeMap;
use std::time::Duration;

use freshrss_feed::http::{FetchOutcome, FetchRequest, Fetcher};
use freshrss_feed::{parse_feed, scrape_feed, FeedError, RawFeed};
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::db::dao::{entry as entry_dao, feed as feed_dao};
use crate::db::Db;
use crate::domain::attributes::keys;
use crate::domain::feed::Feed;
use crate::domain::kind::FeedKind;
use crate::error::Result;
use crate::service::ingest::{self, Sanitize};
use reconcile::reconcile;

/// Everything a refresh needs that is not per-feed.
///
/// Gathered once per run rather than read out of [`AppConfig`] per feed, so the pipeline can be
/// driven from a test with values that have nothing to do with a config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub user_agent: String,
    pub timeout: Duration,
    pub max_bytes: u64,
    /// Hosts whose `http://` links are upgraded to `https://` in article bodies.
    pub force_https_domains: Vec<String>,
    /// A byte budget for one article body, or `None` for unlimited.
    pub max_content_length: Option<usize>,
    /// Used when a feed's own `ttl` is 0, and as the magnitude when muting one that has none.
    pub ttl_default: i64,
    /// How many feeds one run may have in flight at once. The limit is the operator's, not the
    /// feed's: a user with a thousand subscriptions must not open a thousand sockets.
    pub concurrency: usize,
    /// How many feeds one run takes on. The rest wait for the next tick rather than making one
    /// refresh run for as long as the backlog does.
    pub max_feeds: usize,
}

impl Settings {
    pub fn from_config(config: &AppConfig) -> Self {
        Settings {
            user_agent: config.http.user_agent.clone(),
            timeout: Duration::from_secs(config.limits.timeout_secs),
            max_bytes: config.limits.max_body_bytes as u64,
            force_https_domains: Vec::new(),
            max_content_length: None,
            ttl_default: config.limits.ttl_default,
            concurrency: config.limits.refresh_concurrency,
            max_feeds: config.limits.max_feeds_per_refresh,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            user_agent: freshrss_feed::http::DEFAULT_USER_AGENT.to_owned(),
            timeout: Duration::from_secs(20),
            max_bytes: 32 * 1024 * 1024,
            force_https_domains: Vec::new(),
            max_content_length: None,
            ttl_default: 3600,
            concurrency: 4,
            max_feeds: 10,
        }
    }
}

/// How one feed's refresh ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The server answered 304: nothing was transferred, nothing was parsed.
    NotModified,
    /// The body was byte-identical to the previous fetch, so nothing was parsed.
    Unchanged,
    /// The feed was fetched, parsed and reconciled.
    Fetched,
    /// The feed answered 410 Gone and was muted, so it will not be retried.
    Muted,
    /// The refresh failed. The message is what the problems dialog shows, and the status is what
    /// lets a caller tell "that address is not a feed" (404) from "that host is having a bad day"
    /// (503) without parsing the message back apart.
    Failed {
        message: String,
        http_status: Option<u16>,
    },
}

impl Status {
    /// True when the feed was read successfully, whether or not anything in it had changed.
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Status::NotModified | Status::Unchanged | Status::Fetched
        )
    }
}

/// What one feed's refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub new_entries: usize,
    pub updated_entries: usize,
    /// Articles the feed published that were already stored unchanged.
    pub unchanged: usize,
}

impl Outcome {
    fn empty(status: Status) -> Self {
        Outcome {
            status,
            new_entries: 0,
            updated_entries: 0,
            unchanged: 0,
        }
    }
}

/// Refreshes one feed.
///
/// `now` is the refresh's timestamp in unix seconds and is used for every write it makes, so a slow
/// run stamps one coherent instant rather than a spread of them.
///
/// # Errors
///
/// Only when the *database* fails. A feed that cannot be fetched is not an error of this operation:
/// it comes back as [`Status::Failed`] or [`Status::Muted`], because one broken subscription out of
/// a hundred must not abort the run, and the caller reports it per feed.
pub async fn refresh_feed(
    db: &Db,
    fetcher: &(dyn Fetcher + '_),
    feed: &Feed,
    settings: &Settings,
    now: i64,
) -> Result<Outcome> {
    let request = request_for(feed, settings);
    match fetcher.fetch(&request).await {
        Ok(FetchOutcome::NotModified) => {
            // Nothing came back, so `lastSeen` advances for every article seen at the previous
            // refresh rather than for a guid list this run never produced.
            touch(db, feed, now, None, Status::NotModified).await
        }
        Ok(FetchOutcome::Found(body)) => {
            if !body.changed_since(feed.simplepie_hash()) {
                return touch(
                    db,
                    feed,
                    now,
                    Some((
                        &body.hash,
                        body.etag.as_deref(),
                        body.last_modified.as_deref(),
                    )),
                    Status::Unchanged,
                )
                .await;
            }
            fetched(db, feed, settings, now, body).await
        }
        Err(err) => fail(db, feed, settings, now, &err).await,
    }
}

/// The fetch request for one feed, from its own settings where it has them.
pub fn request_for(feed: &Feed, settings: &Settings) -> FetchRequest {
    let mut request = FetchRequest::new(feed.url.clone()).with_kind(wire_kind(feed.kind));
    request.user_agent = settings.user_agent.clone();
    request.timeout = feed
        .timeout_secs()
        .map(Duration::from_secs)
        .unwrap_or(settings.timeout);
    request.max_bytes = settings.max_bytes;
    request.ssl_verify = feed.ssl_verify();
    request.basic_auth = feed.http_auth_credentials();
    request.conditional = feed.conditional_headers();
    request
}

/// Records a refresh that produced no articles to reconcile.
///
/// `validators` carries the new body fingerprint and conditional-GET headers when there was a body
/// to fingerprint; a 304 has neither, and must not clear the ones already stored.
async fn touch(
    db: &Db,
    feed: &Feed,
    now: i64,
    validators: Option<(&str, Option<&str>, Option<&str>)>,
    status: Status,
) -> Result<Outcome> {
    let id = feed.id;
    let id_user = feed.id_user;
    let mut stored = feed.clone();
    if let Some((hash, etag, last_modified)) = validators {
        stored.set_simplepie_hash(Some(hash));
        stored.set_conditional_headers(etag, last_modified);
    }
    // A feed that answers at all is not broken, so whatever the last failure said is stale.
    stored.set_last_error_message(None);
    let attributes = stored.attributes.clone();

    db.with_writer(move |mut conn| async move {
        entry_dao::update_last_seen_unchanged(&mut conn, id, now).await?;
        feed_dao::update_last_update(&mut conn, id, now).await?;
        feed_dao::update_attributes(&mut conn, id, id_user, &attributes).await?;
        feed_dao::update_cached_values(&mut conn, &[id]).await?;
        Ok(())
    })
    .await?;
    Ok(Outcome::empty(status))
}

/// Handles a body that differs from the last fetch.
async fn fetched(
    db: &Db,
    feed: &Feed,
    settings: &Settings,
    now: i64,
    body: freshrss_feed::FetchedBody,
) -> Result<Outcome> {
    let raw = match read_body(&body.body, feed) {
        Ok(raw) => raw,
        Err(err) => return fail(db, feed, settings, now, &err).await,
    };

    let sanitize = Sanitize {
        force_https_domains: settings.force_https_domains.clone(),
        max_length: settings.max_content_length,
    };
    let ingested = ingest::ingest(feed, &raw, &sanitize, now);
    if ingested.invalid_guids > 0 {
        warn!(
            feed = feed.id,
            url = %feed.url,
            invalid = ingested.invalid_guids,
            degraded = ingested.degraded,
            "the feed published blank or duplicate GUIDs"
        );
    }
    if ingested.degraded {
        info!(
            feed = feed.id,
            criteria = ingested.criteria.as_str(),
            "degraded the feed's GUID policy so its articles stay distinguishable"
        );
    }

    // Every guid the feed published, including the unchanged ones: `lastSeen` is what says "this
    // article was still upstream at this refresh", and an article that did not change still was.
    let guids: Vec<String> = ingested.entries.iter().map(|e| e.guid.clone()).collect();
    let entries = ingested.entries;
    let criteria = ingested.criteria;
    let degraded = ingested.degraded;

    let id = feed.id;
    let id_user = feed.id_user;
    let mut stored = feed.clone();
    stored.set_simplepie_hash(Some(&body.hash));
    stored.set_conditional_headers(body.etag.as_deref(), body.last_modified.as_deref());
    stored.set_last_error_message(None);
    if degraded {
        stored
            .attributes
            .set_str(keys::UNICITY_CRITERIA, Some(criteria.as_str()));
    }
    adopt_identity(&mut stored, &raw);
    // What actually has to be written, so a refresh that changed nothing about the feed's identity
    // does not spend two updates saying so.
    let renamed = stored.name != feed.name;
    let redescribed = stored.website != feed.website || stored.description != feed.description;
    // A 301 means the subscription should move, or every future fetch pays for a redirect that will
    // eventually stop being served.
    let moved_to = body
        .permanently_moved
        .then(|| body.effective_url.clone())
        .filter(|url| !url.is_empty() && url != &feed.url);
    let (new_entries, updated_entries, unchanged) = db
        .with_writer(move |mut conn| async move {
            let known = entry_dao::list_hashes_for_guids(&mut conn, id, &guids).await?;
            let plan = reconcile(entries, &known, now);
            let (insert, update, unchanged) = (plan.insert, plan.update, plan.unchanged);

            let mut new_entries = 0;
            if !insert.is_empty() {
                for entry in &insert {
                    entry_dao::stage(&mut conn, entry).await?;
                }
                new_entries = entry_dao::commit_staged(&mut conn, id_user).await?;
            }
            let mut updated_entries = 0;
            for entry in &update {
                if entry_dao::update_existing(&mut conn, entry).await? {
                    updated_entries += 1;
                }
            }

            entry_dao::update_last_seen(&mut conn, id, &guids, now).await?;
            feed_dao::update_last_update(&mut conn, id, now).await?;
            feed_dao::update_attributes(&mut conn, id, id_user, &stored.attributes).await?;
            if renamed {
                feed_dao::update_name(&mut conn, id, id_user, &stored.name).await?;
            }
            if redescribed {
                feed_dao::update_website_description(
                    &mut conn,
                    id,
                    id_user,
                    &stored.website,
                    &stored.description,
                )
                .await?;
            }
            if let Some(url) = &moved_to {
                feed_dao::update_url(&mut conn, id, id_user, url).await?;
            }
            feed_dao::update_cached_values(&mut conn, &[id]).await?;
            Ok((new_entries, updated_entries, unchanged))
        })
        .await?;

    Ok(Outcome {
        status: Status::Fetched,
        new_entries,
        updated_entries,
        unchanged,
    })
}

/// Fills in the feed's own name, website and description from what it published.
///
/// Only where they are blank, which is PHP's rule — `refreshFeed` checks `name(true) === ''`, the
/// *stored* name rather than the displayed one, so the host fallback never counts as an answer. A
/// name the user typed in the add dialog is theirs, and a feed renaming itself three years later
/// must not quietly rewrite it. The website is also replaced when it merely equals the feed URL,
/// because that is the placeholder a new subscription starts with and it carries no information.
fn adopt_identity(stored: &mut Feed, raw: &RawFeed) {
    if stored.name.trim().is_empty() {
        if let Some(title) = nonblank(raw.title.as_deref()) {
            stored.name = title.to_owned();
        }
    }
    if stored.website.trim().is_empty() || stored.website == stored.url {
        if let Some(website) = nonblank(raw.website.as_deref()) {
            stored.website = website.to_owned();
        }
    }
    if stored.description.trim().is_empty() {
        if let Some(description) = nonblank(raw.description.as_deref()) {
            stored.description = description.to_owned();
        }
    }
}

fn nonblank(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// Records a failure, and mutes the feed when the server said it is gone for good.
async fn fail(
    db: &Db,
    feed: &Feed,
    settings: &Settings,
    now: i64,
    err: &FeedError,
) -> Result<Outcome> {
    let gone = matches!(err, FeedError::Gone);
    let message = err.to_string();
    if gone {
        warn!(feed = feed.id, url = %feed.url, "the feed answered 410 Gone; muting it");
    } else {
        warn!(feed = feed.id, url = %feed.url, error = %message, "refresh failed");
    }

    let id = feed.id;
    let id_user = feed.id_user;
    // The timestamp in `feed.error` says when it broke; this says why, and the problems dialog needs
    // both. Stored rather than kept in memory, because the dialog is usually opened long after the
    // refresh that failed.
    let mut stored = feed.clone();
    stored.set_last_error_message(Some(&message));
    let attributes = stored.attributes.clone();
    // The owner's default supplies the magnitude when the feed has none, because muting is the sign
    // of one stored integer and a feed on the default has a magnitude of 0. See [`Ttl::mute`].
    let ttl = feed
        .ttl
        .mute(i32::try_from(settings.ttl_default).unwrap_or(i32::MAX));

    db.with_writer(move |mut conn| async move {
        feed_dao::update_last_error(&mut conn, id, now).await?;
        feed_dao::update_attributes(&mut conn, id, id_user, &attributes).await?;
        if gone {
            feed_dao::update_ttl(&mut conn, id, id_user, ttl).await?;
        }
        Ok(())
    })
    .await?;

    Ok(Outcome::empty(if gone {
        Status::Muted
    } else {
        Status::Failed {
            message,
            http_status: err.http_status(),
        }
    }))
}

/// Reads a body according to the feed's kind.
fn read_body(body: &[u8], feed: &Feed) -> std::result::Result<RawFeed, FeedError> {
    match feed.kind {
        FeedKind::HtmlXpath
        | FeedKind::XmlXpath
        | FeedKind::JsonDotNotation
        | FeedKind::HtmlXpathJsonDotNotation => {
            let map = selector_map(feed);
            scrape_feed(
                body,
                &freshrss_feed::parse::Scrape {
                    kind: wire_kind(feed.kind),
                    map: &map,
                    x_path_to_json: feed
                        .attributes
                        .get_str(keys::XPATH_TO_JSON)
                        .unwrap_or_default(),
                    url: &feed.url,
                    fallback_title: feed.display_name(),
                },
            )
        }
        kind => parse_feed(body, wire_kind(kind)),
    }
}

/// The feed crate's kind for the domain's.
///
/// Two enums for one concept is a wart: the domain one exists so the DTOs and the DAO can carry a
/// kind without depending on the feed crate's type, and the codes are the same on-disk integers, so
/// the conversion cannot lose anything. `from_i16` degrades an unknown code to plain RSS rather than
/// failing, which is right here — a row written by a newer version still has to be refreshable.
fn wire_kind(kind: FeedKind) -> freshrss_feed::FeedKind {
    freshrss_feed::FeedKind::from_i16(kind.as_i16())
}

/// The feed's selector map, in the form the scrapers take.
fn selector_map(feed: &Feed) -> BTreeMap<String, String> {
    let source = match feed.kind {
        FeedKind::JsonDotNotation | FeedKind::HtmlXpathJsonDotNotation => feed.json_dotnotation(),
        _ => feed.xpath(),
    };
    source
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::reqwest::FixtureError;
    use freshrss_feed::http::{FetchedBody, FixtureFetcher, FixtureResponse};

    use crate::db::dao::{entry as entry_dao, feed as feed_dao, user as user_dao};
    use crate::db::support::db;
    use crate::domain::entry::Entry;

    use super::*;

    const URL: &str = "https://e.example/feed.xml";
    /// Later than every `pubDate` in the fixtures, so an article with no date sorts last rather
    /// than first.
    const NOW: i64 = 1_791_000_000;

    /// Two articles, `g1` and `g2`.
    const V1: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://e.example/</link>
<item><title>One</title><link>https://e.example/1</link><guid>g1</guid><pubDate>Thu, 01 Oct 2026 08:00:00 GMT</pubDate><description>First body</description></item>
<item><title>Two</title><link>https://e.example/2</link><guid>g2</guid><description>Second body</description></item>
</channel></rss>"#;

    /// The same feed with `g1` edited and `g3` added, `g2` untouched.
    const V2: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://e.example/</link>
<item><title>One, corrected</title><link>https://e.example/1</link><guid>g1</guid><pubDate>Thu, 01 Oct 2026 08:00:00 GMT</pubDate><description>First body</description></item>
<item><title>Two</title><link>https://e.example/2</link><guid>g2</guid><description>Second body</description></item>
<item><title>Three</title><link>https://e.example/3</link><guid>g3</guid><description>Third body</description></item>
</channel></rss>"#;

    /// A body with no validators, so the conditional-GET path cannot short-circuit it and the test
    /// exercises whatever it says it does.
    fn rss(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    /// The same body as a server that does send validators, so a replay of them answers 304.
    fn rss_validated(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: Some("W/\"v1\"".to_owned()),
            last_modified: Some("Thu, 01 Oct 2026 08:00:00 GMT".to_owned()),
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    /// One user, one feed in their default folder.
    async fn setup(db: &Db) -> Feed {
        setup_named(db, "Example").await
    }

    /// The same, with the stored name chosen by the caller. An empty name is what the add dialog
    /// produces when the user typed nothing, and it is what lets a refresh fill the real title in.
    async fn setup_named(db: &Db, name: &str) -> Feed {
        let name = name.to_owned();
        db.with(move |mut conn| async move {
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
            let mut feed = Feed::new(id_user, URL, name);
            feed.category = user_dao::default_category_id(&mut conn, id_user).await?;
            feed.id = feed_dao::insert(&mut conn, &feed).await?;
            Ok(feed)
        })
        .await
        .expect("seeded")
    }

    async fn entry(db: &Db, feed_id: i64, guid: &str) -> Option<Entry> {
        let guid = guid.to_owned();
        db.with(
            move |mut conn| async move { entry_dao::find_by_guid(&mut conn, feed_id, &guid).await },
        )
        .await
        .expect("queried")
    }

    async fn stored_feed(db: &Db, feed: &Feed) -> Feed {
        let (id, id_user) = (feed.id, feed.id_user);
        db.with(move |mut conn| async move { feed_dao::find_by_id(&mut conn, id, id_user).await })
            .await
            .expect("queried")
            .expect("still there")
    }

    async fn all_entries(db: &Db, id_user: i64) -> Vec<Entry> {
        db.with(move |mut conn| async move {
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM entry WHERE id_user = ?1 ORDER BY id")
                    .bind(id_user)
                    .fetch_all(&mut *conn)
                    .await?;
            let ids: Vec<_> = ids
                .into_iter()
                .map(crate::domain::ids::EntryId::from_micros)
                .collect();
            entry_dao::list_by_ids(&mut conn, id_user, &ids).await
        })
        .await
        .expect("queried")
    }

    #[tokio::test]
    async fn a_first_refresh_inserts_the_articles() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));

        let outcome = refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");
        assert_eq!(outcome.status, Status::Fetched);
        assert_eq!(outcome.new_entries, 2);
        assert_eq!(outcome.updated_entries, 0);

        let entries = all_entries(&db, feed.id_user).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].guid, "g1");
        assert_eq!(entries[0].title, "One");
        assert_eq!(
            entries[0].content, "First body",
            "the summary becomes the body, and plain text is not wrapped in markup"
        );
        assert_eq!(
            entries[0].date, 1_790_841_600,
            "Thu, 01 Oct 2026 08:00:00 GMT"
        );
        assert!(
            entries[0].id.micros() > 0,
            "a real id came from the allocator"
        );
        assert!(!entries[0].is_read);

        let after = stored_feed(&db, &feed).await;
        assert_eq!(after.last_update, NOW);
        assert_eq!(after.error, 0);
        assert!(
            after.simplepie_hash().is_some(),
            "the fingerprint was stored"
        );
        assert_eq!(after.cache_nb_entries, 2);
        assert_eq!(after.cache_nb_unreads, 2);
    }

    /// A channel that says what it is called, where it lives and what it is about.
    const IDENTIFIED: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>The Example Blog</title><link>https://e.example/blog</link>
<description>Everything about examples.</description>
<item><title>One</title><link>https://e.example/1</link><guid>g1</guid></item>
</channel></rss>"#;

    #[tokio::test]
    async fn a_first_refresh_fills_in_the_identity_the_feed_publishes() {
        let db = db().await;
        // An empty stored name is what the add dialog produces when the user typed nothing, so the
        // sidebar shows the host until the feed says otherwise.
        let feed = setup_named(&db, "").await;
        assert_eq!(feed.display_name(), "e.example");

        let fetcher = FixtureFetcher::new().with(URL, rss(IDENTIFIED));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");

        let after = stored_feed(&db, &feed).await;
        assert_eq!(after.name, "The Example Blog");
        assert_eq!(after.display_name(), "The Example Blog");
        assert_eq!(after.website, "https://e.example/blog");
        assert_eq!(after.description, "Everything about examples.");
    }

    #[tokio::test]
    async fn an_identity_the_user_chose_survives_the_feed_renaming_itself() {
        let db = db().await;
        let feed = setup_named(&db, "  Mine  ").await;
        let fetcher = FixtureFetcher::new().with(URL, rss(IDENTIFIED));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");
        // And again, because a name overwritten on the *second* refresh is the same bug with a
        // longer fuse.
        let again = stored_feed(&db, &feed).await;
        refresh_feed(&db, &fetcher, &again, &Settings::default(), NOW + 60)
            .await
            .expect("second");

        let after = stored_feed(&db, &feed).await;
        assert_eq!(after.name, "  Mine  ", "a name the user typed is theirs");
        assert_eq!(
            after.website, "https://e.example/blog",
            "but a blank website is still filled in"
        );
        assert_eq!(after.description, "Everything about examples.");
    }

    #[tokio::test]
    async fn a_feed_that_publishes_no_description_does_not_have_one_invented() {
        let db = db().await;
        let feed = setup_named(&db, "Example").await;
        // V1 has no channel description, so there is nothing to adopt and the stored row must be
        // left exactly as it was rather than being blanked.
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");

        let after = stored_feed(&db, &feed).await;
        assert_eq!(after.name, "Example");
        assert_eq!(after.description, "", "nothing published, nothing stored");
    }

    #[tokio::test]
    async fn an_identical_body_is_not_reparsed_but_last_seen_still_advances() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");

        let again = stored_feed(&db, &feed).await;
        let outcome = refresh_feed(&db, &fetcher, &again, &Settings::default(), NOW + 3600)
            .await
            .expect("second");
        assert_eq!(outcome.status, Status::Unchanged);
        assert_eq!(outcome.new_entries, 0);

        // Without this a healthy feed that publishes nothing would have its whole backlog age out
        // on the retention schedule.
        let entry = entry(&db, feed.id, "g1").await.expect("still there");
        assert_eq!(entry.last_seen, NOW + 3600);
        assert_eq!(
            all_entries(&db, feed.id_user).await.len(),
            2,
            "no duplicates"
        );
    }

    #[tokio::test]
    async fn a_304_advances_last_seen_and_keeps_the_validators() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss_validated(V1));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");

        // The fixture fetcher answers 304 when the conditional headers it stored come back.
        let again = stored_feed(&db, &feed).await;
        assert_eq!(
            again.conditional_headers().etag.as_deref(),
            Some("W/\"v1\""),
            "the validator was stored so it can be replayed"
        );
        let outcome = refresh_feed(&db, &fetcher, &again, &Settings::default(), NOW + 7200)
            .await
            .expect("second");
        assert_eq!(outcome.status, Status::NotModified);

        let requests = fetcher.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].conditional.etag.as_deref(), Some("W/\"v1\""));
        assert_eq!(
            requests[1].conditional.last_modified.as_deref(),
            Some("Thu, 01 Oct 2026 08:00:00 GMT")
        );

        let entry = entry(&db, feed.id, "g1").await.expect("still there");
        assert_eq!(entry.last_seen, NOW + 7200);
        let after = stored_feed(&db, &feed).await;
        assert_eq!(
            after.conditional_headers().etag.as_deref(),
            Some("W/\"v1\""),
            "a 304 carries no validators, so the stored ones must survive it"
        );
    }

    #[tokio::test]
    async fn an_edited_article_updates_in_place_and_a_new_one_is_added() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");
        let before = entry(&db, feed.id, "g1").await.expect("g1");

        let fetcher = FixtureFetcher::new().with(URL, rss(V2));
        let again = stored_feed(&db, &feed).await;
        let outcome = refresh_feed(&db, &fetcher, &again, &Settings::default(), NOW + 3600)
            .await
            .expect("second");
        assert_eq!(outcome.new_entries, 1, "only g3 is new");
        assert_eq!(outcome.updated_entries, 1, "only g1 changed");
        assert_eq!(outcome.unchanged, 1, "g2 was left alone");

        let entries = all_entries(&db, feed.id_user).await;
        assert_eq!(entries.len(), 3, "an edit is not a duplicate");

        let after = entry(&db, feed.id, "g1").await.expect("g1");
        assert_eq!(after.id, before.id, "the id never changes on an edit");
        assert_eq!(after.title, "One, corrected");
        assert_eq!(after.last_modified, Some(NOW + 3600));
        assert_eq!(after.last_seen, NOW + 3600);

        let untouched = entry(&db, feed.id, "g2").await.expect("g2");
        assert_eq!(untouched.last_modified, None, "g2 was not rewritten");
        assert_eq!(untouched.last_seen, NOW + 3600, "but it was still upstream");
    }

    #[tokio::test]
    async fn read_and_starred_state_survive_an_edit() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");

        let (id, id_user) = (feed.id, feed.id_user);
        let ids: Vec<crate::domain::ids::EntryId> = db
            .with(move |mut conn| async move {
                let found = entry_dao::find_by_guid(&mut conn, id, "g1")
                    .await?
                    .expect("g1");
                entry_dao::mark_read(&mut conn, id_user, &[found.id], true).await?;
                entry_dao::mark_favorite(&mut conn, id_user, &[found.id], true).await?;
                Ok(vec![found.id])
            })
            .await
            .expect("marked");
        assert_eq!(ids.len(), 1);

        let fetcher = FixtureFetcher::new().with(URL, rss(V2));
        let again = stored_feed(&db, &feed).await;
        refresh_feed(&db, &fetcher, &again, &Settings::default(), NOW + 3600)
            .await
            .expect("second");

        let after = entry(&db, feed.id, "g1").await.expect("g1");
        assert!(
            after.is_read,
            "an upstream edit must not un-read an article"
        );
        assert!(after.is_favorite, "nor un-star it");
        assert_eq!(after.title, "One, corrected", "but the edit still landed");
    }

    #[tokio::test]
    async fn a_gone_feed_is_muted_and_flagged() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, FixtureResponse::Error(FixtureError::Gone));

        let outcome = refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");
        assert_eq!(outcome.status, Status::Muted);

        let after = stored_feed(&db, &feed).await;
        assert!(after.ttl.is_muted(), "{:?}", after.ttl);
        assert!(
            after.ttl.seconds() > 0,
            "a feed on the default ttl still gets a real magnitude, or -0 would leave it unmuted: {:?}",
            after.ttl
        );
        assert_eq!(after.error, NOW, "so it shows in the problems dialog");
    }

    #[tokio::test]
    async fn a_transient_failure_is_recorded_without_muting() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher =
            FixtureFetcher::new().with(URL, FixtureResponse::Error(FixtureError::Http(503)));

        let outcome = refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");
        assert!(
            matches!(&outcome.status, Status::Failed { message, http_status }
                if message.contains("503") && *http_status == Some(503)),
            "{:?}",
            outcome.status
        );

        let after = stored_feed(&db, &feed).await;
        assert!(!after.ttl.is_muted(), "a 503 is not a retirement");
        assert_eq!(after.error, NOW);
        assert_eq!(
            after.last_update, 0,
            "a failure is not a successful refresh"
        );
        assert!(
            after
                .last_error_message()
                .is_some_and(|m| m.contains("503")),
            "the dialog needs to say why, not just when: {:?}",
            after.last_error_message()
        );

        // And a recovery clears it, or the dialog would keep blaming a feed that is fine now.
        let fetcher = FixtureFetcher::new().with(URL, rss(V1));
        refresh_feed(&db, &fetcher, &after, &Settings::default(), NOW + 60)
            .await
            .expect("recovered");
        let recovered = stored_feed(&db, &feed).await;
        assert_eq!(recovered.error, 0);
        assert_eq!(recovered.last_error_message(), None);
    }

    #[tokio::test]
    async fn an_unparsable_body_is_a_failure_not_a_panic() {
        let db = db().await;
        let feed = setup(&db).await;
        let fetcher = FixtureFetcher::new().with(URL, rss("<html><body>not a feed</body></html>"));
        let outcome = refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");
        assert!(
            matches!(
                outcome.status,
                Status::Failed {
                    http_status: None,
                    ..
                }
            ),
            "a body that is not a feed has no HTTP status behind it: {outcome:?}"
        );
        assert!(all_entries(&db, feed.id_user).await.is_empty());
    }

    #[tokio::test]
    async fn duplicated_guids_degrade_the_policy_and_persist_it() {
        let db = db().await;
        let feed = setup(&db).await;
        // Three articles, one guid: without degradation they would collapse into a single row and
        // two of them would be lost on every refresh, silently.
        let body = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title>
<item><title>One</title><link>https://e.example/1</link><guid>same</guid></item>
<item><title>Two</title><link>https://e.example/2</link><guid>same</guid></item>
<item><title>Three</title><link>https://e.example/3</link><guid>same</guid></item>
</channel></rss>"#;
        let fetcher = FixtureFetcher::new().with(URL, rss(body));

        let outcome = refresh_feed(&db, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("refreshed");
        assert_eq!(outcome.new_entries, 3, "all three survived");

        let after = stored_feed(&db, &feed).await;
        assert_eq!(
            after.unicity_criteria(),
            Some("sha1:link_published"),
            "the degraded policy is persisted so the next refresh does not rediscover the problem"
        );
    }

    #[tokio::test]
    async fn a_second_refresh_after_a_degradation_is_stable() {
        let db = db().await;
        let feed = setup(&db).await;
        let body = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title>
<item><title>One</title><link>https://e.example/1</link><guid>same</guid></item>
<item><title>Two</title><link>https://e.example/2</link><guid>same</guid></item>
</channel></rss>"#;
        refresh_feed(
            &db,
            &FixtureFetcher::new().with(URL, rss(body)),
            &feed,
            &Settings::default(),
            NOW,
        )
        .await
        .expect("first");
        let after = stored_feed(&db, &feed).await;
        let ids: Vec<_> = all_entries(&db, feed.id_user)
            .await
            .iter()
            .map(|e| e.id)
            .collect();

        // Same body, so the fingerprint matches and nothing is reparsed at all.
        let outcome = refresh_feed(
            &db,
            &FixtureFetcher::new().with(URL, rss(body)),
            &after,
            &Settings::default(),
            NOW + 60,
        )
        .await
        .expect("second");
        assert_eq!(outcome.status, Status::Unchanged);
        assert_eq!(all_entries(&db, feed.id_user).await.len(), ids.len());
    }

    /// A fetcher that reports a permanent redirect, which the fixture fetcher cannot express.
    struct MovedFetcher {
        to: String,
    }

    #[async_trait::async_trait]
    impl Fetcher for MovedFetcher {
        async fn fetch(
            &self,
            _request: &FetchRequest,
        ) -> std::result::Result<FetchOutcome, FeedError> {
            let body = V1.as_bytes().to_vec();
            Ok(FetchOutcome::Found(FetchedBody {
                hash: freshrss_feed::http::body_hash(&body),
                body,
                status: 200,
                etag: None,
                last_modified: None,
                content_type: Some("application/rss+xml".to_owned()),
                effective_url: self.to.clone(),
                permanently_moved: true,
            }))
        }
    }

    #[tokio::test]
    async fn a_permanent_redirect_moves_the_subscription() {
        let db = db().await;
        let feed = setup(&db).await;
        let moved = "https://e.example/moved.xml".to_owned();
        let outcome = refresh_feed(
            &db,
            &MovedFetcher { to: moved.clone() },
            &feed,
            &Settings::default(),
            NOW,
        )
        .await
        .expect("refreshed");
        assert_eq!(outcome.new_entries, 2);
        assert_eq!(stored_feed(&db, &feed).await.url, moved);
    }

    /// A feed of `count` articles with distinct guids, for the concurrency test.
    fn generated(prefix: &str, count: usize) -> String {
        let items: String = (0..count)
            .map(|n| {
                format!(
                    "<item><title>{prefix} {n}</title><link>https://{prefix}.example/{n}</link>\
                     <guid>{prefix}-{n}</guid><description>Body {n}</description></item>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>{prefix}</title>{items}</channel></rss>"
        )
    }

    // Multi-threaded on purpose: on the default current-thread runtime the two refreshes only
    // interleave where they await, and the read-modify-write that allocates ids is never actually
    // interrupted — the test passes with the writer lock removed, which makes it decoration rather
    // than a check. Two OS threads make the race real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_users_refreshing_at_once_neither_collide_nor_lose_articles() {
        // The risk this exists for: ids are allocated by a read-modify-write on a `meta` row, so two
        // commits that interleave would hand themselves the same range, and the `INSERT OR IGNORE`
        // that protects against a duplicate guid would then *silently drop* articles. Nothing errors;
        // the user just never sees some of them.
        //
        // A file-backed database with a pool of two, because an in-memory one is capped at a single
        // connection and would serialize these for us, proving nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, db) = crate::db::support::file_db(dir.path()).await;

        let (alice, bob) = db
            .with(|mut conn| async move {
                let mut users = Vec::new();
                for name in ["alice", "bob"] {
                    let id_user = user_dao::insert(
                        &mut conn,
                        &user_dao::NewUser {
                            username: name,
                            email: None,
                            password_hash: "argon2id$hash",
                            is_admin: false,
                        },
                    )
                    .await?;
                    let mut feed =
                        Feed::new(id_user, format!("https://{name}.example/feed.xml"), name);
                    feed.category = user_dao::default_category_id(&mut conn, id_user).await?;
                    feed.id = feed_dao::insert(&mut conn, &feed).await?;
                    users.push(feed);
                }
                Ok((users.remove(0), users.remove(0)))
            })
            .await
            .expect("seeded");

        const EACH: usize = 200;
        let fetcher = FixtureFetcher::new()
            .with(alice.url.clone(), rss(&generated("alice", EACH)))
            .with(bob.url.clone(), rss(&generated("bob", EACH)));
        let settings = Settings::default();

        // Deliberately not sequenced, and on separate tasks so they run on separate threads: the
        // point is that they overlap.
        let (left_db, right_db) = (db.clone(), db.clone());
        let (left_feed, right_feed) = (alice.clone(), bob.clone());
        let (left_fetcher, right_fetcher) = (fetcher.clone(), fetcher);
        let (left_settings, right_settings) = (settings.clone(), settings);
        let left = tokio::spawn(async move {
            refresh_feed(&left_db, &left_fetcher, &left_feed, &left_settings, NOW).await
        });
        let right = tokio::spawn(async move {
            refresh_feed(&right_db, &right_fetcher, &right_feed, &right_settings, NOW).await
        });
        assert_eq!(
            left.await.expect("joined").expect("alice").new_entries,
            EACH
        );
        assert_eq!(right.await.expect("joined").expect("bob").new_entries, EACH);

        let rows: Vec<(i64, i64)> = db
            .with(|mut conn| async move {
                Ok(sqlx::query_as("SELECT id, id_user FROM entry ORDER BY id")
                    .fetch_all(&mut *conn)
                    .await?)
            })
            .await
            .expect("queried");
        assert_eq!(rows.len(), EACH * 2, "no article was dropped");
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let unique: std::collections::HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "no id was handed out twice");
        assert_eq!(
            rows.iter()
                .filter(|(_, user)| *user == alice.id_user)
                .count(),
            EACH,
            "and each user got exactly their own"
        );
        assert_eq!(
            rows.iter().filter(|(_, user)| *user == bob.id_user).count(),
            EACH
        );
    }
}
