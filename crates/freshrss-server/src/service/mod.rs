//! Services: the FreshRSS behaviours, expressed over the DAOs.
//!
//! Handlers call these rather than DAOs directly, so the rules (who may do what, in what order,
//! with which side effects) live in one place and are testable without an HTTP request.

pub mod auth;
pub mod discovery;
pub mod entries;
pub mod export;
pub mod favicon;
pub mod import;
pub mod ingest;
pub mod jobs;
pub mod packages;
pub mod refresh;
pub mod scheduler;
pub mod search;
pub mod subscription;
pub mod tree;
