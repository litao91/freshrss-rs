//! Services: the FreshRSS behaviours, expressed over the DAOs.
//!
//! Handlers call these rather than DAOs directly, so the rules (who may do what, in what order,
//! with which side effects) live in one place and are testable without an HTTP request.

pub mod auth;
pub mod entries;
pub mod ingest;
pub mod search;
pub mod tree;
