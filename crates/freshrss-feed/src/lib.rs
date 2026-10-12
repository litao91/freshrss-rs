//! Feed acquisition: parsing, sanitization, identity (GUID + hash), discovery and OPML.
//!
//! Everything that turns feed bytes into the `RawFeed` intermediate representation lives here.
//! The crate is deliberately free of database and web concerns so it can be exercised entirely
//! from fixtures.

pub mod error;

pub use error::FeedError;
