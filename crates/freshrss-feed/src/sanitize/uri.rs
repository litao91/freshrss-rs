//! URL scheme policy and the force-https rewrite.
//!
//! This is the part of the sanitizer that most has to be right. An allowlist of tags and attributes
//! stops `<script>`; it does nothing about `<a href="javascript:…">`, which is the attack that
//! actually works against a feed reader, because the reader is the one rendering the link.

/// A URL scheme decision for one attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeDecision {
    /// A schemeless (relative, absolute-path or fragment) reference. Safe, and absolutized later.
    Relative,
    /// An allowed scheme.
    Allowed,
    /// Rejected: the attribute must be dropped.
    Denied,
}

/// Schemes allowed on any URL attribute.
///
/// `mailto` is kept because newsletter-style feeds use it for real. `data` is *not* here — see
/// [`classify`], which allows it in exactly one place.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// The one `data:` URL the sanitizer tolerates.
///
/// Inline images are common in feeds that want to survive an image proxy going away, and
/// `data:image/…` cannot execute. Every other `data:` type can: `data:text/html` is a document, and
/// `data:image/svg+xml` may contain a `<script>`.
const DATA_IMAGE_PREFIX: &str = "data:image/";

/// SVG is refused even as a `data:image/`, because it is an image format that runs script.
const DATA_IMAGE_SVG: &str = "data:image/svg";

/// Classifies a URL for one `(element, attribute)` pair.
///
/// Fails closed: anything this function does not recognize as safe comes back [`SchemeDecision::Denied`].
pub fn classify(element: &str, attribute: &str, value: &str) -> SchemeDecision {
    let cleaned = strip_url_controls(value);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return SchemeDecision::Denied;
    }

    let Some(scheme) = scheme_of(trimmed) else {
        return SchemeDecision::Relative;
    };

    let scheme = scheme.to_ascii_lowercase();
    if ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return SchemeDecision::Allowed;
    }

    // `data:` is allowed on an image source only, and only for a non-SVG raster type.
    if scheme == "data" && matches!((element, attribute), ("img", "src") | ("image", "src")) {
        let lowered = trimmed.to_ascii_lowercase();
        let starts_with_image = lowered.starts_with(DATA_IMAGE_PREFIX);
        let is_svg = lowered.starts_with(DATA_IMAGE_SVG);
        return if starts_with_image && !is_svg {
            SchemeDecision::Allowed
        } else {
            SchemeDecision::Denied
        };
    }

    SchemeDecision::Denied
}

/// Removes the characters browsers ignore inside a URL.
///
/// `java&#9;script:alert(1)` arrives here as `java\tscript:alert(1)` once the HTML parser has
/// decoded the entity, and a browser reading that as a scheme strips the tab. Checking the scheme
/// before removing it would see `java` — no colon in the right place — and call the URL relative.
///
/// Only ASCII whitespace and C0/DEL controls are removed. Removing anything else would corrupt
/// legitimate URLs that contain percent-encoded bytes.
fn strip_url_controls(value: &str) -> String {
    if !value.bytes().any(|b| b.is_ascii_control()) {
        return value.to_owned();
    }
    value.chars().filter(|c| !c.is_ascii_control()).collect()
}

/// Returns the scheme if `url` has one, or `None` for a relative reference.
///
/// A scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"` and must appear before any `/`,
/// `?` or `#`. That ordering rule is what makes `/redirect?url=http://x` relative rather than
/// `http`-schemed, and `//host/path` (protocol-relative) relative too.
fn scheme_of(url: &str) -> Option<&str> {
    let bytes = url.as_bytes();
    let colon = bytes.iter().position(|&b| b == b':')?;

    // Anything that delimits a path before the colon means there is no scheme.
    if bytes[..colon]
        .iter()
        .any(|&b| matches!(b, b'/' | b'?' | b'#'))
    {
        return None;
    }
    if colon == 0 {
        return None;
    }

    let candidate = &url[..colon];
    let mut chars = candidate.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some(candidate)
}

/// Rewrites `http://` to `https://` for hosts on the force-https list.
///
/// Returns `None` when nothing changed, so the caller can skip allocating a new attribute value.
///
/// Matching covers the host and its subdomains: listing `example.com` also upgrades
/// `www.example.com`. Without that, a feed that links to the `www` host keeps a mixed-content
/// warning that breaks the whole page over TLS.
pub fn force_https(url: &str, domains: &[String]) -> Option<String> {
    if domains.is_empty() || !url.trim_start().starts_with("http://") {
        return None;
    }
    let host = host_of(url)?;
    let matched = domains.iter().any(|domain| {
        let domain = domain.trim().to_ascii_lowercase();
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    });
    if !matched {
        return None;
    }
    let trimmed = url.trim_start();
    Some(format!("https://{}", &trimmed["http://".len()..]))
}

/// The lowercased host of an absolute URL, without userinfo or port.
///
/// Userinfo is stripped because `http://example.com@evil.test/` has the host `evil.test`; taking
/// everything before the first `@` would read it as `example.com` and apply the wrong force-https
/// rule.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let without_userinfo = authority.rsplit('@').next().unwrap_or(authority);
    // Strip the port, and handle an IPv6 literal.
    let host = match without_userinfo.strip_prefix('[') {
        Some(rest) => rest.split(']').next().map(|h| format!("[{h}]"))?,
        None => without_userinfo
            .split(':')
            .next()
            .unwrap_or(without_userinfo)
            .to_owned(),
    };
    let host = host.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Resolves `url` against `base`, leaving anything unresolvable untouched.
///
/// A relative URL in an article body is resolved against the *feed's* base, never against the
/// reader's own origin: `<img src="/logo.png">` in an article must load from the publisher, and
/// loading it from us would both break the image and let a feed probe our own paths.
pub fn absolutize(url: &str, base: &str) -> String {
    if base.is_empty() {
        return url.to_owned();
    }
    let Ok(base) = url::Url::parse(base) else {
        return url.to_owned();
    };
    // A fragment-only reference must stay relative, or every in-article `#note` link would navigate
    // to the feed's own site instead of scrolling.
    if url.starts_with('#') {
        return url.to_owned();
    }
    match base.join(url) {
        Ok(resolved) => resolved.to_string(),
        Err(_) => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_and_mailto_are_allowed() {
        for url in [
            "http://example.com/a",
            "https://example.com/a",
            "HTTPS://example.com/a",
            "mailto:someone@example.com",
            "https://example.com/a?x=1&y=2#frag",
        ] {
            assert_eq!(classify("a", "href", url), SchemeDecision::Allowed, "{url}");
        }
    }

    #[test]
    fn relative_references_are_relative() {
        for url in [
            "/a/b",
            "a/b",
            "./a",
            "../a",
            "?q=1",
            "#frag",
            "//cdn.example/x.png",
        ] {
            assert_eq!(
                classify("a", "href", url),
                SchemeDecision::Relative,
                "{url}"
            );
        }
    }

    #[test]
    fn script_schemes_are_denied() {
        for url in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html,<script>alert(1)</script>",
            "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
            "file:///etc/passwd",
            "blob:https://example.com/uuid",
            "about:blank",
            "view-source:https://example.com",
            "chrome://settings",
            "moz-extension://abc/x.html",
        ] {
            assert_eq!(classify("a", "href", url), SchemeDecision::Denied, "{url}");
        }
    }

    #[test]
    fn an_obfuscated_script_scheme_is_still_denied() {
        // The HTML parser has already decoded entities and the browser strips controls from a URL,
        // so both of these must be recognized as `javascript:`.
        for url in [
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "java\rscript:alert(1)",
            "\u{0}javascript:alert(1)",
            "javascript\u{1}:alert(1)",
            "  javascript:alert(1)  ",
            "\tjavascript:alert(1)",
            "jAvaScRipT:alert(1)",
        ] {
            assert_eq!(
                classify("a", "href", url),
                SchemeDecision::Denied,
                "{url:?}"
            );
        }
    }

    #[test]
    fn a_control_character_cannot_smuggle_a_scheme_past_the_colon_rule() {
        // `http://x` with a newline in the middle is still http, and must not be read as relative.
        assert_eq!(
            classify("a", "href", "htt\np://x/"),
            SchemeDecision::Allowed
        );
        // …and the reverse: a path that merely contains a colon stays relative.
        assert_eq!(
            classify("a", "href", "/redirect?url=http://x"),
            SchemeDecision::Relative
        );
        assert_eq!(
            classify("a", "href", "a:b:c"),
            SchemeDecision::Denied,
            "an unknown scheme"
        );
    }

    #[test]
    fn data_images_are_allowed_only_on_an_image_source() {
        let png = "data:image/png;base64,iVBORw0KGgo=";
        assert_eq!(classify("img", "src", png), SchemeDecision::Allowed);
        assert_eq!(classify("image", "src", png), SchemeDecision::Allowed);

        // The same bytes anywhere else are refused.
        assert_eq!(classify("a", "href", png), SchemeDecision::Denied);
        assert_eq!(classify("iframe", "src", png), SchemeDecision::Denied);
        assert_eq!(classify("img", "longdesc", png), SchemeDecision::Denied);
        assert_eq!(classify("video", "src", png), SchemeDecision::Denied);
        assert_eq!(classify("source", "src", png), SchemeDecision::Denied);
    }

    #[test]
    fn data_svg_is_denied_because_svg_runs_script() {
        for url in [
            "data:image/svg+xml,<svg/>",
            "data:image/svg+xml;base64,PHN2Zy8+",
            "DATA:IMAGE/SVG+XML,<svg/>",
            "data:image/svg",
        ] {
            assert_eq!(classify("img", "src", url), SchemeDecision::Denied, "{url}");
        }
    }

    #[test]
    fn a_non_image_data_url_is_denied_even_on_an_image_source() {
        assert_eq!(
            classify("img", "src", "data:application/pdf,x"),
            SchemeDecision::Denied
        );
        assert_eq!(
            classify("img", "src", "data:text/html,<b>x</b>"),
            SchemeDecision::Denied
        );
        assert_eq!(classify("img", "src", "data:"), SchemeDecision::Denied);
        assert_eq!(
            classify("img", "src", "data:image/"),
            SchemeDecision::Allowed,
            "the prefix is the rule"
        );
    }

    #[test]
    fn an_empty_url_is_denied() {
        assert_eq!(classify("a", "href", ""), SchemeDecision::Denied);
        assert_eq!(classify("a", "href", "   "), SchemeDecision::Denied);
        assert_eq!(classify("img", "src", ""), SchemeDecision::Denied);
    }

    #[test]
    fn scheme_of_follows_the_rfc_ordering_rule() {
        assert_eq!(scheme_of("http://x"), Some("http"));
        assert_eq!(scheme_of("mailto:a@b"), Some("mailto"));
        assert_eq!(scheme_of("a+b-c.d:x"), Some("a+b-c.d"));
        assert_eq!(
            scheme_of("/a:b"),
            None,
            "a path segment may contain a colon"
        );
        assert_eq!(scheme_of("?a:b"), None);
        assert_eq!(scheme_of("#a:b"), None);
        assert_eq!(
            scheme_of("1http://x"),
            None,
            "a scheme must start with a letter"
        );
        assert_eq!(scheme_of(":x"), None);
        assert_eq!(scheme_of("no colon"), None);
        assert_eq!(scheme_of("http:/x"), Some("http"));
    }

    // --- force-https -------------------------------------------------------

    fn domains(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn force_https_upgrades_a_listed_domain() {
        let d = domains(&["example.com"]);
        assert_eq!(
            force_https("http://example.com/a", &d).as_deref(),
            Some("https://example.com/a")
        );
    }

    #[test]
    fn force_https_covers_subdomains_and_ignores_case() {
        let d = domains(&["EXAMPLE.com"]);
        assert_eq!(
            force_https("http://www.example.com/a", &d).as_deref(),
            Some("https://www.example.com/a")
        );
        assert_eq!(
            force_https("http://a.b.example.com/", &d).as_deref(),
            Some("https://a.b.example.com/")
        );
    }

    #[test]
    fn force_https_does_not_match_a_lookalike_domain() {
        let d = domains(&["example.com"]);
        // A suffix match on the string would wrongly upgrade both of these.
        assert_eq!(force_https("http://notexample.com/a", &d), None);
        assert_eq!(force_https("http://example.com.evil.test/a", &d), None);
    }

    #[test]
    fn force_https_leaves_https_and_other_hosts_alone() {
        let d = domains(&["example.com"]);
        assert_eq!(force_https("https://example.com/a", &d), None);
        assert_eq!(force_https("http://other.test/a", &d), None);
        assert_eq!(force_https("/relative", &d), None);
        assert_eq!(
            force_https("http://example.com/a", &[]),
            None,
            "an empty list changes nothing"
        );
    }

    #[test]
    fn force_https_matches_the_host_not_the_userinfo() {
        let d = domains(&["example.com"]);
        // The real host here is evil.test; a naive prefix match would upgrade it.
        assert_eq!(force_https("http://example.com@evil.test/a", &d), None);

        let d = domains(&["evil.test"]);
        assert_eq!(
            force_https("http://example.com@evil.test/a", &d).as_deref(),
            Some("https://example.com@evil.test/a")
        );
    }

    #[test]
    fn force_https_ignores_the_port() {
        let d = domains(&["example.com"]);
        assert_eq!(
            force_https("http://example.com:8080/a", &d).as_deref(),
            Some("https://example.com:8080/a")
        );
    }

    #[test]
    fn host_of_handles_ipv6_userinfo_and_ports() {
        assert_eq!(host_of("http://[::1]:8080/x").as_deref(), Some("[::1]"));
        assert_eq!(
            host_of("http://user:pw@Example.COM:80/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("http://example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://"), None);
    }

    // --- absolutization ----------------------------------------------------

    #[test]
    fn relative_urls_resolve_against_the_base() {
        let base = "https://example.com/blog/feed.xml";
        assert_eq!(
            absolutize("post.html", base),
            "https://example.com/blog/post.html"
        );
        assert_eq!(
            absolutize("/img/a.png", base),
            "https://example.com/img/a.png"
        );
        assert_eq!(absolutize("../up.png", base), "https://example.com/up.png");
        assert_eq!(
            absolutize("//cdn.example/x.png", base),
            "https://cdn.example/x.png"
        );
    }

    #[test]
    fn absolute_urls_are_left_alone() {
        let base = "https://example.com/feed.xml";
        assert_eq!(
            absolutize("https://other.test/a", base),
            "https://other.test/a"
        );
        assert_eq!(absolutize("mailto:a@b.c", base), "mailto:a@b.c");
    }

    #[test]
    fn a_fragment_reference_stays_relative() {
        // Rewriting `#note` to the publisher's site would navigate the reader away from the article.
        let base = "https://example.com/feed.xml";
        assert_eq!(absolutize("#note", base), "#note");
        assert_eq!(absolutize("#", base), "#");
    }

    #[test]
    fn an_empty_or_invalid_base_leaves_the_url_untouched() {
        assert_eq!(absolutize("/a", ""), "/a");
        assert_eq!(absolutize("/a", "not a url"), "/a");
    }

    #[test]
    fn absolutizing_never_produces_a_relative_against_our_own_origin() {
        // The base is always the feed's, so a root-relative path lands on the publisher's host.
        let base = "https://publisher.test/rss";
        let resolved = absolutize("/logo.png", base);
        assert!(
            resolved.starts_with("https://publisher.test/"),
            "{resolved}"
        );
    }
}
