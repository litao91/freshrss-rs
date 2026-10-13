//! Fetching the article body a feed only links to.
//!
//! A truncated feed publishes a teaser and a link; `pathEntries` says which element of the linked
//! page is the article. This is the step that goes and gets it, and it is the one part of a refresh
//! that issues a request *per article*, which is why it is bounded by the same concurrency limit as
//! feeds and why it runs outside the writer lock — a slow article page must not pause every other
//! user's commit.
//!
//! Deliberately not ported: PHP wraps scraped content in `<!-- FULLCONTENT start //-->` comments.
//! They exist so a theme's CSS can tell scraped bodies apart, and our sanitizer is free to drop
//! comments, so carrying them would make the stored body depend on a sanitizer decision.

use std::sync::Arc;

use futures::StreamExt as _;
use tokio::sync::Semaphore;

use freshrss_feed::fulltext;
use freshrss_feed::http::{FetchOutcome, FetchRequest, Fetcher};
use freshrss_feed::{FeedError, SanitizeOptions};

use crate::domain::attributes::keys;
use crate::domain::entry::Entry;
use crate::domain::feed::Feed;
use crate::service::ingest::Sanitize;
use crate::service::refresh::Settings;
use crate::service::search::EntryView;

/// How many `<meta http-equiv="refresh">` hops to follow. PHP's `$maxRedirs`.
const MAX_HOPS: usize = 4;

/// Scrapes the article pages behind `entries` and rewrites their content in place.
///
/// Entries of a feed with neither `pathEntries` nor `path_entries_filter` come back untouched
/// without a single request, which is every feed that does not truncate.
pub async fn complete_all(
    fetcher: &(dyn Fetcher + '_),
    feed: &Feed,
    entries: &mut [Entry],
    settings: &Settings,
    sanitize: &Sanitize,
) {
    let selector = nonblank(feed.path_entries.as_deref());
    let filter = nonblank(feed.path_entries_filter.as_deref());
    if selector.is_none() && filter.is_none() {
        return;
    }
    let conditions = compile_conditions(feed);

    // Bounded the same way feeds are: `pathEntries` turns one refresh into one request per article,
    // and a feed that publishes fifty teasers must not open fifty sockets at once.
    let permits = Arc::new(Semaphore::new(settings.concurrency.max(1)));
    futures::stream::iter(entries.iter_mut())
        .for_each_concurrent(settings.concurrency.max(1), |entry| {
            let permits = Arc::clone(&permits);
            // Borrowed here so the `async move` block below captures a reference rather than
            // trying to take the vector, which an `FnMut` closure cannot give it.
            let conditions = &conditions;
            async move {
                let _permit = permits.acquire().await.ok();
                complete_one(
                    fetcher, feed, entry, settings, sanitize, selector, filter, conditions,
                )
                .await;
            }
        })
        .await;
}

/// The `path_entries_conditions` searches, compiled once for the batch.
///
/// A condition that does not parse is dropped rather than fatal: it is configuration, and one bad
/// string must not stop the other conditions — or the articles — from being honoured.
fn compile_conditions(feed: &Feed) -> Vec<freshrss_search::ast::Expr> {
    let conditions = feed
        .attributes
        .get_typed::<Vec<String>>(keys::PATH_ENTRIES_CONDITIONS)
        .unwrap_or_default();
    conditions
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .filter_map(|text| match freshrss_search::parser::parse(text, 0) {
            Ok(expr) => Some(expr),
            Err(err) => {
                tracing::warn!(
                    feed = feed.id,
                    condition = text,
                    error = %err,
                    "ignoring an unparseable path_entries_conditions entry"
                );
                None
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn complete_one(
    fetcher: &(dyn Fetcher + '_),
    feed: &Feed,
    entry: &mut Entry,
    settings: &Settings,
    sanitize: &Sanitize,
    selector: Option<&str>,
    filter: Option<&str>,
    conditions: &[freshrss_search::ast::Expr],
) {
    // With conditions configured and none matching, the article keeps what the feed gave it.
    if !conditions.is_empty() {
        let target = EntryView::borrowed(entry, feed.category);
        let matched = conditions
            .iter()
            .any(|expr| freshrss_search::matcher::matches(expr, &target).unwrap_or(false));
        if !matched {
            return;
        }
    }

    let Some(selector) = selector else {
        // A filter with no selector cleans the teaser the feed published instead of fetching
        // anything. `strip` answers `None` for "nothing matched", which is the difference between
        // leaving the body alone and storing an emptied one.
        if let Some(filter) = filter {
            if let Ok(Some(stripped)) = fulltext::strip(&entry.content, filter) {
                entry.content = stripped;
            }
        }
        return;
    };

    let page = match fetch_page(fetcher, &entry.link, settings).await {
        Ok(page) => page,
        Err(err) => {
            tracing::debug!(url = %entry.link, error = %err, "could not fetch the article page");
            return;
        }
    };
    let options = SanitizeOptions {
        base_url: fulltext::document_base(&page.text, &page.url),
        force_https_domains: sanitize.force_https_domains.clone(),
        max_length: sanitize.max_length,
    };
    let extracted = match fulltext::extract(
        &page.text,
        std::slice::from_ref(&selector),
        filter.unwrap_or(""),
        &options,
    ) {
        Ok(extracted) => extracted,
        Err(err) => {
            tracing::warn!(
                feed = feed.id,
                url = %entry.link,
                error = %err,
                "full-text extraction failed; keeping the feed's own content"
            );
            return;
        }
    };
    if extracted.html.is_empty() {
        // Zero matches means the selector has gone stale against this page's markup; storing an
        // empty body would replace a readable teaser with nothing.
        if extracted.matched == 0 {
            tracing::warn!(
                feed = feed.id,
                url = %entry.link,
                selector,
                "pathEntries matched nothing on the article page"
            );
        }
        return;
    }

    apply_content_action(entry, &extracted.html, feed);
}

/// A fetched article page, at the URL it ended up at.
struct Page {
    text: String,
    url: String,
}

/// Fetches an article page, following `<meta http-equiv="refresh">` hops the way a browser would.
///
/// `None` on any failure: the teaser the feed published is a perfectly good fallback, and an
/// article that cannot be scraped must not fail the refresh it arrived in.
async fn fetch_page(
    fetcher: &(dyn Fetcher + '_),
    link: &str,
    settings: &Settings,
) -> std::result::Result<Page, FeedError> {
    let mut url = link.to_owned();
    for _ in 0..MAX_HOPS {
        let mut request = FetchRequest::new(url.clone());
        request.accept = "text/html,application/xhtml+xml".to_owned();
        request.user_agent = settings.user_agent.clone();
        request.timeout = settings.timeout;
        request.max_bytes = settings.max_bytes;

        let body = match fetcher.fetch(&request).await {
            Ok(FetchOutcome::Found(body)) => body,
            Ok(FetchOutcome::NotModified) => {
                return Err(FeedError::Http {
                    status: 304,
                    message: "the article page answered 304 to an unconditional request".to_owned(),
                })
            }
            Err(err) => return Err(err),
        };
        if !(200..300).contains(&body.status) {
            return Err(FeedError::Http {
                status: body.status,
                message: format!("the article page answered {}", body.status),
            });
        }
        let effective = if body.effective_url.is_empty() {
            url.clone()
        } else {
            body.effective_url.clone()
        };
        let text = String::from_utf8_lossy(&body.body).into_owned();

        match fulltext::meta_refresh(&text, &effective) {
            Some(next) if next != effective => url = next,
            _ => {
                return Ok(Page {
                    text,
                    url: effective,
                })
            }
        }
    }
    Err(FeedError::Http {
        status: 0,
        message: format!("more than {MAX_HOPS} meta-refresh hops at {link}"),
    })
}

/// The selectors tried, in order, when a feed configures no `pathEntries` of its own.
///
/// A reader asking for the full article of a feed that was never configured for scraping still
/// deserves an answer, and these are the elements publishers conventionally put the body in. The
/// first that matches wins; a page with none of them is reported as "nothing matched" rather than
/// guessed at any further.
pub const DEFAULT_SELECTORS: &[&str] = &[
    "article",
    "main",
    "[role=\"main\"]",
    ".entry-content",
    ".post-content",
    ".article-content",
    "#content",
    ".content",
    ".post",
    ".story-body",
];

/// What an on-demand extraction produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnDemand {
    pub html: String,
    pub matched: usize,
    /// The page the body came from, after redirects and meta-refresh hops.
    pub url: String,
    /// The selector that matched, so the caller can say what was used.
    pub selector: String,
}

/// Fetches one article's page and extracts its body, for a reader asking now.
///
/// `selector` and `filter` override the feed's own when given, which is also how a settings screen
/// previews a candidate selector against a real article. Unlike the refresh path, an explicit
/// request is not gated on `path_entries_conditions`: a human asking for one article is the
/// condition.
///
/// Returns `Ok(None)` when no selector matched anything, so the caller can tell "the page has no
/// recognisable article element" from "the page could not be fetched", which is an error.
pub async fn on_demand(
    fetcher: &(dyn Fetcher + '_),
    feed: &Feed,
    entry: &Entry,
    settings: &Settings,
    sanitize: &Sanitize,
    selector: Option<&str>,
    filter: Option<&str>,
) -> crate::error::Result<Option<OnDemand>> {
    let page = fetch_page(fetcher, &entry.link, settings).await?;
    let filter = filter
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| nonblank(feed.path_entries_filter.as_deref()));
    let configured = selector
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| nonblank(feed.path_entries.as_deref()));
    let candidates: Vec<&str> = match configured {
        Some(one) => vec![one],
        None => DEFAULT_SELECTORS.to_vec(),
    };

    let options = SanitizeOptions {
        base_url: fulltext::document_base(&page.text, &page.url),
        force_https_domains: sanitize.force_https_domains.clone(),
        max_length: sanitize.max_length,
    };
    let extracted = match fulltext::extract(&page.text, &candidates, filter.unwrap_or(""), &options)
    {
        Ok(extracted) => extracted,
        // A selector the caller typed that is not valid CSS is their typo, not a fault.
        Err(err @ FeedError::BadConfig(_)) if selector.is_some() => {
            return Err(crate::error::validation(err.to_string()))
        }
        Err(err) => return Err(err.into()),
    };
    if extracted.matched == 0 || extracted.html.is_empty() {
        return Ok(None);
    }
    Ok(Some(OnDemand {
        html: extracted.html,
        matched: extracted.matched,
        url: page.url,
        selector: extracted.selector,
    }))
}

/// Applies the scraped body according to the feed's `content_action`.
///
/// `replace` keeps the teaser in `original_content` so the feed's own text is recoverable; `prepend`
/// and `append` combine the two and drop the attribute, which is PHP's behaviour and the reason a
/// feed that switches actions does not accumulate stale copies.
fn apply_content_action(entry: &mut Entry, scraped: &str, feed: &Feed) {
    let original = entry
        .attributes
        .get_str(keys::ORIGINAL_CONTENT)
        .map(str::to_owned)
        .unwrap_or_else(|| entry.content.clone());
    match feed.attributes.get_str(keys::CONTENT_ACTION) {
        Some("prepend") => {
            entry.attributes.remove(keys::ORIGINAL_CONTENT);
            entry.content = format!("{scraped}{original}");
        }
        Some("append") => {
            entry.attributes.remove(keys::ORIGINAL_CONTENT);
            entry.content = format!("{original}{scraped}");
        }
        _ => {
            entry
                .attributes
                .set_str(keys::ORIGINAL_CONTENT, Some(&original));
            entry.content = scraped.to_owned();
        }
    }
}

fn nonblank(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::{FixtureFetcher, FixtureResponse};

    use crate::domain::ids::EntryId;

    use super::*;

    const NOW: i64 = 1_791_000_000;

    fn html(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("text/html".to_owned()),
        }
    }

    const PAGE: &str = r#"<html><head><title>Page</title></head>
<body><nav>Menus and ads</nav><article><p>The whole article, in full.</p></article></body></html>"#;

    fn feed_with(selector: &str, filter: &str) -> Feed {
        let mut feed = Feed::new(1, "https://e.example/f", "Example");
        feed.id = 7;
        feed.path_entries = nonblank(Some(selector)).map(str::to_owned);
        feed.path_entries_filter = nonblank(Some(filter)).map(str::to_owned);
        feed
    }

    fn entry(link: &str, content: &str) -> Entry {
        let mut entry = Entry::new(1, 7, "g1");
        entry.id = EntryId::from_micros(NOW * 1_000_000);
        entry.link = link.to_owned();
        entry.content = content.to_owned();
        entry
    }

    async fn run(feed: &Feed, entry: &mut Entry, fetcher: &FixtureFetcher) {
        complete_all(
            fetcher,
            feed,
            std::slice::from_mut(entry),
            &Settings::default(),
            &Sanitize::default(),
        )
        .await;
    }

    #[tokio::test]
    async fn a_feed_with_no_selector_fetches_nothing() {
        let feed = feed_with("", "");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let mut entry = entry("https://e.example/1", "Teaser");
        run(&feed, &mut entry, &fetcher).await;
        assert_eq!(entry.content, "Teaser");
        assert!(fetcher.requests().is_empty(), "no page was fetched");
    }

    #[tokio::test]
    async fn path_entries_replaces_the_teaser_with_the_article_body() {
        let feed = feed_with("article", "");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let mut entry = entry("https://e.example/1", "Teaser");
        run(&feed, &mut entry, &fetcher).await;
        assert!(
            entry.content.contains("The whole article, in full."),
            "{}",
            entry.content
        );
        assert!(!entry.content.contains("Menus and ads"));
        assert_eq!(
            entry.attributes.get_str(keys::ORIGINAL_CONTENT),
            Some("Teaser"),
            "the feed's own text stays recoverable"
        );
    }

    #[tokio::test]
    async fn path_entries_filter_strips_the_boilerplate() {
        let feed = feed_with("body", "nav");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let mut entry = entry("https://e.example/1", "Teaser");
        run(&feed, &mut entry, &fetcher).await;
        assert!(entry.content.contains("The whole article, in full."));
        assert!(
            !entry.content.contains("Menus and ads"),
            "{}",
            entry.content
        );
    }

    #[tokio::test]
    async fn a_filter_without_a_selector_cleans_the_teaser_in_place() {
        let mut feed = feed_with("", "a.share");
        feed.path_entries = None;
        let mut entry = entry(
            "https://e.example/1",
            "<p>Teaser</p><a class=\"share\">Share!</a>",
        );
        run(&feed, &mut entry, &FixtureFetcher::new()).await;
        assert_eq!(entry.content, "<p>Teaser</p>");
    }

    #[tokio::test]
    async fn a_selector_that_matches_nothing_keeps_the_teaser() {
        let feed = feed_with(".does-not-exist", "");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let mut entry = entry("https://e.example/1", "Teaser");
        run(&feed, &mut entry, &fetcher).await;
        assert_eq!(
            entry.content, "Teaser",
            "an empty body must not replace a readable one"
        );
    }

    #[tokio::test]
    async fn an_unfetchable_page_keeps_the_teaser_and_the_refresh_alive() {
        let feed = feed_with("article", "");
        let fetcher = FixtureFetcher::new();
        let mut entry = entry("https://e.example/1", "Teaser");
        run(&feed, &mut entry, &fetcher).await;
        assert_eq!(entry.content, "Teaser");
    }

    #[tokio::test]
    async fn content_action_append_keeps_the_teaser_first() {
        let mut feed = feed_with("article", "");
        feed.attributes
            .set_str(keys::CONTENT_ACTION, Some("append"));
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let mut entry = entry("https://e.example/1", "Teaser ");
        run(&feed, &mut entry, &fetcher).await;
        assert!(entry.content.starts_with("Teaser "), "{}", entry.content);
        assert!(entry.content.contains("The whole article, in full."));
        assert_eq!(entry.attributes.get_str(keys::ORIGINAL_CONTENT), None);
    }

    #[tokio::test]
    async fn conditions_gate_which_articles_are_scraped() {
        let mut feed = feed_with("article", "");
        feed.attributes
            .set_typed(
                keys::PATH_ENTRIES_CONDITIONS,
                &["intitle:Wanted".to_owned()],
            )
            .expect("stores");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));

        let mut wanted = {
            let mut e = entry("https://e.example/1", "Teaser");
            e.title = "Wanted".to_owned();
            e
        };
        run(&feed, &mut wanted, &fetcher).await;
        assert!(wanted.content.contains("The whole article, in full."));

        let mut other = {
            let mut e = entry("https://e.example/1", "Teaser");
            // `intitle:` is a containment test, so the negative case must not merely negate the
            // positive one's words.
            e.title = "Something else entirely".to_owned();
            e
        };
        run(&feed, &mut other, &fetcher).await;
        assert_eq!(other.content, "Teaser");
    }

    #[tokio::test]
    async fn a_meta_refresh_hop_is_followed() {
        let hop = r#"<html><head><meta http-equiv="refresh" content="0; url=/real.html"></head><body></body></html>"#;
        let feed = feed_with("article", "");
        let fetcher = FixtureFetcher::new()
            .with("https://e.example/1", html(hop))
            .with("https://e.example/real.html", html(PAGE));
        let mut entry = entry("https://e.example/1", "Teaser");
        run(&feed, &mut entry, &fetcher).await;
        assert!(entry.content.contains("The whole article, in full."));
        assert_eq!(fetcher.requests().len(), 2);
    }
    // --- on-demand extraction -------------------------------------------------

    #[tokio::test]
    async fn on_demand_falls_back_to_the_default_selectors() {
        let feed = feed_with("", "");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let entry = entry("https://e.example/1", "Teaser");
        let got = on_demand(
            &fetcher,
            &feed,
            &entry,
            &Settings::default(),
            &Sanitize::default(),
            None,
            None,
        )
        .await
        .expect("fetched")
        .expect("a match");
        assert!(
            got.html.contains("The whole article, in full."),
            "{}",
            got.html
        );
        assert_eq!(
            got.selector, "article",
            "the first default that matches wins"
        );
        assert_eq!(got.url, "https://e.example/1");
    }

    #[tokio::test]
    async fn on_demand_honours_an_explicit_selector_over_the_feed_s() {
        let feed = feed_with("nav", "");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let entry = entry("https://e.example/1", "Teaser");
        let got = on_demand(
            &fetcher,
            &feed,
            &entry,
            &Settings::default(),
            &Sanitize::default(),
            Some("article"),
            None,
        )
        .await
        .expect("fetched")
        .expect("a match");
        assert_eq!(
            got.selector, "article",
            "the caller's selector outranks the feed's"
        );
        assert!(!got.html.contains("Menus and ads"));
    }

    #[tokio::test]
    async fn on_demand_reports_a_page_with_no_recognisable_article() {
        let feed = feed_with("", "");
        let fetcher = FixtureFetcher::new().with(
            "https://e.example/1",
            html("<html><body><div>nothing semantic here</div></body></html>"),
        );
        let entry = entry("https://e.example/1", "Teaser");
        let got = on_demand(
            &fetcher,
            &feed,
            &entry,
            &Settings::default(),
            &Sanitize::default(),
            None,
            None,
        )
        .await
        .expect("fetched");
        assert_eq!(
            got, None,
            "no selector matched, which the caller reports as such"
        );
    }

    #[tokio::test]
    async fn on_demand_reports_a_page_that_cannot_be_fetched() {
        let feed = feed_with("article", "");
        let fetcher = FixtureFetcher::new().with(
            "https://e.example/1",
            FixtureResponse::Error(freshrss_feed::http::reqwest::FixtureError::Http(503)),
        );
        let entry = entry("https://e.example/1", "Teaser");
        let err = on_demand(
            &fetcher,
            &feed,
            &entry,
            &Settings::default(),
            &Sanitize::default(),
            None,
            None,
        )
        .await
        .expect_err("the page is unavailable");
        assert!(err.to_string().contains("503"), "{err}");
    }

    #[tokio::test]
    async fn an_invalid_explicit_selector_is_a_validation_error() {
        let feed = feed_with("", "");
        let fetcher = FixtureFetcher::new().with("https://e.example/1", html(PAGE));
        let entry = entry("https://e.example/1", "Teaser");
        let err = on_demand(
            &fetcher,
            &feed,
            &entry,
            &Settings::default(),
            &Sanitize::default(),
            Some("article:::not-css"),
            None,
        )
        .await
        .expect_err("not a selector");
        assert!(
            matches!(err, crate::error::Error::Validation(_)),
            "a typed selector is the caller's typo: {err:?}"
        );
    }
}
