//! Finding the feed behind a URL that is not one.
//!
//! People paste a website's address into the add-feed box far more often than they paste a feed URL,
//! because that is the address they can see. This is the chain that turns one into the other, and it
//! is SimplePie's `Locator` — the one PHP FreshRSS uses — so the same addresses work in both:
//!
//! 1. the URL is fetched and, if it already parses as a feed, that is the answer;
//! 2. otherwise the body is read as HTML and the `<link>`, `<a>` and `<area>` elements carrying
//!    `rel="alternate"` with a feed MIME type, or any `rel` naming `feed`, are collected;
//! 3. failing that, every `<a href>` on the page is tried in four passes — same-host URLs whose path
//!    ends in `.rss`/`.rdf`/`.atom`/`.xml`, then same-host URLs merely *mentioning* a feed word,
//!    then the same two passes over other hosts;
//! 4. each candidate is fetched and has to parse as a feed to be accepted, and at most
//!    [`MAX_CHECKED`] of them are fetched at all.
//!
//! Verification is by parsing rather than by `Content-Type`, which is the one place this differs from
//! SimplePie, and deliberately: plenty of servers answer a feed request with `text/html`, and the
//! discovery dialog wants to show the title and article count it found, which only a parse supplies.

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::node::Element;
use scraper::{Html, Selector};
use serde::Serialize;
use url::Url;

use crate::error::FeedError;
use crate::http::{FetchOutcome, FetchRequest, FetchedBody, Fetcher};
use crate::kind::FeedKind;
use crate::raw::{FeedFormat, RawFeed};
use crate::sanitize::uri::absolutize;

/// How many candidate URLs are fetched before discovery gives up.
///
/// SimplePie's `max_checked_feeds`. The candidates come from a document we do not control, so this
/// is what stops a page full of links from turning one add-feed request into a hundred fetches.
pub const MAX_CHECKED: usize = 10;

/// Path endings tried before the looser name match.
const FEED_EXTENSIONS: [&str; 4] = [".rss", ".rdf", ".atom", ".xml"];

/// Words that make a URL worth fetching even without a feed extension.
///
/// Deliberately loose — this is the last resort, and `/index.php?feed=rss2`,
/// `https://blog.example/atom.xml` and `https://example.com/rss` are all real.
const FEED_WORDS: [&str; 5] = ["feed", "rss", "rdf", "atom", "xml"];

/// The MIME types that make a `rel="alternate"` link a feed candidate.
///
/// `text/html` is not among them, which is another difference from SimplePie. It accepts that type
/// because it then runs discovery *again* on the page it fetched; this does not recurse, so the only
/// effect of accepting it would be spending a fetch on the canonical-homepage `<link>` that nearly
/// every page carries, to conclude that the homepage is not a feed.
const FEED_MIME_TYPES: [&str; 4] = [
    "application/rss+xml",
    "application/atom+xml",
    "application/rdf+xml",
    "application/feed+json",
];

static PAGE_ELEMENTS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("base, link, a, area").expect("a constant selector list is valid")
});

/// Which pass produced a candidate.
///
/// Reported to the user because "we found it from a `<link rel=alternate>`" and "we guessed from a
/// URL that happened to contain the word rss" are different levels of confidence, and the second one
/// is worth a look before subscribing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The URL that was given is itself a feed.
    Given,
    /// `<link rel=…>`.
    Link,
    /// `<a rel=…>`.
    Anchor,
    /// `<area rel=…>`.
    Area,
    /// A same-host `<a href>` whose path ends in a feed extension.
    LocalExtension,
    /// A same-host `<a href>` whose URL mentions a feed word.
    LocalBody,
    /// A cross-host `<a href>` whose path ends in a feed extension.
    RemoteExtension,
    /// A cross-host `<a href>` whose URL mentions a feed word.
    RemoteBody,
}

/// What a candidate turned out to contain.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Probe {
    pub title: Option<String>,
    pub website: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub item_count: usize,
    pub format: FeedFormat,
}

impl Probe {
    fn of(feed: &RawFeed) -> Self {
        Probe {
            title: feed.title.clone(),
            website: feed.website.clone(),
            description: feed.description.clone(),
            icon: feed.icon.clone(),
            item_count: feed.items.len(),
            format: feed.format,
        }
    }
}

/// One URL that was considered.
///
/// Rejected candidates are kept rather than dropped: the add-feed dialog shows them, and "we tried
/// nine links on that page and none was a feed" is a far more useful answer than "not found".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub url: String,
    pub source: Source,
    /// Set when this URL is a feed.
    pub probe: Option<Probe>,
    /// Why it was not accepted, when it was not.
    pub rejected: Option<String>,
}

/// The result of looking at a URL.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Discovered {
    /// The URL that was fetched, after redirects. Everything on the page was resolved against this.
    pub url: String,
    /// Every URL that was considered, in the order it was tried.
    pub candidates: Vec<Candidate>,
}

impl Discovered {
    /// The feed to subscribe to, when one was found.
    pub fn found(&self) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.probe.is_some())
    }

    /// True when the URL that was given needed no discovery at all.
    pub fn is_feed(&self) -> bool {
        self.found().is_some_and(|c| c.source == Source::Given)
    }
}

/// Fetches `url` and looks for a feed at or behind it.
///
/// `template` supplies everything except the URL — user agent, timeout, TLS and proxy settings — so
/// discovery obeys the same limits as a refresh.
///
/// # Errors
///
/// Only when the URL that was given cannot be fetched at all. A page with no feed on it, or whose
/// candidates all fail, is a successful [`Discovered`] with no [`Discovered::found`]: that is an
/// answer, not an error, and the caller has to be able to report it.
pub async fn discover(
    fetcher: &(dyn Fetcher + '_),
    url: &str,
    template: &FetchRequest,
) -> Result<Discovered, FeedError> {
    let body = fetch(fetcher, template, url).await?;
    let document_url = if body.effective_url.is_empty() {
        url.to_owned()
    } else {
        body.effective_url.clone()
    };

    if let Some(feed) = probe(&body) {
        return Ok(Discovered {
            url: document_url.clone(),
            candidates: vec![Candidate {
                url: document_url,
                source: Source::Given,
                probe: Some(feed),
                rejected: None,
            }],
        });
    }

    let mut candidates = Vec::new();
    for (url, source) in plan(&body.body, &document_url)
        .into_iter()
        .take(MAX_CHECKED)
    {
        let mut candidate = Candidate {
            url,
            source,
            probe: None,
            rejected: None,
        };
        match fetch(fetcher, template, &candidate.url).await {
            Ok(fetched) => match probe(&fetched) {
                Some(feed) => {
                    candidate.probe = Some(feed);
                    candidates.push(candidate);
                    break;
                }
                None => candidate.rejected = Some("the response is not a feed".to_owned()),
            },
            Err(err) => candidate.rejected = Some(err.to_string()),
        }
        candidates.push(candidate);
    }

    Ok(Discovered {
        url: document_url,
        candidates,
    })
}

async fn fetch(
    fetcher: &(dyn Fetcher + '_),
    template: &FetchRequest,
    url: &str,
) -> Result<FetchedBody, FeedError> {
    let request = FetchRequest {
        url: url.to_owned(),
        ..template.clone()
    };
    match fetcher.fetch(&request).await? {
        FetchOutcome::Found(body) => Ok(body),
        // Unreachable without conditional headers on the template, but a 304 carries no body to look
        // at and pretending otherwise would report a stale fingerprint as a discovery.
        FetchOutcome::NotModified => Err(FeedError::Http {
            status: 304,
            message: "the server answered 304 to an unconditional request".to_owned(),
        }),
    }
}

/// Reads a fetched body as a feed.
fn probe(body: &FetchedBody) -> Option<Probe> {
    // A non-2xx body is not a feed even if it parses: some servers answer every path with the same
    // XML error document, and subscribing to that would refresh an error page forever.
    if !(200..300).contains(&body.status) {
        return None;
    }
    let feed = crate::parse::parse(&body.body, FeedKind::Rss).ok()?;
    Some(Probe::of(&feed))
}

/// The candidate URLs a page offers, in the order they should be tried.
///
/// Deduplicated across every pass, so a URL that appears both as `<link rel=alternate>` and as an
/// `<a>` in the sidebar is fetched once, in the earlier and more trustworthy pass.
fn plan(html: &[u8], document_url: &str) -> Vec<(String, Source)> {
    let document = Html::parse_document(&String::from_utf8_lossy(html));
    let mut page = Page::default();

    // One pass in document order, so `<base href>` is picked up at the point it appears and applies
    // to exactly the links after it — the rule HTML specifies and the one SimplePie reproduces by
    // comparing line numbers.
    for element in document.select(&PAGE_ELEMENTS) {
        page.visit(element.value(), document_url);
    }
    page.into_plan()
}

/// The elements of one page, bucketed by the pass that will try them.
#[derive(Default)]
struct Page {
    base: Option<String>,
    /// Tag-major, matching SimplePie: every `<link>`, then every `<a>`, then every `<area>`. A
    /// `<link rel=alternate>` in the head beats an `<a rel=feed>` above it in the body, because the
    /// head element is the one a publisher generates deliberately.
    links: Vec<String>,
    anchors: Vec<String>,
    areas: Vec<String>,
    local: Vec<String>,
    remote: Vec<String>,
}

impl Page {
    fn visit(&mut self, element: &Element, document_url: &str) {
        match element.name() {
            "base" => {
                if let Some(href) = trimmed(element, "href") {
                    let resolved = absolutize(href, document_url);
                    if fetchable(&resolved) {
                        self.base = Some(resolved);
                    }
                }
            }
            name @ ("link" | "a" | "area") => {
                let Some(href) = trimmed(element, "href") else {
                    return;
                };
                let url = absolutize(href, self.base.as_deref().unwrap_or(document_url));
                if !fetchable(&url) {
                    return;
                }
                if name == "a" {
                    if same_host(&url, document_url) {
                        self.local.push(url.clone());
                    } else {
                        self.remote.push(url.clone());
                    }
                }
                if autodiscovers(element) {
                    match name {
                        "link" => self.links.push(url),
                        "area" => self.areas.push(url),
                        _ => self.anchors.push(url),
                    }
                }
            }
            _ => {}
        }
    }

    fn into_plan(self) -> Vec<(String, Source)> {
        let mut plan = Vec::new();
        let mut seen = HashSet::new();
        let mut add = |url: String, source: Source| {
            if seen.insert(url.clone()) {
                plan.push((url, source));
            }
        };

        for url in self.links {
            add(url, Source::Link);
        }
        for url in self.anchors {
            add(url, Source::Anchor);
        }
        for url in self.areas {
            add(url, Source::Area);
        }
        // Extension before name-match, local before remote: each pass is less likely to be right
        // than the one before it, and the first candidate that parses wins.
        for url in self.local.iter().filter(|u| feed_extension(u)) {
            add(url.clone(), Source::LocalExtension);
        }
        for url in self.local.iter().filter(|u| mentions_feed(u)) {
            add(url.clone(), Source::LocalBody);
        }
        for url in self.remote.iter().filter(|u| feed_extension(u)) {
            add(url.clone(), Source::RemoteExtension);
        }
        for url in self.remote.iter().filter(|u| mentions_feed(u)) {
            add(url.clone(), Source::RemoteBody);
        }

        plan
    }
}

/// An attribute value with its surrounding whitespace removed, or `None` when blank or absent.
///
/// Trimming is what SimplePie does, and it matters: a hand-written `<link href=" /feed.xml ">` is
/// common on templated sites and an untrimmed value would fail to resolve.
fn trimmed<'a>(element: &'a Element, name: &str) -> Option<&'a str> {
    element.attr(name).map(str::trim).filter(|v| !v.is_empty())
}

/// SimplePie's accept rule for the `rel` scan.
fn autodiscovers(element: &Element) -> bool {
    let Some(rel) = element.attr("rel") else {
        return false;
    };
    let has = |wanted: &str| {
        rel.split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case(wanted))
    };

    if has("feed") {
        return true;
    }
    has("alternate")
        && !has("stylesheet")
        && element.attr("type").is_some_and(|t| {
            FEED_MIME_TYPES
                .iter()
                .any(|m| m.eq_ignore_ascii_case(mime(t)))
        })
}

/// A `type` attribute's MIME type without its parameters.
fn mime(value: &str) -> &str {
    value.split(';').next().unwrap_or(value).trim()
}

/// True when the URL can be fetched at all.
///
/// This is also the SSRF gate's first line: only `http` and `https` are considered, so a page cannot
/// point discovery at `file:`, `data:` or a `javascript:` URL. The fetcher itself refuses anything
/// that does not resolve to a remote address.
fn fetchable(url: &str) -> bool {
    Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some_and(|host| !host.is_empty())
    })
}

/// True when two URLs differ only in path.
fn same_host(url: &str, document_url: &str) -> bool {
    let (Ok(url), Ok(document)) = (Url::parse(url), Url::parse(document_url)) else {
        return false;
    };
    url.host_str().map(str::to_ascii_lowercase) == document.host_str().map(str::to_ascii_lowercase)
        && url.port_or_known_default() == document.port_or_known_default()
}

/// True when the path's last segment ends in a feed extension.
///
/// The query and fragment are excluded, which is a difference from SimplePie: it takes everything
/// after the last `.` in the *whole URL*, so `https://example.com/feed?v=1` misses (the suffix is
/// `.com/feed?v=1`) while `https://example.com/feed?v=1.rss` hits. Neither answer is right, and
/// looking at the path is the one that matches what the extension is for.
fn feed_extension(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(segment) = parsed.path().rsplit('/').next() else {
        return false;
    };
    let Some(dot) = segment.rfind('.') else {
        return false;
    };
    let extension = segment[dot..].to_ascii_lowercase();
    FEED_EXTENSIONS.contains(&extension.as_str())
}

/// The last-resort test: does the URL merely look like a feed?
fn mentions_feed(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    FEED_WORDS.iter().any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FixtureFetcher, FixtureResponse};

    const PAGE_URL: &str = "https://e.example/page";

    const FEED: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
	<channel>
		<title>Example &amp; Co</title>
		<link>https://e.example/</link>
		<description>An example</description>
		<image><url>https://e.example/icon.png</url><title>Example</title><link>https://e.example/</link></image>
		<item><title>One</title><link>https://e.example/1</link><guid>1</guid></item>
		<item><title>Two</title><link>https://e.example/2</link><guid>2</guid></item>
	</channel>
</rss>"#;

    fn html_response(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("text/html; charset=utf-8".to_owned()),
        }
    }

    fn feed_response() -> FixtureResponse {
        FixtureResponse::ok(FEED.as_bytes().to_vec())
    }

    /// A page whose `<head>` carries `head` and whose `<body>` carries `body`.
    fn page(head: &str, body: &str) -> String {
        format!(
            "<!doctype html><html><head><title>Site</title>{head}</head><body>{body}</body></html>"
        )
    }

    /// A fetcher serving `document` at [`PAGE_URL`].
    fn serving(document: &str) -> FixtureFetcher {
        FixtureFetcher::new().with(PAGE_URL, html_response(document))
    }

    async fn run(fetcher: &FixtureFetcher, document_url: &str) -> Discovered {
        discover(fetcher, document_url, &FetchRequest::new(String::new()))
            .await
            .expect("the page itself is always served")
    }

    fn urls(found: &Discovered) -> Vec<&str> {
        found.candidates.iter().map(|c| c.url.as_str()).collect()
    }

    #[tokio::test]
    async fn a_url_that_is_already_a_feed_needs_no_discovery() {
        let fetcher = FixtureFetcher::new().with(PAGE_URL, feed_response());
        let found = run(&fetcher, PAGE_URL).await;

        assert!(found.is_feed());
        let feed = found.found().expect("found");
        assert_eq!(feed.source, Source::Given);
        let probe = feed.probe.as_ref().expect("probed");
        assert_eq!(probe.title.as_deref(), Some("Example & Co"));
        assert_eq!(probe.website.as_deref(), Some("https://e.example/"));
        assert_eq!(probe.description.as_deref(), Some("An example"));
        assert_eq!(probe.icon.as_deref(), Some("https://e.example/icon.png"));
        assert_eq!(probe.item_count, 2);
        assert_eq!(probe.format, FeedFormat::Rss2);
        assert_eq!(fetcher.requests().len(), 1, "nothing else was fetched");
    }

    #[tokio::test]
    async fn a_link_rel_alternate_is_found() {
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="/feed.xml">"#,
            "",
        );
        let fetcher = serving(&document).with("https://e.example/feed.xml", feed_response());
        let found = run(&fetcher, PAGE_URL).await;

        let feed = found.found().expect("found");
        assert_eq!(
            feed.url, "https://e.example/feed.xml",
            "relative hrefs resolve"
        );
        assert_eq!(feed.source, Source::Link);
        assert!(!found.is_feed());
        assert_eq!(fetcher.requests().len(), 2);
    }

    #[tokio::test]
    async fn the_rel_scan_is_tag_major_not_document_major() {
        // Both are in the body, the anchor first. SimplePie scans every `<link>`, then every `<a>`,
        // then every `<area>`, because a `<link rel=alternate>` in a head is what a publisher
        // generates deliberately while an `<a rel=feed>` in a sidebar is often a template leftover.
        let document = page(
            "",
            r#"<a rel="feed" href="/anchor.xml">feed</a><link rel="alternate" type="application/atom+xml" href="/link.xml"><area rel="alternate" type="application/rss+xml" href="/area.xml">"#,
        );
        let fetcher = serving(&document)
            .with("https://e.example/link.xml", feed_response())
            .with("https://e.example/anchor.xml", feed_response());
        let found = run(&fetcher, PAGE_URL).await;

        let feed = found.found().expect("found");
        assert_eq!(feed.url, "https://e.example/link.xml");
        assert_eq!(feed.source, Source::Link);
        assert_eq!(urls(&found).len(), 1, "it stopped at the first feed");
    }

    #[tokio::test]
    async fn base_href_applies_to_the_links_after_it() {
        let document = page(
            r#"<base href="https://cdn.example/sub/"><link rel="alternate" type="application/rss+xml" href="feed.xml">"#,
            "",
        );
        let fetcher = serving(&document).with("https://cdn.example/sub/feed.xml", feed_response());
        let found = run(&fetcher, PAGE_URL).await;
        assert_eq!(
            found.found().expect("found").url,
            "https://cdn.example/sub/feed.xml"
        );
    }

    #[tokio::test]
    async fn a_link_before_base_href_resolves_against_the_document() {
        // The HTML rule `<base>` exists to express: it changes the base for what follows it, not for
        // the whole document. SimplePie reproduces this by comparing line numbers.
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="feed.xml"><base href="https://cdn.example/sub/">"#,
            "",
        );
        let fetcher = serving(&document).with("https://e.example/feed.xml", feed_response());
        let found = run(&fetcher, PAGE_URL).await;
        assert_eq!(
            found.found().expect("found").url,
            "https://e.example/feed.xml"
        );
    }

    #[tokio::test]
    async fn a_stylesheet_and_a_text_html_alternate_are_not_candidates() {
        let document = page(
            r#"<link rel="stylesheet alternate" type="application/rss+xml" href="/s.css">
               <link rel="alternate" type="text/html" href="/">
               <link rel="alternate" href="/no-type.xml">"#,
            "",
        );
        let found = run(&serving(&document), PAGE_URL).await;
        // `/s.css` and `/` are `<link>`s, so they are not picked up by the `<a>` passes either.
        assert!(found.candidates.is_empty(), "{found:?}");
        assert!(found.found().is_none());
    }

    #[tokio::test]
    async fn the_extension_pass_runs_when_no_rel_link_exists() {
        let document = page(
            "",
            r#"<a href="/news">News</a><a href="/news/feed.rss">RSS</a><a href="/blog/feed">Blog feed</a>"#,
        );
        let fetcher = serving(&document).with("https://e.example/news/feed.rss", feed_response());
        let found = run(&fetcher, PAGE_URL).await;

        let feed = found.found().expect("found");
        assert_eq!(feed.url, "https://e.example/news/feed.rss");
        assert_eq!(feed.source, Source::LocalExtension);
        assert_eq!(
            urls(&found),
            vec!["https://e.example/news/feed.rss"],
            "the first extension match is fetched and wins"
        );
    }

    #[tokio::test]
    async fn the_name_pass_is_the_last_resort() {
        let document = page("", r#"<a href="/index.php?feed=rss2">Subscribe</a>"#);
        let fetcher =
            serving(&document).with("https://e.example/index.php?feed=rss2", feed_response());
        let found = run(&fetcher, PAGE_URL).await;
        assert_eq!(found.found().expect("found").source, Source::LocalBody);
    }

    #[tokio::test]
    async fn local_links_are_tried_before_remote_ones() {
        // SimplePie's order is local-extension, local-body, remote-extension, remote-body, so a
        // same-host URL that merely mentions a feed word beats a cross-host `.rss` path.
        let document = page(
            "",
            r#"<a href="https://other.example/x.rss">Elsewhere</a><a href="/blog/feed">Here</a>"#,
        );
        let fetcher = serving(&document).with("https://e.example/blog/feed", feed_response());
        let found = run(&fetcher, PAGE_URL).await;
        assert_eq!(urls(&found), vec!["https://e.example/blog/feed"]);
        assert_eq!(found.found().expect("found").source, Source::LocalBody);
    }

    #[tokio::test]
    async fn a_remote_link_is_still_tried_when_there_is_nothing_local() {
        let document = page("", r#"<a href="https://other.example/x.rss">Elsewhere</a>"#);
        let fetcher = serving(&document).with("https://other.example/x.rss", feed_response());
        let found = run(&fetcher, PAGE_URL).await;
        assert_eq!(
            found.found().expect("found").source,
            Source::RemoteExtension
        );
    }

    #[tokio::test]
    async fn a_candidate_that_is_not_a_feed_is_reported_and_the_search_continues() {
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="/broken.xml">
               <link rel="alternate" type="application/rss+xml" href="/good.xml">"#,
            "",
        );
        let fetcher = serving(&document)
            .with(
                "https://e.example/broken.xml",
                html_response("<html><body>nope</body></html>"),
            )
            .with("https://e.example/good.xml", feed_response());
        let found = run(&fetcher, PAGE_URL).await;

        assert_eq!(found.candidates.len(), 2);
        assert_eq!(
            found.candidates[0].rejected.as_deref(),
            Some("the response is not a feed")
        );
        assert_eq!(
            found.found().expect("found").url,
            "https://e.example/good.xml"
        );
    }

    #[tokio::test]
    async fn a_fetch_failure_is_recorded_rather_than_fatal() {
        // No fixture is registered for `/missing.xml`, so the fixture fetcher answers 404.
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="/missing.xml">
               <link rel="alternate" type="application/rss+xml" href="/good.xml">"#,
            "",
        );
        let fetcher = serving(&document).with("https://e.example/good.xml", feed_response());
        let found = run(&fetcher, PAGE_URL).await;

        let rejected = found.candidates[0].rejected.as_deref().expect("rejected");
        assert!(rejected.contains("404"), "{rejected}");
        assert_eq!(
            found.found().expect("found").url,
            "https://e.example/good.xml"
        );
    }

    #[tokio::test]
    async fn an_unfetchable_page_is_an_error_not_an_empty_result() {
        let fetcher = FixtureFetcher::new();
        let err = discover(&fetcher, PAGE_URL, &FetchRequest::new(String::new()))
            .await
            .expect_err("nothing is served");
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[tokio::test]
    async fn the_candidate_budget_is_enforced() {
        let links: String = (0..MAX_CHECKED + 4)
            .map(|i| {
                format!(r#"<link rel="alternate" type="application/rss+xml" href="/f{i}.xml">"#)
            })
            .collect();
        let document = page(&links, "");
        let found = run(&serving(&document), PAGE_URL).await;

        assert_eq!(found.candidates.len(), MAX_CHECKED);
        assert!(found.found().is_none());
    }

    #[tokio::test]
    async fn a_url_that_appears_twice_is_fetched_once() {
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="/feed.xml">"#,
            r#"<a href="/feed.xml">RSS</a>"#,
        );
        let fetcher = serving(&document);
        let found = run(&fetcher, PAGE_URL).await;
        // `/feed.xml` is a `<link rel=alternate>` and an `<a>` with a feed extension and an `<a>`
        // whose URL mentions a feed word, so all three passes nominate it; the shared seen-set
        // nominates it once. No fixture is registered for it, so it is rejected — the point is that
        // it was only ever asked for once.
        assert_eq!(plan(document.as_bytes(), PAGE_URL).len(), 1);
        assert_eq!(found.candidates.len(), 1, "{found:?}");
        assert_eq!(
            fetcher.requests().len(),
            2,
            "the page and the one candidate"
        );
    }

    #[tokio::test]
    async fn schemes_that_cannot_be_fetched_are_ignored() {
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="javascript:alert(1)">
               <link rel="alternate" type="application/rss+xml" href="data:text/xml,x">"#,
            r##"<a href="mailto:feed@e.example">mail</a><a href="#frag">frag</a><a href="file:///etc/passwd">f</a>"##,
        );
        let found = run(&serving(&document), PAGE_URL).await;
        assert!(found.candidates.is_empty(), "{found:?}");
    }

    #[tokio::test]
    async fn a_page_with_no_links_at_all_yields_nothing() {
        let found = run(&serving(&page("", "<p>Hello</p>")), PAGE_URL).await;
        assert_eq!(found.url, PAGE_URL);
        assert!(found.candidates.is_empty());
        assert!(found.found().is_none());
    }

    #[tokio::test]
    async fn a_non_2xx_body_is_not_a_feed_even_if_it_parses() {
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="/gone.xml">"#,
            "",
        );
        let fetcher = serving(&document).with(
            "https://e.example/gone.xml",
            FixtureResponse::Body {
                status: 404,
                body: FEED.as_bytes().to_vec(),
                etag: None,
                last_modified: None,
                content_type: Some("application/rss+xml".to_owned()),
            },
        );
        let found = run(&fetcher, PAGE_URL).await;
        assert!(found.found().is_none(), "{found:?}");
        assert_eq!(
            found.candidates[0].rejected.as_deref(),
            Some("the response is not a feed")
        );
    }

    #[test]
    fn feed_extensions_are_read_from_the_path_only() {
        for url in [
            "https://e.example/feed.rss",
            "https://e.example/feed.RSS",
            "https://e.example/a/b.atom",
            "https://e.example/feed.rdf",
            "https://e.example/feed.xml",
            "https://e.example/feed.rss?v=2",
            "https://e.example/feed.rss#frag",
        ] {
            assert!(feed_extension(url), "{url}");
        }
        for url in [
            "https://e.example/feed",
            "https://e.example/feed?v=1.rss",
            "https://e.example/a.b/feed",
            "https://e.example/",
            "https://e.example/feed.html",
            "not a url",
        ] {
            assert!(!feed_extension(url), "{url}");
        }
    }

    #[test]
    fn the_name_match_is_case_insensitive_and_loose() {
        assert!(mentions_feed("https://e.example/index.php?feed=rss2"));
        assert!(mentions_feed("https://e.example/FEED"));
        assert!(mentions_feed("https://atom.example/"));
        assert!(!mentions_feed("https://e.example/news"));
    }

    #[test]
    fn hosts_are_compared_ignoring_case_and_default_ports() {
        assert!(same_host("https://E.example/a", "https://e.example/b"));
        assert!(same_host("http://e.example:80/a", "http://e.example/b"));
        assert!(!same_host(
            "https://e.example:8443/a",
            "https://e.example/b"
        ));
        assert!(!same_host("https://other.example/a", "https://e.example/b"));
        assert!(
            !same_host("http://e.example/a", "https://e.example/b"),
            "scheme is not compared, but the default port differs"
        );
    }

    #[test]
    fn a_type_attribute_keeps_only_its_mime_type() {
        assert_eq!(mime("application/rss+xml"), "application/rss+xml");
        assert_eq!(
            mime("application/rss+xml; charset=utf-8"),
            "application/rss+xml"
        );
        assert_eq!(mime("  application/atom+xml ;x=1"), "application/atom+xml");
    }

    #[test]
    fn the_plan_deduplicates_and_orders_by_pass() {
        let document = page(
            r#"<link rel="alternate" type="application/rss+xml" href="/link.xml">"#,
            r#"<a rel="feed" href="/anchor.xml">a</a>
               <a href="/blog/feed">1</a><a href="/blog/feed">2</a>
               <a href="https://other.example/remote.rss">r</a>"#,
        );
        let plan = plan(document.as_bytes(), PAGE_URL);
        assert_eq!(
            plan,
            vec![
                ("https://e.example/link.xml".to_owned(), Source::Link),
                ("https://e.example/anchor.xml".to_owned(), Source::Anchor),
                ("https://e.example/blog/feed".to_owned(), Source::LocalBody),
                (
                    "https://other.example/remote.rss".to_owned(),
                    Source::RemoteExtension
                ),
            ],
            "the rel scan claims /anchor.xml before the extension pass can, and /blog/feed appears once"
        );
    }
}
