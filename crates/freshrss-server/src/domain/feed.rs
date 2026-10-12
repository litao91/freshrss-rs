//! The `feed` row and the settings that drive its refresh.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::domain::attributes::{keys, Attributes};
use crate::domain::filter_action::FilterAction;
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;

pub const FEED_NAME_MAX_LEN: usize = 191;

#[derive(Debug, Clone, PartialEq)]
pub struct Feed {
    pub id: i64,
    pub id_user: i64,
    pub url: String,
    pub kind: FeedKind,
    /// `None` when the folder was deleted (`ON DELETE SET NULL`) or never assigned.
    pub category: Option<i64>,
    pub name: String,
    pub website: String,
    pub description: String,
    /// Unix time of the last successful refresh; 0 for a feed never fetched. A zero value is what
    /// marks a feed as new, which forces a full parse on the first refresh.
    pub last_update: i64,
    pub priority: Priority,
    /// CSS selector extracting the full article body from the entry's own page.
    pub path_entries: Option<String>,
    /// CSS selector removing boilerplate from scraped or feed-provided content.
    pub path_entries_filter: Option<String>,
    /// Base64 of `user:password`, as stored by FreshRSS's `FeedDAO`.
    pub http_auth: Option<String>,
    /// 0 when healthy, otherwise the unix time of the last failed refresh.
    pub error: i64,
    pub ttl: Ttl,
    pub attributes: Attributes,
    pub cache_nb_entries: i64,
    pub cache_nb_unreads: i64,
}

impl Feed {
    pub fn new(id_user: i64, url: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: 0,
            id_user,
            url: url.into(),
            kind: FeedKind::default(),
            category: None,
            name: name.into(),
            website: String::new(),
            description: String::new(),
            last_update: 0,
            priority: Priority::default(),
            path_entries: None,
            path_entries_filter: None,
            http_auth: None,
            error: 0,
            ttl: Ttl::default(),
            attributes: Attributes::new(),
            cache_nb_entries: 0,
            cache_nb_unreads: 0,
        }
    }

    /// The display name, falling back to the URL's host the way `FreshRSS_Feed::name()` does.
    ///
    /// A feed whose title has not been fetched yet still needs something readable in the sidebar,
    /// and `https://www.example.com/feed.xml` is worse than `example.com`.
    pub fn display_name(&self) -> &str {
        let trimmed = self.name.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
        host_of(&self.url)
    }

    pub fn is_muted(&self) -> bool {
        self.ttl.is_muted()
    }

    pub fn is_new(&self) -> bool {
        self.last_update <= 0
    }

    pub fn has_error(&self) -> bool {
        self.error > 0
    }

    /// The effective refresh interval, resolving a zero ttl against the owner's default.
    pub fn effective_ttl(&self, user_default: i64) -> i64 {
        self.ttl.effective_seconds(user_default)
    }

    // --- attribute-backed settings -----------------------------------------

    pub fn simplepie_hash(&self) -> Option<&str> {
        self.attributes.get_str(keys::SIMPLEPIE_HASH)
    }

    pub fn set_simplepie_hash(&mut self, hash: Option<&str>) {
        self.attributes
            .set_str(keys::SIMPLEPIE_HASH, hash.map(str::to_string));
    }

    /// The GUID policy override, when the user set one.
    pub fn unicity_criteria(&self) -> Option<&str> {
        self.attributes.get_str(keys::UNICITY_CRITERIA)
    }

    /// True when the user chose the GUID policy explicitly, so refresh must not silently
    /// auto-degrade it after seeing bad GUIDs.
    pub fn unicity_criteria_forced(&self) -> bool {
        self.attributes
            .get_bool(keys::UNICITY_CRITERIA_FORCED)
            .unwrap_or(false)
    }

    /// Per-feed HTTP timeout override, in seconds.
    pub fn timeout_secs(&self) -> Option<u64> {
        self.attributes
            .get_i64(keys::TIMEOUT)
            .filter(|t| *t > 0)
            .map(|t| t as u64)
    }

    /// Whether TLS verification is disabled for this feed.
    pub fn ssl_verify(&self) -> bool {
        self.attributes.get_bool(keys::SSL_VERIFY).unwrap_or(true)
    }

    pub fn feed_icon_url(&self) -> Option<&str> {
        self.attributes.get_str(keys::FEED_ICON_URL)
    }

    pub fn filter_actions(&self) -> Vec<FilterAction> {
        self.attributes
            .get_typed::<Vec<FilterAction>>(keys::FILTERS)
            .unwrap_or_default()
    }

    pub fn set_filter_actions(&mut self, mut rules: Vec<FilterAction>) {
        FilterAction::retain_valid(&mut rules);
        if rules.is_empty() {
            self.attributes.remove(keys::FILTERS);
        } else {
            // A feed with no usable rules stores nothing rather than `[]`.
            let _ = self.attributes.set_typed(keys::FILTERS, &rules);
        }
    }

    /// The XPath map for kinds 10/15/20/35.
    pub fn xpath(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.attributes.get_object(keys::XPATH)
    }

    /// The dot-notation map for kinds 30/35.
    pub fn json_dotnotation(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.attributes.get_object(keys::JSON_DOTNOTATION)
    }

    // --- http auth ----------------------------------------------------------

    /// Stores credentials, base64-encoded as FreshRSS's DAO does.
    ///
    /// This is *encoding*, not encryption — it only keeps credentials out of plain sight in a
    /// database dump. Anyone with the file can decode it; treat the database as a secret.
    pub fn set_http_auth(&mut self, username: Option<&str>, password: Option<&str>) {
        match (username, password) {
            (Some(user), Some(pass)) if !user.is_empty() => {
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
                self.http_auth = Some(encoded);
            }
            _ => self.http_auth = None,
        }
    }

    /// Decodes the stored credentials.
    pub fn http_auth_credentials(&self) -> Option<(String, String)> {
        let stored = self.http_auth.as_deref()?.trim();
        if stored.is_empty() {
            return None;
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(stored)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())?;
        // Split on the first colon only, so a password containing ':' survives.
        let (user, pass) = decoded.split_once(':')?;
        Some((user.to_string(), pass.to_string()))
    }

    /// The URL with any embedded credentials removed, for display and logging.
    pub fn url_without_credentials(&self) -> String {
        url_without_credentials(&self.url)
    }
}

/// The host part of a URL, or the whole string if it does not parse. Used for display fallbacks,
/// so a best-effort answer beats an error.
fn host_of(url: &str) -> &str {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    // Strip credentials and a port.
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    host.split_once(':').map(|(h, _)| h).unwrap_or(host)
}

/// Removes `user:pass@` from a URL. Mirrors `SimplePie\Misc::url_remove_credentials`.
pub fn url_without_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // The authority runs up to the first '/', '?' or '#'; everything after it is kept verbatim.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    // Split on the *last* '@' so a password containing one survives.
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{tail}"),
        None => url.to_string(),
    }
}

/// The letter and colour for a feed's `.rd-favicon--letter` avatar, derived from the name so the
/// result is stable across reloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LetterAvatar {
    pub letter: String,
    pub color: String,
}

impl LetterAvatar {
    pub fn for_name(name: &str) -> Self {
        let letter = name
            .chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_else(|| "?".into());

        // A small fixed palette keeps avatars distinguishable without competing with the theme's
        // accent colour. Picked by hashing the name so it never changes between renders.
        const PALETTE: [&str; 10] = [
            "#3a6ea5", "#6a4c93", "#19857b", "#a4453a", "#8a6d1f", "#4a6741", "#7a4a6b", "#2f5d7c",
            "#8c5a2b", "#515a6b",
        ];
        let mut hash: u64 = 5381;
        for byte in name.as_bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(u64::from(*byte));
        }
        let color = PALETTE[(hash % PALETTE.len() as u64) as usize].to_string();
        Self { letter, color }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> Feed {
        Feed::new(1, "https://example.com/feed.xml", "Example")
    }

    #[test]
    fn new_feed_defaults_match_the_schema() {
        let f = feed();
        assert_eq!(f.kind, FeedKind::Rss);
        assert_eq!(f.priority, Priority::MAIN_STREAM);
        assert_eq!(f.ttl, Ttl::DEFAULT);
        assert_eq!(f.error, 0);
        assert_eq!(f.last_update, 0);
        assert!(f.is_new());
        assert!(!f.is_muted());
        assert!(!f.has_error());
        assert!(f.attributes.is_empty());
    }

    #[test]
    fn display_name_falls_back_to_the_host() {
        assert_eq!(feed().display_name(), "Example");
        let mut f = feed();
        f.name = "   ".into();
        assert_eq!(f.display_name(), "example.com");
    }

    #[test]
    fn host_of_strips_scheme_path_port_and_credentials() {
        assert_eq!(host_of("https://example.com/feed.xml"), "example.com");
        assert_eq!(host_of("https://example.com:8080/a/b?c=1#d"), "example.com");
        assert_eq!(host_of("http://user:pass@example.com/f"), "example.com");
        assert_eq!(host_of("example.com"), "example.com");
        assert_eq!(host_of("not a url at all"), "not a url at all");
    }

    #[test]
    fn url_without_credentials_preserves_the_rest() {
        assert_eq!(
            url_without_credentials("https://user:pass@example.com/feed?a=1"),
            "https://example.com/feed?a=1"
        );
        assert_eq!(
            url_without_credentials("https://example.com/feed"),
            "https://example.com/feed",
            "a url with no credentials is unchanged"
        );
        // Only the last '@' in the authority splits, so a password containing '@' survives.
        assert_eq!(
            url_without_credentials("https://u:p@ss@example.com/f"),
            "https://example.com/f"
        );
    }

    #[test]
    fn muting_is_expressed_through_the_ttl_sign() {
        let mut f = feed();
        assert!(!f.is_muted());
        f.ttl = Ttl::new(3600, true);
        assert!(f.is_muted());
        assert_eq!(f.effective_ttl(900), 3600);
    }

    #[test]
    fn a_zero_ttl_defers_to_the_user_default() {
        assert_eq!(feed().effective_ttl(3600), 3600);
        let mut f = feed();
        f.ttl = Ttl::new(120, false);
        assert_eq!(f.effective_ttl(3600), 120);
    }

    #[test]
    fn simplepie_hash_round_trips() {
        let mut f = feed();
        assert_eq!(f.simplepie_hash(), None);
        f.set_simplepie_hash(Some("abc123"));
        assert_eq!(f.simplepie_hash(), Some("abc123"));
        f.set_simplepie_hash(None);
        assert_eq!(f.simplepie_hash(), None);
        assert!(
            f.attributes.is_empty(),
            "clearing the only key empties attributes"
        );
    }

    #[test]
    fn timeout_ignores_non_positive_values() {
        let mut f = feed();
        assert_eq!(f.timeout_secs(), None);
        f.attributes.set(keys::TIMEOUT, 45);
        assert_eq!(f.timeout_secs(), Some(45));
        f.attributes.set(keys::TIMEOUT, 0);
        assert_eq!(f.timeout_secs(), None, "0 means 'use the system default'");
        f.attributes.set(keys::TIMEOUT, -5);
        assert_eq!(f.timeout_secs(), None);
    }

    #[test]
    fn ssl_verify_defaults_to_true() {
        assert!(feed().ssl_verify());
        let mut f = feed();
        f.attributes.set(keys::SSL_VERIFY, false);
        assert!(!f.ssl_verify());
    }

    #[test]
    fn http_auth_round_trips_through_base64() {
        let mut f = feed();
        assert_eq!(f.http_auth_credentials(), None);

        f.set_http_auth(Some("alice"), Some("s3cret"));
        // Stored encoded, not in the clear.
        assert!(!f.http_auth.as_deref().unwrap().contains("s3cret"));
        assert_eq!(
            f.http_auth_credentials(),
            Some(("alice".into(), "s3cret".into()))
        );
    }

    #[test]
    fn http_auth_splits_on_the_first_colon_only() {
        let mut f = feed();
        f.set_http_auth(Some("alice"), Some("pa:ss:word"));
        assert_eq!(
            f.http_auth_credentials(),
            Some(("alice".into(), "pa:ss:word".into()))
        );
    }

    #[test]
    fn http_auth_is_cleared_when_either_part_is_missing() {
        let mut f = feed();
        f.set_http_auth(Some("alice"), Some("s3cret"));
        assert!(f.http_auth.is_some());
        f.set_http_auth(Some("alice"), None);
        assert!(f.http_auth.is_none());
        f.set_http_auth(Some(""), Some("s3cret"));
        assert!(f.http_auth.is_none(), "an empty username stores nothing");
    }

    #[test]
    fn malformed_http_auth_yields_none_rather_than_panicking() {
        let mut f = feed();
        f.http_auth = Some("not base64 !!!".into());
        assert_eq!(f.http_auth_credentials(), None);
        // Valid base64 but no colon separator.
        f.http_auth = Some(base64::engine::general_purpose::STANDARD.encode("nocolon"));
        assert_eq!(f.http_auth_credentials(), None);
    }

    #[test]
    fn invalid_filter_rules_are_dropped_on_write() {
        let mut f = feed();
        f.set_filter_actions(vec![
            FilterAction {
                search: "author:bot".into(),
                actions: vec![crate::domain::filter_action::FilterActionType::Read],
            },
            FilterAction {
                search: "".into(),
                actions: vec![],
            },
        ]);
        let stored = f.filter_actions();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].search, "author:bot");
    }

    #[test]
    fn clearing_every_filter_removes_the_key() {
        let mut f = feed();
        f.set_filter_actions(vec![FilterAction {
            search: "a".into(),
            actions: vec![crate::domain::filter_action::FilterActionType::Read],
        }]);
        assert!(f.attributes.contains(keys::FILTERS));
        f.set_filter_actions(vec![]);
        assert!(!f.attributes.contains(keys::FILTERS));
        assert!(f.attributes.is_empty());
    }

    #[test]
    fn letter_avatars_are_stable_and_non_empty() {
        let a = LetterAvatar::for_name("Ars Technica - All content");
        let b = LetterAvatar::for_name("Ars Technica - All content");
        assert_eq!(a, b, "the same name must always give the same avatar");
        assert_eq!(a.letter, "A");
        assert!(a.color.starts_with('#'));

        assert_eq!(LetterAvatar::for_name("1234").letter, "1");
        assert_eq!(LetterAvatar::for_name("").letter, "?");
        assert_eq!(LetterAvatar::for_name("  spaced").letter, "S");
    }

    #[test]
    fn letter_avatars_vary_across_names() {
        let colors: std::collections::HashSet<_> = (0..40)
            .map(|i| LetterAvatar::for_name(&format!("Feed {i}")).color)
            .collect();
        assert!(
            colors.len() > 1,
            "a single colour for every feed is not an avatar"
        );
    }
}
