//! Full-article extraction: `pathEntries` and `path_entries_filter`.
//!
//! Most feeds publish a teaser and a link. When a feed is configured with a CSS selector, the
//! refresh pipeline fetches the article page too and replaces the teaser with the real body, which
//! is the difference between a reader you skim and one you read in.
//!
//! The two settings are FreshRSS's and so are their names in the database: `pathEntries` selects the
//! article, `path_entries_filter` strips what the article dragged in with it — navigation, related-
//! post widgets, the newsletter signup. The filter is applied twice, once to the raw page and once
//! to the sanitized output, because the sanitizer rewrites markup and a selector that matched before
//! it may not match after (or may match something new).
//!
//! Selectors are CSS and are evaluated as CSS. PHP FreshRSS translates them to XPath with
//! `phpgt/cssxpath` because its DOM has no CSS engine; that translation is lossy and surprising, and
//! the plan is explicit that this port does not repeat it.
//!
//! One part of `Entry::getContentByParsing` is *not* here: `path_entries_conditions`, the list of
//! searches that gate extraction per article. It needs the search engine, and the crates that hold
//! the two do not depend on each other, so the gate lives in the server's refresh pipeline.

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{ElementRef, Html, Selector};

use crate::error::FeedError;
use crate::sanitize::uri::absolutize;
use crate::sanitize::{sanitize_html, SanitizeOptions};

static BODY: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("body").expect("`body` is a valid selector"));
static BASE_WITH_HREF: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("base[href]").expect("`base[href]` is a valid selector"));
static META_WITH_CONTENT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("meta[content]").expect("`meta[content]` is a valid selector")
});

/// What one extraction produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    /// The sanitized article HTML, empty when nothing matched or the filter removed everything.
    pub html: String,
    /// How many elements `pathEntries` matched.
    ///
    /// Kept separate from "the html is empty" because the two mean different things and FreshRSS
    /// logs them differently: zero matches is a selector that has gone stale, while matches that
    /// produced nothing is a filter that was too broad.
    pub matched: usize,
}

/// Extracts an article body from a fetched page.
///
/// `selector` is the feed's `pathEntries` and `filter` its `path_entries_filter`, either of which may
/// be empty. The result is already sanitized against `options`, so the caller stores it as-is.
///
/// # Errors
///
/// [`FeedError::BadConfig`] when either selector is not valid CSS. That is a 400 at the API rather
/// than a silent empty result, because a mistyped selector otherwise looks exactly like a page that
/// has no article on it.
pub fn extract(
    page: &str,
    selector: &str,
    filter: &str,
    options: &SanitizeOptions,
) -> Result<Extracted, FeedError> {
    let selector = trim_selector(selector);
    if selector.is_empty() {
        return Ok(Extracted::default());
    }
    let selector = parse_selector(selector)?;

    let filter = trim_selector(filter);
    let filter_selector = (!filter.is_empty())
        .then(|| parse_selector(filter))
        .transpose()?;

    let mut document = Html::parse_document(page);
    let matched: Vec<_> = document
        .select(&selector)
        .map(|element| element.id())
        .collect();
    if matched.is_empty() {
        return Ok(Extracted::default());
    }

    // Node ids rather than element references, collected before anything is mutated: an
    // `ElementRef` borrows the whole document, and removing a node needs it mutably.
    let mut dropped = HashSet::new();
    if let Some(filter) = &filter_selector {
        let mut doomed = Vec::new();
        for id in &matched {
            let Some(element) = document.tree.get(*id).and_then(ElementRef::wrap) else {
                continue;
            };
            // A node the filter matches itself is skipped whole, rather than emptied: `pathEntries`
            // pointed at the very thing `path_entries_filter` says to remove.
            if filter.matches(&element) {
                dropped.insert(*id);
            }
            doomed.extend(element.select(filter).map(|element| element.id()));
        }
        for id in doomed {
            if let Some(mut node) = document.tree.get_mut(id) {
                node.detach();
            }
        }
    }

    let mut html = String::new();
    for id in &matched {
        if dropped.contains(id) {
            continue;
        }
        let Some(element) = document.tree.get(*id).and_then(ElementRef::wrap) else {
            continue;
        };
        html.push_str(&element.html());
        html.push('\n');
    }

    let sanitized = sanitize_html(&html, options);
    let html = match &filter_selector {
        Some(filter) => strip_with(&sanitized, filter).unwrap_or(sanitized),
        None => sanitized,
    };
    Ok(Extracted {
        html: html.trim().to_owned(),
        matched: matched.len(),
    })
}

/// Removes every element matching `filter` from `html`.
///
/// This is the half of extraction that also runs on its own: a feed with a filter but no
/// `pathEntries` gets its *teaser* cleaned instead, which is how a publisher's "continue reading"
/// block and share buttons are dropped without fetching anything.
///
/// Returns `None` when nothing matched, so a caller can tell "unchanged" from "emptied". That
/// distinction is what stops a refresh from rewriting every article it looked at: an unchanged body
/// must not be written back, or the change-detection hash moves and the whole backlog looks new.
///
/// # Errors
///
/// [`FeedError::BadConfig`] when the selector is not valid CSS.
pub fn strip(html: &str, filter: &str) -> Result<Option<String>, FeedError> {
    let filter = trim_selector(filter);
    if filter.is_empty() {
        return Ok(None);
    }
    Ok(strip_with(html, &parse_selector(filter)?))
}

fn strip_with(html: &str, filter: &Selector) -> Option<String> {
    let mut document = Html::parse_document(html);
    let doomed: Vec<_> = document
        .select(filter)
        .map(|element| element.id())
        .collect();
    if doomed.is_empty() {
        return None;
    }
    for id in doomed {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
    // The body's *inner* HTML. Parsing a fragment as a document wraps it in `<html><body>`, and
    // writing that wrapper back would put a `<body>` element inside an article.
    let body = document.select(&BODY).next()?;
    Some(body.inner_html())
}

/// The URL to resolve an extracted article's relative links against.
///
/// The page's own `<base href>` when it has one, otherwise the URL it was fetched from — which after
/// redirects is not necessarily the URL that was asked for. A protocol-relative `//cdn.example/`
/// needs no special case here because a real URL parser inherits the scheme, which is more than the
/// string handling it replaces can say.
pub fn document_base(html: &str, url: &str) -> String {
    let document = Html::parse_document(html);
    let base = document
        .select(&BASE_WITH_HREF)
        .next()
        .and_then(|element| element.attr("href"))
        .map(str::trim)
        .filter(|href| !href.is_empty());
    match base {
        Some(href) => absolutize(href, url),
        None => url.to_owned(),
    }
}

/// The target of the first `<meta http-equiv="refresh">` that points somewhere else.
///
/// Single-step by design: the redirect budget is the caller's, because it is shared with the HTTP
/// redirects already followed to get the page, and only the caller knows how many those were.
pub fn meta_refresh(html: &str, url: &str) -> Option<String> {
    let document = Html::parse_document(html);
    for meta in document.select(&META_WITH_CONTENT) {
        let equiv = meta.attr("http-equiv").unwrap_or_default().trim();
        if !equiv.eq_ignore_ascii_case("refresh") {
            continue;
        }
        let Some(target) = refresh_target(meta.attr("content")?) else {
            continue;
        };
        let absolute = absolutize(&target, url);
        if absolute != url {
            return Some(absolute);
        }
    }
    None
}

/// The URL out of a `content` attribute like `5; url=/next`, `0;URL='/next'` or `/next`.
fn refresh_target(content: &str) -> Option<String> {
    // The delay: digits, separators and whitespace. A comma is not in PHP's character class but
    // appears in the wild, and the cost of accepting it is nothing.
    let rest = content
        .trim_start_matches(|c: char| {
            c.is_ascii_digit() || matches!(c, '.' | ';' | ',') || c.is_whitespace()
        })
        .trim_start();
    // The `url=` keyword is only the keyword when an `=` follows it, so `urlaub=…` is left alone.
    let rest = match rest.get(..3) {
        Some(head) if head.eq_ignore_ascii_case("url") => {
            let after = rest[3..].trim_start();
            match after.strip_prefix('=') {
                Some(value) => value.trim_start(),
                None => rest,
            }
        }
        _ => rest,
    };
    let target = unquote(rest.trim());
    (!target.is_empty()).then(|| target.to_owned())
}

/// Drops one layer of matching quotes.
///
/// `content="0;url='/next'"` is common in templated sites, and a quoted path is not a path.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    let quoted =
        bytes.len() >= 2 && matches!(bytes[0], b'\'' | b'"') && bytes[bytes.len() - 1] == bytes[0];
    if quoted {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// `trim($selector, ', ')`: a stray trailing comma makes the whole list invalid.
fn trim_selector(selector: &str) -> &str {
    selector.trim_matches(|c: char| c == ',' || c.is_ascii_whitespace())
}

fn parse_selector(selector: &str) -> Result<Selector, FeedError> {
    Selector::parse(selector)
        .map_err(|err| FeedError::BadConfig(format!("invalid CSS selector {selector:?}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SanitizeOptions {
        SanitizeOptions {
            base_url: "https://e.example/article".to_owned(),
            force_https_domains: Vec::new(),
            max_length: None,
        }
    }

    const PAGE: &str = r#"<html><head><title>Article</title></head><body>
<nav class="site-nav"><a href="/">Home</a></nav>
<article class="post">
	<h1>The title</h1>
	<p>The first paragraph.</p>
	<div class="share">Share this!</div>
	<p>The second paragraph.</p>
	<aside class="related">Read more</aside>
</article>
<footer>Copyright</footer>
</body></html>"#;

    #[test]
    fn extracts_the_selected_element() {
        let extracted = extract(PAGE, "article.post", "", &options()).unwrap();
        assert_eq!(extracted.matched, 1);
        assert!(
            extracted.html.contains("The first paragraph."),
            "{}",
            extracted.html
        );
        assert!(
            extracted.html.contains("The second paragraph."),
            "{}",
            extracted.html
        );
        assert!(extracted.html.starts_with("<article"), "{}", extracted.html);
        assert!(!extracted.html.contains("site-nav"), "{}", extracted.html);
        assert!(!extracted.html.contains("Copyright"), "{}", extracted.html);
    }

    #[test]
    fn several_matches_are_concatenated() {
        let extracted = extract(PAGE, "article.post p", "", &options()).unwrap();
        assert_eq!(extracted.matched, 2);
        let first = extracted.html.find("first paragraph").expect("first");
        let second = extracted.html.find("second paragraph").expect("second");
        assert!(first < second, "document order: {}", extracted.html);
    }

    #[test]
    fn the_filter_strips_boilerplate_from_inside_the_article() {
        let extracted = extract(PAGE, "article.post", ".share, .related", &options()).unwrap();
        assert_eq!(extracted.matched, 1);
        assert!(
            extracted.html.contains("first paragraph"),
            "{}",
            extracted.html
        );
        assert!(
            !extracted.html.contains("Share this!"),
            "{}",
            extracted.html
        );
        assert!(!extracted.html.contains("Read more"), "{}", extracted.html);
    }

    #[test]
    fn a_selector_that_matches_only_the_filter_yields_nothing_but_still_reports_the_match() {
        let extracted = extract(PAGE, ".share", ".share", &options()).unwrap();
        assert_eq!(extracted.matched, 1, "the selector did match");
        assert!(extracted.html.is_empty(), "{}", extracted.html);
    }

    #[test]
    fn a_selector_that_matches_nothing_is_reported_as_zero() {
        let extracted = extract(PAGE, "#does-not-exist", "", &options()).unwrap();
        assert_eq!(extracted, Extracted::default());
    }

    #[test]
    fn an_empty_selector_extracts_nothing() {
        assert_eq!(
            extract(PAGE, "", "", &options()).unwrap(),
            Extracted::default()
        );
        assert_eq!(
            extract(PAGE, "  , ", "", &options()).unwrap(),
            Extracted::default()
        );
    }

    #[test]
    fn an_invalid_selector_is_a_configuration_error_not_an_empty_result() {
        let err = extract(PAGE, "article >>>", "", &options()).unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");
        assert!(err.to_string().contains("article >>>"), "{err}");

        let err = strip("<p>x</p>", ">>>").unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");
    }

    #[test]
    fn a_trailing_comma_is_tolerated() {
        // PHP trims `, ` off both ends, because a trailing comma makes the whole list invalid.
        let extracted = extract(PAGE, "article.post,", "", &options()).unwrap();
        assert_eq!(extracted.matched, 1);
        let extracted = extract(PAGE, "article.post", ".share,", &options()).unwrap();
        assert!(
            !extracted.html.contains("Share this!"),
            "{}",
            extracted.html
        );
    }

    #[test]
    fn the_extraction_is_sanitized() {
        let hostile = r#"<html><body><article><p>Safe</p><script>alert(1)</script><a href="javascript:alert(1)">x</a><img src="/p.png"></article></body></html>"#;
        let extracted = extract(hostile, "article", "", &options()).unwrap();
        assert!(!extracted.html.contains("<script"), "{}", extracted.html);
        assert!(
            !extracted.html.contains("javascript:"),
            "{}",
            extracted.html
        );
        assert!(
            extracted.html.contains("https://e.example/p.png"),
            "relative image resolved against the base: {}",
            extracted.html
        );
    }

    #[test]
    fn the_filter_runs_again_after_sanitizing() {
        // The second pass exists because sanitizing rewrites markup, so a selector can match the
        // output that did not match the input. `.keep` survives sanitizing and is then removed.
        let page = r#"<html><body><article><p>Body</p><span class="keep">boilerplate</span></article></body></html>"#;
        let extracted = extract(page, "article", "span", &options()).unwrap();
        assert!(extracted.html.contains("Body"), "{}", extracted.html);
        assert!(
            !extracted.html.contains("boilerplate"),
            "{}",
            extracted.html
        );
    }

    #[test]
    fn strip_removes_matching_elements_and_reports_whether_it_did() {
        let html = "<p>Keep</p><div class=\"ads\">Buy</div><p>Also keep</p>";
        let stripped = strip(html, ".ads").unwrap().expect("something matched");
        assert!(!stripped.contains("Buy"), "{stripped}");
        assert!(stripped.contains("Keep"), "{stripped}");
        assert!(stripped.contains("Also keep"), "{stripped}");
        assert!(
            !stripped.contains("<body>"),
            "no document wrapper: {stripped}"
        );
    }

    #[test]
    fn strip_returns_none_when_nothing_matched() {
        assert_eq!(strip("<p>Keep</p>", ".ads").unwrap(), None);
        assert_eq!(strip("<p>Keep</p>", "").unwrap(), None);
        assert_eq!(strip("", ".ads").unwrap(), None);
    }

    #[test]
    fn strip_removes_nested_matches_once() {
        let html = r#"<div class="ads"><span class="ads">inner</span>outer</div><p>keep</p>"#;
        let stripped = strip(html, ".ads").unwrap().expect("something matched");
        assert!(!stripped.contains("inner"), "{stripped}");
        assert!(!stripped.contains("outer"), "{stripped}");
        assert!(stripped.contains("keep"), "{stripped}");
    }

    #[test]
    fn the_document_base_is_the_first_base_href() {
        let html =
            r#"<html><head><base href="/sub/"><base href="/other/"></head><body></body></html>"#;
        assert_eq!(
            document_base(html, "https://e.example/article"),
            "https://e.example/sub/"
        );
    }

    #[test]
    fn without_a_base_href_the_document_url_is_the_base() {
        assert_eq!(
            document_base("<html><body>x</body></html>", "https://e.example/a/b"),
            "https://e.example/a/b"
        );
        assert_eq!(
            document_base(r#"<base href="   ">"#, "https://e.example/a"),
            "https://e.example/a",
            "a blank href is as absent as none"
        );
    }

    #[test]
    fn a_protocol_relative_base_inherits_the_scheme() {
        assert_eq!(
            document_base(r#"<base href="//cdn.example/x/">"#, "https://e.example/a"),
            "https://cdn.example/x/"
        );
    }

    #[test]
    fn a_meta_refresh_target_is_found_and_absolutized() {
        for (content, expected) in [
            ("0;url=/next", "https://e.example/next"),
            (
                "5; URL = https://other.example/x",
                "https://other.example/x",
            ),
            ("0;url='/quoted'", "https://e.example/quoted"),
            ("10;url=&quot;double&quot;", "https://e.example/double"),
            ("/bare", "https://e.example/bare"),
        ] {
            let html = format!(
                r#"<html><head><meta http-equiv="Refresh" content="{content}"></head><body></body></html>"#
            );
            assert_eq!(
                meta_refresh(&html, "https://e.example/here"),
                Some(expected.to_owned()),
                "content={content:?}"
            );
        }
    }

    #[test]
    fn a_meta_refresh_to_the_same_url_is_not_a_redirect() {
        let html = r#"<meta http-equiv="refresh" content="0;url=https://e.example/here">"#;
        assert_eq!(meta_refresh(html, "https://e.example/here"), None);
    }

    #[test]
    fn only_a_refresh_meta_counts() {
        let html = r#"<meta http-equiv="content-type" content="0;url=/next"><meta name="x" content="0;url=/other">"#;
        assert_eq!(meta_refresh(html, "https://e.example/here"), None);
    }

    #[test]
    fn a_meta_refresh_with_no_usable_target_is_skipped() {
        let html = r#"<meta http-equiv="refresh" content="5"><meta http-equiv="refresh" content="0;url=/next">"#;
        assert_eq!(
            meta_refresh(html, "https://e.example/here"),
            Some("https://e.example/next".to_owned()),
            "the second one is used because the first names no URL"
        );
    }

    #[test]
    fn a_keyword_that_is_not_the_keyword_is_left_alone() {
        // `urlaub=` is not `url=`; stripping the first three characters would corrupt it.
        assert_eq!(refresh_target("0;urlaub=/x").as_deref(), Some("urlaub=/x"));
        assert_eq!(refresh_target("url=/x").as_deref(), Some("/x"));
        assert_eq!(refresh_target("  "), None);
    }

    #[test]
    fn extraction_survives_broken_html() {
        // html5ever rather than an XML parser, because an article page is written by a CMS and not by
        // a machine that cares about well-formedness.
        let page = "<html><body><article><p>Unclosed<p>Also unclosed</article>";
        let extracted = extract(page, "article", "", &options()).unwrap();
        assert_eq!(extracted.matched, 1);
        assert!(extracted.html.contains("Unclosed"), "{}", extracted.html);
        assert!(
            extracted.html.contains("Also unclosed"),
            "{}",
            extracted.html
        );
    }
}
