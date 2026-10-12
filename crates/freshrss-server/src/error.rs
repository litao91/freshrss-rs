//! The one error type the whole server crate speaks. Everything else converts into it, and
//! [`crate::api::error`] maps it onto an HTTP status plus the uniform JSON envelope.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Builds a 400-class validation error.
///
/// The domain validators return `&'static str` reasons so their messages can live next to the
/// rules they enforce; this adapts them to the crate error type at the API boundary.
pub fn validation(reason: impl Into<String>) -> Error {
    Error::Validation(reason.into())
}

/// The crate's error type.
///
/// The bulky source errors are boxed. `rusqlite::Error` alone is 64 bytes and `figment::Error`
/// pushes `ConfigError` past 200; because `Result<T, Error>` is the return type of every DAO,
/// service and handler, an unboxed variant makes *each* of those Results that size. Clippy reports
/// it once on the enum, but the cost is paid at every call site.
#[derive(Debug, Error)]
pub enum Error {
    // --- storage ---
    #[error("sqlite: {0}")]
    Sqlite(#[source] Box<rusqlite::Error>),
    #[error("connection pool exhausted: {0}")]
    Pool(#[source] Box<r2d2::Error>),
    #[error("blocking database task failed: {0}")]
    Join(#[source] Box<tokio::task::JoinError>),
    #[error("schema migration: {0}")]
    Migrate(#[source] Box<crate::db::migrate::MigrateError>),
    #[error("could not open {path}: {source}")]
    OpenDb {
        path: String,
        #[source]
        source: std::io::Error,
    },

    // --- auth ---
    #[error("not authenticated")]
    Unauthorized,
    #[error("invalid username or password")]
    BadCredentials,
    #[error("{0}")]
    Validation(String),
    #[error("registration is disabled")]
    RegistrationClosed,
    #[error("account disabled")]
    AccountDisabled,
    #[error("password hashing failed")]
    PasswordHash,

    // --- domain ---
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    Conflict(String),

    // --- feeds / refresh ---
    #[error("feed error: {0}")]
    Feed(#[source] Box<freshrss_feed::FeedError>),
    #[error("invalid feed url {url:?}: {reason}")]
    BadUrl { url: String, reason: String },
    #[error("http {status}: {message}")]
    Http { status: u16, message: String },

    // --- search ---
    #[error("invalid search query: {0}")]
    Search(#[source] Box<freshrss_search::SearchError>),

    // --- config / io / misc ---
    #[error("configuration: {0}")]
    Config(#[source] Box<crate::config::ConfigError>),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// True when the caller caused the problem (4xx) rather than the server (5xx).
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Error::Unauthorized
                | Error::BadCredentials
                | Error::Validation(_)
                | Error::RegistrationClosed
                | Error::AccountDisabled
                | Error::NotFound(_)
                | Error::Conflict(_)
                | Error::BadUrl { .. }
                | Error::Search(_)
        )
    }

    /// Short stable machine-readable code for the API error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Sqlite(_) | Error::Pool(_) | Error::Join(_) | Error::Migrate(_) => "db",
            Error::OpenDb { .. } => "db_open",
            Error::Unauthorized => "unauthorized",
            Error::BadCredentials => "bad_credentials",
            Error::Validation(_) => "validation",
            Error::RegistrationClosed => "registration_closed",
            Error::AccountDisabled => "account_disabled",
            Error::PasswordHash => "password_hash",
            Error::NotFound(_) => "not_found",
            Error::Conflict(_) => "conflict",
            Error::Feed(_) => "feed",
            Error::BadUrl { .. } => "bad_url",
            Error::Http { .. } => "http",
            Error::Search(_) => "bad_search",
            Error::Config(_) => "config",
            Error::Io(_) => "io",
            Error::Json(_) => "json",
            Error::Internal(_) => "internal",
        }
    }
}

// Boxing conversions, written by hand because thiserror's `#[from]` on a `Box<T>` field generates
// `From<Box<T>>` rather than `From<T>` — which would break every `?` on a rusqlite call.
macro_rules! box_from {
    ($from:ty => $variant:ident) => {
        impl From<$from> for Error {
            fn from(err: $from) -> Self {
                Error::$variant(Box::new(err))
            }
        }
    };
}

box_from!(rusqlite::Error => Sqlite);
box_from!(r2d2::Error => Pool);
box_from!(tokio::task::JoinError => Join);
box_from!(crate::db::migrate::MigrateError => Migrate);
box_from!(freshrss_feed::FeedError => Feed);
box_from!(freshrss_search::SearchError => Search);
box_from!(crate::config::ConfigError => Config);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_errors_are_classified() {
        assert!(Error::Unauthorized.is_client_error());
        assert!(Error::NotFound("feed 7".into()).is_client_error());
        assert!(Error::Validation("bad name".into()).is_client_error());
        assert!(!Error::Internal("boom".into()).is_client_error());
    }

    #[test]
    fn every_variant_has_a_code() {
        // Guards against a new variant silently falling through to "internal".
        let samples = [
            (Error::Unauthorized, "unauthorized"),
            (Error::NotFound("x".into()), "not_found"),
            (Error::Conflict("x".into()), "conflict"),
            (Error::Internal("x".into()), "internal"),
            (Error::BadCredentials, "bad_credentials"),
        ];
        for (err, want) in samples {
            assert_eq!(err.code(), want);
        }
    }
}
