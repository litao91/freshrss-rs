//! The one error type the whole server crate speaks. Everything else converts into it, and
//! [`crate::api::error`] maps it onto an HTTP status plus the uniform JSON envelope.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // --- storage ---
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("connection pool exhausted: {0}")]
    Pool(#[from] r2d2::Error),
    #[error("blocking database task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("schema migration: {0}")]
    Migrate(#[from] crate::db::migrate::MigrateError),
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
    Feed(#[from] freshrss_feed::FeedError),
    #[error("invalid feed url {url:?}: {reason}")]
    BadUrl { url: String, reason: String },
    #[error("http {status}: {message}")]
    Http { status: u16, message: String },

    // --- search ---
    #[error("invalid search query: {0}")]
    Search(#[from] freshrss_search::SearchError),

    // --- config / io / misc ---
    #[error("configuration: {0}")]
    Config(#[from] crate::config::ConfigError),
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
