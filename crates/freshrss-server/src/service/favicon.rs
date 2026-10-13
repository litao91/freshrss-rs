//! Fetching and caching a feed's icon.
//!
//! The sidebar shows an icon beside every feed and re-renders often, so the bytes are cached in the
//! `favicon` table and served from there until they go stale rather than re-downloaded per page load.
//! A feed publishes its icon address in the `feedIconUrl` attribute, discovered during a refresh; when
//! it has not (a feed that declares no icon, or one that has never been fetched) the conventional
//! `<scheme>://<host>/favicon.ico` is tried instead, which is what a browser would do.
//!
//! Fetching goes through the same [`Fetcher`] and obeys the same [`Settings`] as a feed refresh, so an
//! icon request cannot be talked into reaching a host the operator's network policy excludes.

use freshrss_feed::http::{FetchOutcome, FetchRequest, Fetcher};
use url::Url;

use crate::db::dao::favicon as favicon_dao;
use crate::db::Db;
use crate::domain::feed::Feed;
use crate::error::Result;
use crate::service::refresh::Settings;

/// How long a cached icon stays fresh. Icons change rarely, so a week is a good trade between a
/// stale logo and hammering the origin on every render.
const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// A cap on how much of an icon is read. Far below the feed-body budget: an icon that is larger than
/// this is not an icon, and caching it would waste space on a mistake or an abuse.
const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;

/// A feed's icon: the bytes and the media type to serve them with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub bytes: Vec<u8>,
    /// A concrete image type (`image/png`, `image/svg+xml`, `image/x-icon`, …) taken from the origin's
    /// `Content-Type` where it sent one, so the endpoint does not have to sniff the bytes.
    pub content_type: String,
}

/// Returns a feed's icon, from the cache when it is fresh and by fetching it otherwise.
///
/// `Ok(None)` means "this feed has no icon to show" — it publishes no address and has no derivable
/// host, or the fetch came back empty or failing — which the endpoint turns into a 404 rather than an
/// error, because a missing favicon is a normal state, not a fault.
///
/// # Errors
///
/// Only when the database cannot be read or written; a failed fetch is `Ok(None)`.
pub async fn icon(
    db: &Db,
    fetcher: &(dyn Fetcher + '_),
    feed: &Feed,
    settings: &Settings,
    now: i64,
) -> Result<Option<Icon>> {
    let Some(url) = icon_url(feed) else {
        return Ok(None);
    };

    if let Some(cached) = fresh(db, feed.id, &url, now).await? {
        return Ok(Some(cached));
    }

    let request = FetchRequest {
        url: url.clone(),
        accept: "image/*,*/*;q=0.8".to_owned(),
        user_agent: settings.user_agent.clone(),
        timeout: settings.timeout,
        max_bytes: MAX_ICON_BYTES.min(settings.max_bytes),
        basic_auth: None,
        ssl_verify: feed.ssl_verify(),
        proxy: None,
        conditional: Default::default(),
    };

    match fetcher.fetch(&request).await {
        Ok(FetchOutcome::Found(body))
            if (200..300).contains(&body.status) && !body.body.is_empty() =>
        {
            let content_type = body
                .content_type
                .filter(|ct| !ct.trim().is_empty())
                .unwrap_or_else(|| guess_content_type(&url));
            let icon = Icon {
                bytes: body.body,
                content_type,
            };
            store(db, feed.id, &url, &icon, now).await?;
            Ok(Some(icon))
        }
        // Not Modified cannot happen (we send no validators), and any other outcome — a 404, a
        // timeout, an empty body — is simply "no icon".
        _ => Ok(None),
    }
}

/// The address to fetch a feed's icon from: its published `feedIconUrl`, else the conventional
/// `favicon.ico` on its website (falling back to the feed's own host).
fn icon_url(feed: &Feed) -> Option<String> {
    if let Some(url) = feed
        .feed_icon_url()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        return Some(url.to_owned());
    }
    default_icon_url(feed)
}

/// Derives `<scheme>://<host>/favicon.ico` from the website, or the feed address when the website is
/// absent or unparseable.
fn default_icon_url(feed: &Feed) -> Option<String> {
    for base in [feed.website.as_str(), feed.url.as_str()] {
        let base = base.trim();
        if base.is_empty() {
            continue;
        }
        if let Ok(parsed) = Url::parse(base) {
            if let Some(host) = parsed.host_str() {
                let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
                return Some(format!(
                    "{}://{}{}/favicon.ico",
                    parsed.scheme(),
                    host,
                    port
                ));
            }
        }
    }
    None
}

/// A cached icon for this exact address, when it is still within the TTL.
async fn fresh(db: &Db, id_feed: i64, url: &str, now: i64) -> Result<Option<Icon>> {
    let cached = db
        .with(move |mut conn| async move { favicon_dao::find(&mut conn, id_feed).await })
        .await?;
    Ok(cached
        .filter(|row| {
            row.url == url
                && !row.body.is_empty()
                && now.saturating_sub(row.fetched_at) < CACHE_TTL_SECS
        })
        .map(|row| Icon {
            bytes: row.body,
            content_type: row.content_type,
        }))
}

async fn store(db: &Db, id_feed: i64, url: &str, icon: &Icon, now: i64) -> Result<()> {
    let url = url.to_owned();
    let content_type = icon.content_type.clone();
    let bytes = icon.bytes.clone();
    db.with(move |mut conn| async move {
        favicon_dao::upsert(&mut conn, id_feed, &url, &content_type, &bytes, now).await
    })
    .await
}

/// The media type to serve when the origin sent none, guessed from the address's extension.
fn guess_content_type(url: &str) -> String {
    let path = Url::parse(url)
        .map(|parsed| parsed.path().to_owned())
        .unwrap_or_else(|_| url.to_owned());
    // `mime_guess` spells the ICO type `image/vnd.microsoft.icon`; `image/x-icon` is the spelling the
    // rest of the ecosystem (and the reader's own CSS) expects, so it is special-cased.
    if path.ends_with(".ico") {
        return "image/x-icon".to_owned();
    }
    mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string()
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::reqwest::FixtureError;
    use freshrss_feed::http::{FixtureFetcher, FixtureResponse};

    use crate::db::dao::{feed as feed_dao, user as user_dao};
    use crate::db::support::db;
    use crate::domain::attributes::keys;

    use super::*;

    const NOW: i64 = 1_791_000_000;
    const FEED_URL: &str = "https://e.example/feed.xml";

    fn png() -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a],
            etag: None,
            last_modified: None,
            content_type: Some("image/png".to_owned()),
        }
    }

    /// A feed row, with `feedIconUrl` set to `icon_url` when one is given.
    async fn setup(db: &Db, icon_url: Option<&str>) -> Feed {
        let icon_url = icon_url.map(str::to_owned);
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
            let mut feed = Feed::new(id_user, FEED_URL, "Example");
            feed.website = "https://e.example/".into();
            feed.category = user_dao::default_category_id(&mut conn, id_user).await?;
            if let Some(url) = icon_url {
                feed.attributes.set(keys::FEED_ICON_URL, url);
            }
            feed.id = feed_dao::insert(&mut conn, &feed).await?;
            Ok(feed)
        })
        .await
        .expect("seeded")
    }

    #[tokio::test]
    async fn a_published_icon_is_fetched_and_its_type_kept() {
        let store = db().await;
        let feed = setup(&store, Some("https://e.example/icon.png")).await;
        let fetcher = FixtureFetcher::new().with("https://e.example/icon.png", png());

        let icon = icon(&store, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("icon")
            .expect("some");
        assert_eq!(icon.content_type, "image/png");
        assert_eq!(icon.bytes, vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a]);
        assert_eq!(fetcher.requests().len(), 1);
        assert_eq!(fetcher.requests()[0].url, "https://e.example/icon.png");
    }

    #[tokio::test]
    async fn a_fresh_cache_avoids_a_second_fetch() {
        let store = db().await;
        let feed = setup(&store, Some("https://e.example/icon.png")).await;
        let fetcher = FixtureFetcher::new().with("https://e.example/icon.png", png());

        let first = icon(&store, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");
        let second = icon(&store, &fetcher, &feed, &Settings::default(), NOW + 60)
            .await
            .expect("second");
        assert_eq!(first, second, "the cached copy is identical");
        assert_eq!(
            fetcher.requests().len(),
            1,
            "the second call was served from the cache"
        );
    }

    #[tokio::test]
    async fn a_stale_cache_is_refetched() {
        let store = db().await;
        let feed = setup(&store, Some("https://e.example/icon.png")).await;
        let fetcher = FixtureFetcher::new().with("https://e.example/icon.png", png());

        icon(&store, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("first");
        icon(
            &store,
            &fetcher,
            &feed,
            &Settings::default(),
            NOW + CACHE_TTL_SECS + 1,
        )
        .await
        .expect("second");
        assert_eq!(fetcher.requests().len(), 2, "past the TTL it fetches again");
    }

    #[tokio::test]
    async fn a_feed_with_no_published_icon_derives_favicon_ico() {
        let store = db().await;
        let feed = setup(&store, None).await;
        let fetcher = FixtureFetcher::new().with("https://e.example/favicon.ico", png());

        let icon = icon(&store, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("icon")
            .expect("some");
        assert!(!icon.bytes.is_empty());
        assert_eq!(fetcher.requests()[0].url, "https://e.example/favicon.ico");
    }

    #[tokio::test]
    async fn a_missing_icon_is_none_not_an_error() {
        let store = db().await;
        let feed = setup(&store, Some("https://e.example/gone.png")).await;
        let fetcher = FixtureFetcher::new().with(
            "https://e.example/gone.png",
            FixtureResponse::Error(FixtureError::Http(404)),
        );

        let icon = icon(&store, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("no error on a 404");
        assert!(icon.is_none());
    }

    #[tokio::test]
    async fn an_origin_without_a_content_type_is_guessed_from_the_address() {
        let store = db().await;
        let feed = setup(&store, Some("https://e.example/logo.svg")).await;
        let fetcher = FixtureFetcher::new().with(
            "https://e.example/logo.svg",
            FixtureResponse::Body {
                status: 200,
                body: b"<svg/>".to_vec(),
                etag: None,
                last_modified: None,
                content_type: None,
            },
        );
        let icon = icon(&store, &fetcher, &feed, &Settings::default(), NOW)
            .await
            .expect("icon")
            .expect("some");
        assert_eq!(icon.content_type, "image/svg+xml");
    }

    #[test]
    fn the_ico_type_is_spelled_the_way_the_ecosystem_expects() {
        assert_eq!(
            guess_content_type("https://e.example/favicon.ico"),
            "image/x-icon"
        );
        assert_eq!(guess_content_type("https://e.example/a.png"), "image/png");
    }

    #[test]
    fn the_derived_url_keeps_the_scheme_host_and_port() {
        let mut feed = Feed::new(1, "http://e.example:8080/feed.xml", "E");
        feed.website = "".into();
        assert_eq!(
            default_icon_url(&feed).as_deref(),
            Some("http://e.example:8080/favicon.ico")
        );
        // The website wins over the feed address when it is present and absolute.
        feed.website = "https://blog.example/about".into();
        assert_eq!(
            default_icon_url(&feed).as_deref(),
            Some("https://blog.example/favicon.ico")
        );
    }
}
