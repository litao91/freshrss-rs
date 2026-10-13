//! freshrss-rs: a Rust port of the FreshRSS backend, serving a Feeddi-style reader UI.
//!
//! Layering: [`db`] owns SQLite access, `domain` holds the value types and invariants,
//! `service` implements the FreshRSS behaviours (refresh, retention, discovery, import),
//! and `api` maps those onto HTTP. [`freshrss_search`] and [`freshrss_feed`] are separate
//! crates so the query language and the feed parsers stay testable without a database.

pub mod api;
pub mod cli;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod middleware;
pub mod router;
pub mod service;
pub mod static_files;

pub use config::AppConfig;
pub use db::Db;
pub use error::{Error, Result};
