//! The `user` row and its per-user configuration.
//!
//! `User` deliberately does **not** implement `Serialize`: the password hash must reach the API
//! layer only through an explicit DTO, so leaking it requires writing new code rather than
//! forgetting to annotate a field.

use serde::{Deserialize, Serialize};

use crate::config::{ArchivingOptions, MarkWhen};
use crate::domain::attributes::Attributes;

/// Per-user settings, stored as JSON in `user.config`. Ports FreshRSS's
/// `config-user.default.php`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    /// Used when a feed's `ttl` is 0. FreshRSS's `ttl_default`.
    pub ttl_default: i64,
    /// Retention defaults, overridable per category and per feed.
    pub archiving: ArchivingOptions,
    /// Automatic mark-as-read policies.
    pub mark_when: MarkWhen,
    /// Default listing sort: `id`, `date`, `title`, `link`, `length`, `last_modified`,
    /// `last_user_modified`, `category`, `feed`, `rand`.
    pub sort: String,
    /// `asc` or `desc`.
    pub sort_order: SortOrder,
    /// Tie-breaker used when sorting by category or feed name.
    pub secondary_sort: String,
    pub secondary_sort_order: SortOrder,
    /// Global filter actions applied to every feed.
    pub filters: Vec<crate::domain::filter_action::FilterAction>,
    /// Entries per page.
    pub posts_per_page: u32,
    /// Interface language, matching the reader's `select#rd-lang`.
    pub language: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    #[default]
    Desc,
}

impl SortOrder {
    pub const fn sql(self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }

    pub const fn is_desc(self) -> bool {
        matches!(self, SortOrder::Desc)
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            ttl_default: 3600,
            archiving: ArchivingOptions::default(),
            mark_when: MarkWhen::default(),
            sort: "id".into(),
            sort_order: SortOrder::Desc,
            secondary_sort: "id".into(),
            secondary_sort_order: SortOrder::Desc,
            filters: Vec::new(),
            posts_per_page: 50,
            language: "en".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    /// An argon2id PHC string. Never serialized.
    pub password_hash: String,
    pub is_admin: bool,
    pub enabled: bool,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub config: UserConfig,
    pub attributes: Attributes,
}

/// Username rules. FreshRSS restricts to a conservative character set because the username
/// becomes a filesystem path component in its per-user layout; we keep the same rules so an
/// imported account still works and so nothing surprising can reach a log or a URL.
pub const USERNAME_MIN_LEN: usize = 2;
pub const USERNAME_MAX_LEN: usize = 64;
pub const PASSWORD_MIN_LEN: usize = 8;
pub const PASSWORD_MAX_LEN: usize = 512;

impl User {
    /// Validates a username, returning the reason it was rejected.
    pub fn validate_username(username: &str) -> Result<(), &'static str> {
        let len = username.chars().count();
        if !(USERNAME_MIN_LEN..=USERNAME_MAX_LEN).contains(&len) {
            return Err("username must be between 2 and 64 characters");
        }
        // Alphanumeric plus `-`, `_`, `.`; must start and end alphanumeric so `..` and a leading
        // or trailing dot are impossible.
        let bytes = username.as_bytes();
        let ok_edge = |b: u8| b.is_ascii_alphanumeric();
        if !ok_edge(bytes[0]) || !ok_edge(bytes[bytes.len() - 1]) {
            return Err("username must start and end with a letter or digit");
        }
        if username.contains("..") {
            return Err("username must not contain '..'");
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err("username may contain only letters, digits, '-', '_' and '.'");
        }
        Ok(())
    }

    pub fn validate_password(password: &str) -> Result<(), &'static str> {
        let len = password.chars().count();
        if len < PASSWORD_MIN_LEN {
            return Err("password must be at least 8 characters");
        }
        if len > PASSWORD_MAX_LEN {
            return Err("password must be at most 512 characters");
        }
        // A NUL would truncate the hash input in some PHC implementations.
        if password.contains('\0') {
            return Err("password must not contain a NUL byte");
        }
        Ok(())
    }

    /// Validates an email if one was supplied. Deliberately permissive: the address is only used
    /// for display and (later) password resets, never for routing decisions.
    pub fn validate_email(email: Option<&str>) -> Result<(), &'static str> {
        let Some(email) = email.filter(|e| !e.trim().is_empty()) else {
            return Ok(());
        };
        if email.chars().count() > 254 {
            return Err("email address is too long");
        }
        let (local, domain) = email
            .split_once('@')
            .ok_or("email address must contain '@'")?;
        if local.is_empty() || domain.is_empty() {
            return Err("email address must have text on both sides of '@'");
        }
        if !domain.contains('.') {
            return Err("email domain must contain a dot");
        }
        if email.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err("email address must not contain whitespace or control characters");
        }
        Ok(())
    }

    /// The initial letter used by the reader's `.rd-favicon--letter` avatar and the account menu.
    pub fn initial(&self) -> String {
        self.username
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_else(|| "?".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_freshrss_user_config() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.ttl_default, 3600);
        assert_eq!(cfg.archiving.keep_max, Some(200));
        assert_eq!(cfg.archiving.keep_min, Some(50));
        assert_eq!(cfg.archiving.keep_period.as_deref(), Some("P3M"));
        assert!(cfg.archiving.keep_favourites);
        assert!(cfg.archiving.keep_labels);
        assert!(!cfg.archiving.keep_unreads);
        assert_eq!(cfg.sort, "id");
        assert!(cfg.sort_order.is_desc());
        assert!(!cfg.mark_when.reception);
        assert!(!cfg.mark_when.gone);
    }

    #[test]
    fn config_round_trips_through_json_with_unknown_keys_dropped() {
        let cfg = UserConfig {
            ttl_default: 900,
            ..UserConfig::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: UserConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.ttl_default, 900);
        assert_eq!(back, cfg);
    }

    #[test]
    fn partial_json_falls_back_to_defaults() {
        let back: UserConfig = serde_json::from_str(r#"{"ttl_default":120}"#).expect("partial");
        assert_eq!(back.ttl_default, 120);
        assert_eq!(back.sort, "id", "unset fields take their defaults");
    }

    #[test]
    fn sort_order_serializes_lowercase_and_maps_to_sql() {
        assert_eq!(serde_json::to_string(&SortOrder::Asc).unwrap(), "\"asc\"");
        assert_eq!(SortOrder::Asc.sql(), "ASC");
        assert_eq!(SortOrder::Desc.sql(), "DESC");
        assert!(SortOrder::Desc.is_desc());
        assert!(!SortOrder::Asc.is_desc());
    }

    #[test]
    fn accepts_reasonable_usernames() {
        for name in ["al", "alice", "alice-smith", "a.b_c", "Litao912", "x1"] {
            assert!(
                User::validate_username(name).is_ok(),
                "{name} should be valid"
            );
        }
    }

    #[test]
    fn rejects_bad_usernames() {
        for name in [
            "",            // empty
            "a",           // too short
            ".alice",      // leading dot
            "alice.",      // trailing dot
            "../etc",      // traversal
            "al..ice",     // interior double dot
            "-alice",      // leading dash
            "alice smith", // space
            "alice!",      // punctuation
            "üser",        // non-ASCII
            "a".repeat(65).as_str(),
        ] {
            assert!(
                User::validate_username(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_a_64_character_username() {
        let name = "a".repeat(64);
        assert!(User::validate_username(&name).is_ok());
    }

    #[test]
    fn password_length_and_nul_are_enforced() {
        assert!(
            User::validate_password("1234567").is_err(),
            "7 chars is too short"
        );
        assert!(User::validate_password("12345678").is_ok());
        assert!(User::validate_password(&"p".repeat(512)).is_ok());
        assert!(User::validate_password(&"p".repeat(513)).is_err());
        assert!(User::validate_password("valid\0pass").is_err());
    }

    #[test]
    fn email_is_optional_and_loosely_validated() {
        assert!(User::validate_email(None).is_ok());
        assert!(User::validate_email(Some("")).is_ok());
        assert!(
            User::validate_email(Some("  ")).is_ok(),
            "blank means unset"
        );
        assert!(User::validate_email(Some("a@b.co")).is_ok());
        assert!(User::validate_email(Some("litao912002@hotmail.com")).is_ok());
        assert!(User::validate_email(Some("no-at-sign")).is_err());
        assert!(User::validate_email(Some("@b.co")).is_err());
        assert!(User::validate_email(Some("a@")).is_err());
        assert!(
            User::validate_email(Some("a@localhost")).is_err(),
            "needs a dot"
        );
        assert!(User::validate_email(Some("a b@c.co")).is_err());
    }

    #[test]
    fn initial_uppercases_the_first_character() {
        let user = User {
            id: 1,
            username: "alice".into(),
            email: None,
            password_hash: String::new(),
            is_admin: false,
            enabled: true,
            created_at: 0,
            last_seen_at: None,
            config: UserConfig::default(),
            attributes: Attributes::new(),
        };
        assert_eq!(user.initial(), "A");

        let mut litao = user.clone();
        litao.username = "litao9120021062".into();
        assert_eq!(litao.initial(), "L");

        let mut numeric = user.clone();
        numeric.username = "9lives".into();
        assert_eq!(numeric.initial(), "9");
    }
}
