//! Minimal cookie parsing and `Set-Cookie` construction.
//!
//! Hand-rolled rather than pulled from `tower-cookies`/`axum-extra`: the reader needs exactly three
//! operations on one cookie, and doing them here keeps the security-relevant attributes
//! (`HttpOnly`, `SameSite`, `Secure`, `Path`) visible in one place instead of spread across a
//! library's builder defaults.

/// Finds `name` in a `Cookie` request header value.
///
/// Cookie values are percent-decoded, since a value containing `;` or `,` must have been encoded by
/// whoever set it. Our own session id is hex and unaffected either way.
pub fn parse(header: &str, name: &str) -> Option<String> {
    for pair in header.split(';') {
        let mut parts = pair.trim().splitn(2, '=');
        let (key, value) = (parts.next()?, parts.next()?);
        if key.trim() != name {
            continue;
        }
        return Some(
            percent_encoding::percent_decode_str(value.trim())
                .decode_utf8_lossy()
                .into_owned(),
        );
    }
    None
}

/// Options for [`build`].
#[derive(Debug, Clone, Copy)]
pub enum SameSite {
    /// Only sent for same-site requests. Right for a session cookie on a reader with no
    /// cross-origin embedders; it is also the strongest CSRF defence available here.
    Lax,
    Strict,
    None,
}

impl SameSite {
    const fn header_value(self) -> &'static str {
        match self {
            SameSite::Lax => "Lax",
            SameSite::Strict => "Strict",
            SameSite::None => "None",
        }
    }
}

/// Builds a `Set-Cookie` header value.
///
/// `max_age_secs` of `None` produces a session cookie (deleted when the browser closes). A negative
/// value produces an immediate expiry, which is how [`removal`] works.
pub fn build(
    name: &str,
    value: &str,
    max_age_secs: Option<i64>,
    secure: bool,
    same_site: SameSite,
) -> String {
    let mut out = format!(
        "{name}={value}; Path=/; HttpOnly; SameSite={}",
        same_site.header_value()
    );
    if let Some(secs) = max_age_secs {
        out.push_str(&format!("; Max-Age={secs}"));
    }
    if secure {
        // `Secure` cannot be set unconditionally: browsers reject Secure cookies over plain HTTP,
        // which would break every localhost deployment.
        out.push_str("; Secure");
    }
    // `SameSite=None` is only honoured by browsers together with `Secure`.
    if matches!(same_site, SameSite::None) && !secure {
        tracing::warn!("SameSite=None without Secure will be rejected by browsers");
    }
    out
}

/// Builds a `Set-Cookie` that deletes the cookie.
pub fn removal(name: &str) -> String {
    format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_cookie() {
        assert_eq!(
            parse("frss_sid=abc123", "frss_sid").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn finds_the_right_cookie_among_several() {
        let header = "theme=dark; frss_sid=abc123; other=1";
        assert_eq!(parse(header, "frss_sid").as_deref(), Some("abc123"));
        assert_eq!(parse(header, "theme").as_deref(), Some("dark"));
    }

    #[test]
    fn tolerates_missing_whitespace_and_trailing_semicolons() {
        assert_eq!(parse("a=1;b=2;", "b").as_deref(), Some("2"));
        assert_eq!(parse("  a = 1 ", "a").as_deref(), Some("1"));
    }

    #[test]
    fn a_missing_cookie_is_none_not_an_empty_string() {
        assert_eq!(parse("other=1", "frss_sid"), None);
        assert_eq!(parse("", "frss_sid"), None);
    }

    #[test]
    fn does_not_match_a_prefixed_name() {
        // `frss_sid_backup` must not satisfy a lookup for `frss_sid`.
        assert_eq!(parse("frss_sid_backup=xyz", "frss_sid"), None);
    }

    #[test]
    fn a_cookie_with_no_value_yields_an_empty_string() {
        assert_eq!(parse("frss_sid=", "frss_sid").as_deref(), Some(""));
    }

    #[test]
    fn percent_decodes_values() {
        assert_eq!(
            parse("a=hello%20world", "a").as_deref(),
            Some("hello world")
        );
        assert_eq!(parse("a=x%3By", "a").as_deref(), Some("x;y"));
    }

    #[test]
    fn only_the_first_equals_sign_separates_name_from_value() {
        assert_eq!(parse("a=b=c=d", "a").as_deref(), Some("b=c=d"));
    }

    #[test]
    fn builds_a_hardened_session_cookie() {
        let cookie = build("frss_sid", "abc", Some(3600), true, SameSite::Strict);
        assert!(cookie.starts_with("frss_sid=abc;"));
        assert!(cookie.contains("Path=/"));
        assert!(
            cookie.contains("HttpOnly"),
            "a session cookie must never be script-readable"
        );
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn omits_secure_for_plain_http_deployments() {
        let cookie = build("frss_sid", "abc", Some(60), false, SameSite::Lax);
        assert!(
            !cookie.contains("Secure"),
            "browsers would reject it over http"
        );
        assert!(cookie.contains("SameSite=Lax"));
    }

    #[test]
    fn no_max_age_produces_a_session_cookie() {
        let cookie = build("frss_sid", "abc", None, true, SameSite::Lax);
        assert!(!cookie.contains("Max-Age"));
    }

    #[test]
    fn removal_expires_immediately() {
        let cookie = removal("frss_sid");
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.starts_with("frss_sid=;"));
    }

    #[test]
    fn a_built_cookie_can_be_parsed_back() {
        let built = build("frss_sid", "deadbeef", Some(60), true, SameSite::Lax);
        // Strip the attributes, leaving what a browser would send back.
        let sent = built.split(';').next().unwrap();
        assert_eq!(parse(sent, "frss_sid").as_deref(), Some("deadbeef"));
    }
}
