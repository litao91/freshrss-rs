//! HTML sanitization for feed content.
//!
//! This module is the project's single XSS trust boundary. Article bodies come from strangers on the
//! internet and are rendered through exactly one `{@html}` in the reader, so everything unsafe has
//! to be removed *here* — the frontend cannot defend against a body that was stored dirty, and a
//! body sanitized twice can be mangled.
//!
//! The shape is ported from `FreshRSS_SimplePieCustom`, which configures SimplePie's sanitizer with a
//! ~200-entry element whitelist. [`policy`] holds that whitelist verbatim; this module wires it to
//! `ammonia` and adds the two things ammonia does not do on its own: a per-attribute URL scheme
//! decision ([`uri`]) and the shrink-and-retry loop that fits a body into a byte budget.

pub mod policy;
pub mod uri;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use ammonia::{Builder, UrlRelative};
use once_cell::sync::Lazy;

/// Everything that varies between feeds.
#[derive(Debug, Clone, Default)]
pub struct SanitizeOptions {
    /// The feed's own URL, used to resolve relative links in article bodies.
    ///
    /// Must never be the reader's URL: a relative `<img src="/logo.png">` belongs to the publisher.
    pub base_url: String,
    /// Hosts whose `http://` links are upgraded to `https://`.
    pub force_https_domains: Vec<String>,
    /// A byte budget for the result, or `None` for unlimited.
    pub max_length: Option<usize>,
}

/// The static halves of the whitelist, built once.
///
/// `ammonia::Builder` takes ownership of its sets, and a builder cannot be reused across feeds (the
/// base URL differs), so these are materialized once here rather than re-hashed for every article.
static TAG_SET: Lazy<HashSet<&'static str>> = Lazy::new(|| policy::TAGS.iter().copied().collect());
static GENERIC_ATTR_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| policy::GENERIC_ATTRIBUTES.iter().copied().collect());
static PREFIX_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| policy::GENERIC_ATTRIBUTE_PREFIXES.iter().copied().collect());
static CLEAN_CONTENT_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| policy::CLEAN_CONTENT_TAGS.iter().copied().collect());
static URL_SCHEME_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| HashSet::from_iter(["http", "https", "mailto", "data"]));
static TAG_ATTR_MAP: Lazy<HashMap<&'static str, HashSet<&'static str>>> = Lazy::new(|| {
    policy::TAG_ATTRIBUTES
        .iter()
        .map(|(tag, list)| (*tag, list.iter().copied().collect()))
        .collect()
});
static ADDED_ATTR_MAP: Lazy<HashMap<&'static str, HashMap<&'static str, &'static str>>> =
    Lazy::new(|| {
        let mut map: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
        for (tag, attr, value) in policy::ADDED_ATTRIBUTES {
            map.entry(tag).or_default().insert(attr, value);
        }
        map
    });

/// Sanitizes an article body.
///
/// The result is HTML that is safe to render and safe to store. It is idempotent: sanitizing an
/// already-sanitized body returns it unchanged, which matters because the refresh pipeline may run
/// this again when an article is edited upstream.
pub fn sanitize_html(content: &str, options: &SanitizeOptions) -> String {
    if content.trim().is_empty() {
        return String::new();
    }
    let Some(max) = options.max_length.filter(|m| *m > 0) else {
        return run_ammonia(content, options);
    };
    sanitize_within(content, max, options)
}

/// Sanitizes, shrinking the input until the *output* fits `max_bytes`.
///
/// The retry loop is ported from `FreshRSS_SimplePieCustom::sanitizeHTML` and exists because
/// sanitizing can grow a document: an unclosed `<p>` comes back as `<p>…</p>`, and a decoded `<`
/// comes back as `&lt;`. So cutting the input to N bytes does not guarantee N bytes out, and the
/// only way to find the right input length is to measure the overflow and try again.
fn sanitize_within(content: &str, max_bytes: usize, options: &SanitizeOptions) -> String {
    let mut input = content.to_owned();
    let mut budget = max_bytes;

    for attempt in 0..MAX_ATTEMPTS {
        input = cut(&input, budget);
        let candidate = run_ammonia(&input, options);
        if candidate.len() <= max_bytes {
            // FreshRSS additionally requires the result to contain text and falls back to plain text
            // when it does not. That is a divergence worth making: an article that is only an
            // `<img>` — a comic, a photo post — has no text at all, and blanking it loses the whole
            // article rather than trimming it.
            return candidate;
        }
        let overflow = candidate.len() - max_bytes;
        budget = budget
            .saturating_sub(overflow + (1 << attempt))
            .min(input.len());
        if budget == 0 {
            break;
        }
    }

    // Every attempt overflowed. Plain text always fits, at the cost of the images and links.
    let full = run_ammonia(content, options);
    crate::text::strcut(&crate::text::strip_tags(&full), max_bytes).to_owned()
}

/// How many shrink-and-resanitize passes to attempt before falling back to plain text.
///
/// FreshRSS's value. Each pass shrinks by the measured overflow plus `2^attempt`, so four passes
/// converge on anything short of pathological — and a pathological document is better served by the
/// fallback than by an unbounded loop.
const MAX_ATTEMPTS: usize = 4;

/// Truncates to `max_bytes` on a character boundary, then removes a tag or entity the cut split.
fn cut(input: &str, max_bytes: usize) -> String {
    let cut = crate::text::strcut(input, max_bytes);
    crate::text::drop_trailing_fragment(cut).to_owned()
}

/// One ammonia pass over `content`.
fn run_ammonia(content: &str, options: &SanitizeOptions) -> String {
    let base_url = options.base_url.clone();
    let force_https = options.force_https_domains.clone();

    let mut builder = Builder::default();
    builder
        .tags(TAG_SET.clone())
        .clean_content_tags(CLEAN_CONTENT_SET.clone())
        .generic_attributes(GENERIC_ATTR_SET.clone())
        .generic_attribute_prefixes(PREFIX_SET.clone())
        .tag_attributes(TAG_ATTR_MAP.clone())
        .set_tag_attribute_values(ADDED_ATTR_MAP.clone())
        .url_schemes(URL_SCHEME_SET.clone())
        // Relative URLs are resolved in the attribute filter below, against the *feed's* base.
        // Letting ammonia do it too would either be redundant or use the wrong base.
        .url_relative(UrlRelative::PassThrough)
        .link_rel(Some(policy::LINK_REL))
        // Comments are stripped: conditional-comment tricks (`<!--[if IE]><script>…`) are the
        // classic bypass, and no feed needs a comment to survive.
        .strip_comments(true)
        .attribute_filter(move |element, attribute, value| {
            filter_attribute(element, attribute, value, &base_url, &force_https)
        });

    builder.clean(content).to_string()
}

/// Decides the fate of one attribute value.
///
/// Returning `None` drops the attribute; returning `Some(value)` keeps it, possibly rewritten.
///
/// The order matters. The scheme check runs before absolutization, so a `javascript:` URL is gone
/// before anything can make it look like a path; and force-https runs last, so it sees the final
/// absolute URL rather than a relative one it could not match a domain against.
fn filter_attribute<'v>(
    element: &str,
    attribute: &str,
    value: &'v str,
    base_url: &str,
    force_https_domains: &[String],
) -> Option<Cow<'v, str>> {
    if policy::STRIPPED_DATA_ATTRIBUTES.contains(&attribute) {
        return None;
    }
    if !policy::is_url_attribute(element, attribute) {
        return Some(Cow::Borrowed(value));
    }

    match uri::classify(element, attribute, value) {
        uri::SchemeDecision::Denied => None,
        uri::SchemeDecision::Relative => {
            let resolved = uri::absolutize(value, base_url);
            // Both steps allocate, so there is nothing left to borrow from `value`.
            Some(Cow::Owned(
                uri::force_https(&resolved, force_https_domains).unwrap_or(resolved),
            ))
        }
        uri::SchemeDecision::Allowed => Some(upgrade_https(value, force_https_domains)),
    }
}

/// Applies the force-https rewrite, borrowing when nothing changes.
fn upgrade_https<'a>(value: &'a str, domains: &[String]) -> Cow<'a, str> {
    match uri::force_https(value, domains) {
        Some(upgraded) => Cow::Owned(upgraded),
        None => Cow::Borrowed(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> SanitizeOptions {
        SanitizeOptions {
            base_url: "https://example.com/feed.xml".to_owned(),
            force_https_domains: Vec::new(),
            max_length: None,
        }
    }

    fn clean(html: &str) -> String {
        sanitize_html(html, &opts())
    }

    // --- elements ----------------------------------------------------------

    #[test]
    fn ordinary_markup_survives() {
        assert_eq!(
            clean("<p>Hello <b>world</b> and <em>friends</em></p>"),
            "<p>Hello <b>world</b> and <em>friends</em></p>"
        );
    }

    #[test]
    fn script_and_its_body_are_gone() {
        let out = clean("<p>ok</p><script>alert(1)</script>");
        assert!(!out.contains("script"), "{out}");
        assert!(!out.contains("alert"), "{out}");
        assert_eq!(out, "<p>ok</p>");
    }

    #[test]
    fn style_and_its_body_are_gone() {
        let out = clean("<style>body{display:none}</style><p>x</p>");
        assert!(!out.to_ascii_lowercase().contains("style"), "{out}");
        assert!(!out.contains("display"), "{out}");
    }

    #[test]
    fn dangerous_elements_are_unwrapped_rather_than_dropped_whole() {
        // The tag goes, the text stays: removing the words too would silently censor articles.
        let out = clean("<form><input value=x>text</form>");
        assert!(!out.contains("<form"), "{out}");
        assert!(!out.contains("<input"), "{out}");
        assert!(out.contains("text"), "{out}");
    }

    #[test]
    fn object_embed_and_applet_do_not_survive() {
        let out = clean(
            r#"<object data="x.swf"><param name="a" value="b"></object>
               <embed src="x.swf"><applet code="E.class"></applet>"#,
        );
        for tag in ["object", "embed", "applet", "param"] {
            assert!(!out.contains(&format!("<{tag}")), "{out}");
        }
    }

    #[test]
    fn svg_is_removed() {
        let out = clean(r#"<svg onload="alert(1)"><circle r="1"/></svg>"#);
        assert!(!out.contains("svg"), "{out}");
        assert!(!out.contains("onload"), "{out}");
    }

    #[test]
    fn iframe_survives_but_is_sandboxed() {
        let out = clean(r#"<iframe src="https://youtube.com/embed/x"></iframe>"#);
        assert!(out.contains("<iframe"), "{out}");
        assert!(out.contains("sandbox="), "{out}");
        assert!(out.contains("allow-scripts"), "{out}");
        assert!(out.contains("allowfullscreen"), "{out}");
    }

    #[test]
    fn audio_and_video_get_controls_and_do_not_preload() {
        let out =
            clean(r#"<audio src="https://x/e.mp3"></audio><video src="https://x/v.mp4"></video>"#);
        assert!(out.contains(r#"preload="none""#), "{out}");
        assert_eq!(out.matches("controls=").count(), 2, "{out}");
    }

    #[test]
    fn comments_are_stripped() {
        let out = clean("<p>a</p><!-- secret --><!--[if IE]><script>x</script><![endif]-->");
        assert!(!out.contains("<!--"), "{out}");
        assert!(!out.contains("secret"), "{out}");
        assert!(!out.contains("script"), "{out}");
    }

    #[test]
    fn mathml_is_preserved() {
        let out = clean("<math display=\"block\"><mfrac><mn>1</mn><mn>2</mn></mfrac></math>");
        assert!(out.contains("<math"), "{out}");
        assert!(out.contains("<mfrac>"), "{out}");
    }

    // --- attributes --------------------------------------------------------

    #[test]
    fn event_handlers_are_removed() {
        for handler in [
            "onclick",
            "onerror",
            "onload",
            "onmouseover",
            "onfocus",
            "onanimationstart",
        ] {
            let out = clean(&format!("<p {handler}=\"alert(1)\">x</p>"));
            assert!(!out.to_ascii_lowercase().contains(handler), "{out}");
        }
    }

    #[test]
    fn style_attributes_are_removed() {
        let out = clean(r#"<p style="background:url(javascript:alert(1))">x</p>"#);
        assert!(!out.contains("style"), "{out}");
    }

    #[test]
    fn id_and_class_are_removed() {
        // SimplePie renames them instead; the practical effect is the same — in-article `#anchor`
        // links stop working — but stripping also stops a feed from targeting the reader's own CSS.
        let out = clean(r#"<p id="rd-nav" class="rd-item is-unread">x</p>"#);
        assert!(!out.contains("id="), "{out}");
        assert!(!out.contains("class="), "{out}");
        assert!(!out.contains("rd-nav"), "{out}");
    }

    #[test]
    fn allowed_attributes_survive() {
        let out = clean(r#"<img src="https://x/a.png" alt="A" width="10" height="20">"#);
        assert!(out.contains(r#"alt="A""#), "{out}");
        assert!(out.contains(r#"width="10""#), "{out}");
    }

    #[test]
    fn aria_and_data_attributes_survive() {
        let out = clean(r#"<p aria-label="x" data-foo="1">y</p>"#);
        assert!(out.contains(r#"aria-label="x""#), "{out}");
        assert!(out.contains(r#"data-foo="1""#), "{out}");
    }

    #[test]
    fn simplepie_own_data_attributes_are_stripped() {
        let out = clean(r#"<p data-leave-validation="1" data-original="x" data-keep="2">y</p>"#);
        assert!(!out.contains("data-leave-validation"), "{out}");
        assert!(!out.contains("data-original"), "{out}");
        assert!(out.contains(r#"data-keep="2""#), "{out}");
    }

    #[test]
    fn links_get_rel_noopener() {
        let out = clean(r#"<a href="https://x/a">a</a>"#);
        assert!(out.contains("noopener"), "{out}");
        assert!(out.contains("noreferrer"), "{out}");
    }

    #[test]
    fn an_existing_rel_is_replaced_not_appended_to() {
        // `rel="author"` is not a security relation; leaving it would drop the noopener.
        let out = clean(r#"<a href="https://x/a" rel="author">a</a>"#);
        assert!(out.contains("noopener"), "{out}");
    }

    // --- URLs --------------------------------------------------------------

    #[test]
    fn javascript_urls_are_removed_whatever_the_case_or_spacing() {
        for url in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "  javascript:alert(1)",
            "java\tscript:alert(1)",
            "javascript&#58;alert(1)",
        ] {
            let out = clean(&format!(r#"<a href="{url}">x</a>"#));
            assert!(
                !out.to_ascii_lowercase().contains("javascript"),
                "{out} for {url:?}"
            );
            assert!(!out.contains("alert"), "{out} for {url:?}");
        }
    }

    #[test]
    fn an_encoded_javascript_url_is_removed() {
        let out = clean("<a href=\"&#106;avascript:alert(1)\">x</a>");
        assert!(!out.to_ascii_lowercase().contains("javascript"), "{out}");
    }

    #[test]
    fn other_dangerous_schemes_are_removed() {
        for url in [
            "vbscript:x",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
        ] {
            let out = clean(&format!(r#"<a href="{url}">x</a>"#));
            assert!(!out.contains("href"), "{out} for {url:?}");
        }
    }

    #[test]
    fn a_data_image_is_kept_on_an_img_src_only() {
        let png = "data:image/png;base64,iVBORw0KGgo=";
        let out = clean(&format!(r#"<img src="{png}">"#));
        assert!(out.contains(png), "{out}");

        let out = clean(&format!(r#"<a href="{png}">x</a>"#));
        assert!(!out.contains("href"), "{out}");

        let out = clean(&format!(r#"<iframe src="{png}"></iframe>"#));
        assert!(!out.contains(png), "{out}");
    }

    #[test]
    fn a_data_svg_image_is_removed() {
        let out = clean(r#"<img src="data:image/svg+xml,<svg onload=alert(1)>">"#);
        assert!(!out.contains("data:"), "{out}");
    }

    #[test]
    fn relative_urls_are_resolved_against_the_feed_base() {
        let out = clean(r#"<img src="/img/a.png"><a href="../post.html">x</a>"#);
        assert!(
            out.contains(r#"src="https://example.com/img/a.png""#),
            "{out}"
        );
        assert!(
            out.contains(r#"href="https://example.com/post.html""#),
            "{out}"
        );
    }

    #[test]
    fn a_relative_url_never_resolves_against_the_readers_origin() {
        // With no base at all the URL must be left relative rather than invented.
        let options = SanitizeOptions {
            base_url: String::new(),
            ..opts()
        };
        let out = sanitize_html(r#"<img src="/img/a.png">"#, &options);
        assert!(out.contains(r#"src="/img/a.png""#), "{out}");
    }

    #[test]
    fn a_fragment_link_stays_relative() {
        let out = clean(r##"<a href="#note">n</a>"##);
        assert!(out.contains(r##"href="#note""##), "{out}");
    }

    #[test]
    fn force_https_upgrades_a_listed_domain() {
        let options = SanitizeOptions {
            force_https_domains: vec!["example.org".to_owned()],
            ..opts()
        };
        let out = sanitize_html(
            r#"<img src="http://example.org/a.png"><img src="http://other.test/b.png">"#,
            &options,
        );
        assert!(out.contains("https://example.org/a.png"), "{out}");
        assert!(out.contains("http://other.test/b.png"), "{out}");
    }

    #[test]
    fn force_https_applies_after_absolutization() {
        // The base is http, so the relative URL becomes an http URL on a listed domain and must
        // still be upgraded — the order of the two rewrites is observable here.
        let options = SanitizeOptions {
            base_url: "http://example.org/feed.xml".to_owned(),
            force_https_domains: vec!["example.org".to_owned()],
            max_length: None,
        };
        let out = sanitize_html(r#"<img src="/a.png">"#, &options);
        assert!(out.contains("https://example.org/a.png"), "{out}");
    }

    #[test]
    fn cite_attributes_are_scheme_checked_too() {
        let out = clean(r#"<blockquote cite="javascript:alert(1)">x</blockquote>"#);
        assert!(!out.to_ascii_lowercase().contains("javascript"), "{out}");

        let out = clean(r#"<blockquote cite="https://source.example/a">x</blockquote>"#);
        assert!(out.contains(r#"cite="https://source.example/a""#), "{out}");
    }

    // --- behaviour ---------------------------------------------------------

    #[test]
    fn sanitizing_is_idempotent() {
        let body =
            r#"<p>Text with <a href="/rel">a link</a> and <img src="https://x/a.png" alt="a"></p>"#;
        let once = clean(body);
        let twice = clean(&once);
        assert_eq!(once, twice, "a second pass must not change the result");
    }

    #[test]
    fn empty_and_whitespace_input_yields_empty() {
        assert_eq!(clean(""), "");
        assert_eq!(clean("   \n  "), "");
    }

    #[test]
    fn the_output_is_always_balanced_markup() {
        // Unclosed tags are closed rather than left dangling, because the result goes into a DOM and
        // an unbalanced `<b>` would swallow everything after the article.
        let out = clean("<p>unclosed <b>bold");
        assert_eq!(out, "<p>unclosed <b>bold</b></p>");
        assert_eq!(out.matches("<p>").count(), out.matches("</p>").count());
        assert_eq!(out.matches("<b>").count(), out.matches("</b>").count());
    }

    #[test]
    fn deeply_nested_input_does_not_overflow_the_stack() {
        let depth = 5_000;
        let body = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
        let out = sanitize_html(&body, &opts());
        assert!(
            out.contains('x'),
            "content survives {depth} levels of nesting"
        );
    }

    #[test]
    fn invalid_utf8_cannot_reach_the_sanitizer() {
        // Callers hand us a `&str` from the parser, which already validated UTF-8; this asserts the
        // sanitizer is total over arbitrary valid strings rather than assuming well-formed input.
        let out = clean("<p>\u{fffd}\u{0}null\u{7f}</p>");
        assert!(out.contains("<p>"), "{out}");
    }

    #[test]
    fn a_huge_body_is_handled_without_a_stack_overflow() {
        let body = format!("<p>{}</p>", "a".repeat(1_000_000));
        let out = sanitize_html(&body, &opts());
        assert!(out.len() > 1_000_000, "output length {}", out.len());
    }

    // --- byte budget -------------------------------------------------------

    fn with_budget(max_length: usize) -> SanitizeOptions {
        SanitizeOptions {
            max_length: Some(max_length),
            ..opts()
        }
    }

    #[test]
    fn the_result_fits_the_budget() {
        let body = format!("<p>{}</p>", "word ".repeat(500));
        for budget in [10, 100, 1_000, 5_000] {
            let out = sanitize_html(&body, &with_budget(budget));
            assert!(out.len() <= budget, "budget {budget}, got {}", out.len());
        }
    }

    #[test]
    fn growth_from_balancing_tags_is_accounted_for() {
        // Sanitizing this adds a closing `</p>` and `</b>`, so a naive single cut overshoots.
        let body = "<p>a<p>b<p>c<p>d<p>e<p>f<p>g<p>h";
        let out = sanitize_html(body, &with_budget(30));
        assert!(out.len() <= 30, "{out:?} is {} bytes", out.len());
    }

    #[test]
    fn an_image_only_body_is_kept_not_blanked() {
        // The deliberate divergence from FreshRSS, which falls back to plain text here and returns "".
        let out = sanitize_html(r#"<img src="https://x/comic.png">"#, &with_budget(500));
        assert!(out.contains("comic.png"), "{out:?}");
    }

    #[test]
    fn a_body_already_within_budget_is_untouched() {
        let body = "<p>short</p>";
        assert_eq!(sanitize_html(body, &with_budget(1_000)), body);
    }

    #[test]
    fn a_zero_budget_means_unlimited_not_empty() {
        // `max_length: Some(0)` reads as "no limit was configured", matching FreshRSS's
        // `$maxLength <= 0` early return — a zero cap would silently blank every article.
        let out = sanitize_html("<p>text</p>", &with_budget(0));
        assert_eq!(out, "<p>text</p>");
    }

    #[test]
    fn the_cut_never_leaves_a_partial_tag() {
        let body = r#"<p>hello <img src="https://example.com/a.png" alt="x"></p>"#;
        for budget in 1..body.len() {
            let out = sanitize_html(body, &with_budget(budget));
            assert!(out.len() <= budget, "budget {budget}: {out:?}");
            assert!(
                !out.ends_with('<'),
                "budget {budget} left a bare '<': {out:?}"
            );
            assert!(
                std::str::from_utf8(out.as_bytes()).is_ok(),
                "budget {budget} split a character: {out:?}"
            );
        }
    }

    #[test]
    fn the_loop_converges_when_sanitizing_adds_closing_tags() {
        // Balancing `<p>` makes the output longer than the input, so the first cut overshoots and
        // the loop has to measure the overflow and try a shorter prefix.
        let body = format!("<p>{}</p>", "x".repeat(5_000));
        let out = sanitize_html(&body, &with_budget(20));
        assert!(out.len() <= 20, "{out:?}");
        assert_eq!(out, "<p>xxxxxxxxxxxx</p>", "converged on a balanced prefix");
    }

    #[test]
    fn an_unreachable_budget_falls_back_to_plain_text() {
        // A single 5 000-byte attribute value cannot be cut at a tag boundary without dropping the
        // element entirely, so no prefix fits and the fallback has to end the loop.
        let body = format!(
            r#"<p><img alt="{}" src="https://x/a.png"></p>"#,
            "y".repeat(5_000)
        );
        let out = sanitize_html(&body, &with_budget(30));
        assert!(out.len() <= 30, "{out:?} is {} bytes", out.len());
    }
}
