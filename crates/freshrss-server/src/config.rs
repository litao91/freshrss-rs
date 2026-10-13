//! Application configuration.
//!
//! Layered with figment: compiled-in defaults, then `config.toml` (or `$FRESHRSS_CONFIG`),
//! then environment variables of the form `FRESHRSS_<SECTION>_<KEY>`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use figment::providers::Format;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Port of FreshRSS's `config-user.default.php` `archiving` block. Resolved per entry
/// deletion as: feed attribute → category attribute → user config → these defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchivingOptions {
    /// ISO-8601 duration, e.g. `P3M`. `None` disables the age bound.
    pub keep_period: Option<String>,
    /// Keep at most this many most-recently-seen articles. `None` disables.
    pub keep_max: Option<u32>,
    /// Always keep at least this many most-recently-seen articles. `None` disables.
    pub keep_min: Option<u32>,
    pub keep_favourites: bool,
    pub keep_labels: bool,
    pub keep_unreads: bool,
}

impl Default for ArchivingOptions {
    fn default() -> Self {
        Self {
            keep_period: Some("P3M".into()),
            keep_max: Some(200),
            keep_min: Some(50),
            keep_favourites: true,
            keep_labels: true,
            keep_unreads: false,
        }
    }
}

/// Port of FreshRSS's `mark_when` user settings: conditions under which an article is
/// marked read without the user asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkWhen {
    /// `read_upon_reception`: new articles arrive already read.
    pub reception: bool,
    /// `read_upon_gone`: an article that disappeared from the feed is marked read.
    pub gone: bool,
    /// `keep_max_n_unread`: cap the unread count per feed.
    pub max_n_unread: Option<u32>,
    /// `read_when_same_title_in_feed`.
    pub same_title_in_feed: bool,
    /// `read_when_same_title_in_category`.
    pub same_title_in_category: bool,
    /// `read_when_same_guid_in_category`.
    pub same_guid_in_category: bool,
    /// `mark_updated_article_unread`: an upstream edit clears the read flag.
    pub updated_unread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub static_dir: PathBuf,
    /// Origins trusted in addition to the request's own `Host`, for the cross-origin check.
    ///
    /// Exists for one case: a development proxy. `npm run dev` serves the SPA on its own port and
    /// forwards `/api`, so the browser's `Origin` names the proxy rather than this server, and
    /// without listing it every mutation in development would be refused. Leave it empty in
    /// production — an origin listed here can make authenticated requests as though it were us.
    pub allowed_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080"
                .parse()
                .expect("valid default bind address"),
            static_dir: PathBuf::from("static"),
            allowed_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub path: PathBuf,
    pub pool_size: u32,
    pub busy_timeout_ms: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("data/freshrss.sqlite"),
            pool_size: 8,
            busy_timeout_ms: 10_000,
        }
    }
}

pub const DEFAULT_SESSION_SECRET: &str = "change-me";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub secret: String,
    pub cookie_name: String,
    pub max_age_days: u32,
    pub allow_insecure_secret: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            secret: DEFAULT_SESSION_SECRET.into(),
            cookie_name: "frss_sid".into(),
            max_age_days: 30,
            allow_insecure_secret: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationMode {
    /// Anyone may register.
    Open,
    /// Registration closes once the first account exists.
    FirstUserOnly,
    /// Accounts are created with the `create-user` CLI subcommand only.
    #[default]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistrationConfig {
    pub mode: RegistrationMode,
    pub first_user_is_admin: bool,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        Self {
            mode: RegistrationMode::default(),
            first_user_is_admin: true,
        }
    }
}

/// Port of FreshRSS's `limits` system-config block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_search_length: usize,
    pub max_search_parentheses_depth: usize,
    pub timeout_secs: u64,
    pub max_feeds_per_refresh: usize,
    pub refresh_concurrency: usize,
    pub scheduler_tick_secs: u64,
    pub ttl_default: i64,
    pub max_body_bytes: usize,
    pub max_page_size: usize,
    /// Per-minute ceiling on each metered class of request (login, register, anything that makes
    /// the server fetch, whole-tree refreshes), keyed per session or per address.
    pub rate_limit_per_minute: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_search_length: 16_384,
            max_search_parentheses_depth: 32,
            timeout_secs: 20,
            max_feeds_per_refresh: 10,
            refresh_concurrency: 4,
            scheduler_tick_secs: 60,
            ttl_default: 3600,
            max_body_bytes: 50 * 1024 * 1024,
            max_page_size: 500,
            rate_limit_per_minute: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub user_agent: String,
    /// Refuse to fetch URLs resolving to loopback / RFC1918 / link-local addresses.
    pub block_private_networks: bool,
    /// Hostnames, single addresses or CIDR networks exempt from
    /// [`Self::block_private_networks`], for a feed that genuinely lives on the LAN.
    pub internal_host_allowlist: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: concat!("freshrss-rs/", env!("CARGO_PKG_VERSION")).into(),
            block_private_networks: true,
            internal_host_allowlist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Compact,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub session: SessionConfig,
    pub registration: RegistrationConfig,
    pub limits: LimitsConfig,
    pub archiving: ArchivingOptions,
    pub mark_when: MarkWhen,
    pub http: HttpConfig,
    pub log: LogConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            // Development-friendly default; `load` refuses to start with this secret
            // unless `allow_insecure_secret` is explicitly set.
            session: SessionConfig {
                allow_insecure_secret: true,
                ..SessionConfig::default()
            },
            registration: RegistrationConfig::default(),
            limits: LimitsConfig::default(),
            archiving: ArchivingOptions::default(),
            mark_when: MarkWhen::default(),
            http: HttpConfig::default(),
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    /// Boxed because `figment::Error` is ~200 bytes, and this type is the payload of
    /// `Error::Config`; an unboxed variant would make every `Result` in the crate carry it.
    #[error("invalid configuration: {0}")]
    Invalid(#[source] Box<figment::Error>),
    #[error("configuration file not found: {}", .0.display())]
    FileNotFound(PathBuf),
    #[error(
        "session.secret is still the shipped default; set a secret or \
         session.allow_insecure_secret = true to proceed anyway"
    )]
    InsecureSecret,
}

impl From<figment::Error> for ConfigError {
    fn from(err: figment::Error) -> Self {
        ConfigError::Invalid(Box::new(err))
    }
}

impl AppConfig {
    /// Loads defaults → `config.toml` → environment, then validates.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(
            std::env::var("FRESHRSS_CONFIG")
                .ok()
                .as_deref()
                .map(Path::new),
        )
    }

    /// Same as [`Self::load`] with an explicit config path (`None` = `config.toml` if present).
    pub fn load_from(path: Option<&Path>) -> Result<Self, ConfigError> {
        let explicit = path.map(PathBuf::from);
        let candidate = explicit
            .clone()
            .unwrap_or_else(|| PathBuf::from("config.toml"));

        let mut fig = figment::Figment::from(figment::providers::Serialized::defaults(
            AppConfig::default(),
        ));
        if candidate.exists() {
            fig = fig.merge(figment::providers::Toml::file(&candidate));
        } else if let Some(p) = explicit {
            return Err(ConfigError::FileNotFound(p));
        }
        fig = fig.merge(
            figment::providers::Env::prefixed("FRESHRSS_")
                .split("_")
                .map(|k| k.as_str().replace("__", ".").into())
                // `FRESHRSS_CONFIG` says which file to read, and is consumed by clap and by
                // [`Self::load`] before this layer runs. Left in it, the variable arrives as a
                // `config` field that `AppConfig` does not have, and serde's `deny_unknown_fields`
                // turns the documented way of pointing at a config file into a startup failure.
                .ignore(&["config"]),
        );

        let cfg: AppConfig = fig.extract()?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.session.secret == DEFAULT_SESSION_SECRET && !self.session.allow_insecure_secret {
            return Err(ConfigError::InsecureSecret);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Serialized, Toml};
    use figment::Figment;

    #[test]
    fn defaults_apply_when_file_is_absent() {
        let cfg: AppConfig = Figment::from(Serialized::defaults(AppConfig::default()))
            .extract()
            .expect("defaults must be extractable");
        assert_eq!(cfg.limits.ttl_default, 3600);
        assert_eq!(cfg.limits.max_search_parentheses_depth, 32);
        assert_eq!(cfg.archiving.keep_period.as_deref(), Some("P3M"));
        assert_eq!(cfg.archiving.keep_max, Some(200));
        assert_eq!(cfg.session.cookie_name, "frss_sid");
    }

    #[test]
    fn toml_overrides_defaults_section_by_section() {
        let cfg: AppConfig = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(
                r#"
                [server]
                bind = "0.0.0.0:9000"
                [limits]
                ttl_default = 900
                [archiving]
                keep_max = 50
                keep_unreads = true
                "#,
            ))
            .extract()
            .expect("partial toml must merge over defaults");
        assert_eq!(cfg.server.bind.port(), 9000);
        assert_eq!(cfg.limits.ttl_default, 900);
        // untouched siblings keep their defaults
        assert_eq!(cfg.limits.max_page_size, 500);
        assert_eq!(cfg.archiving.keep_max, Some(50));
        assert!(cfg.archiving.keep_unreads);
        assert_eq!(cfg.archiving.keep_min, Some(50));
    }

    #[test]
    fn rejects_shipped_secret_without_opt_in() {
        let cfg = AppConfig {
            session: SessionConfig {
                allow_insecure_secret: false,
                ..SessionConfig::default()
            },
            ..AppConfig::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::InsecureSecret)));
    }

    #[test]
    fn registration_modes_parse_from_kebab_case() {
        for (raw, want) in [
            ("open", RegistrationMode::Open),
            ("first-user-only", RegistrationMode::FirstUserOnly),
            ("closed", RegistrationMode::Closed),
        ] {
            let cfg: AppConfig = Figment::from(Serialized::defaults(AppConfig::default()))
                .merge(Toml::string(&format!("[registration]\nmode = \"{raw}\"\n")))
                .extract()
                .expect("valid mode");
            assert_eq!(cfg.registration.mode, want);
        }
    }
}
