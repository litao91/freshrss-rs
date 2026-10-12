//! Feed acquisition: parsing, sanitization, identity (GUID + hash), discovery and OPML.
//!
//! Everything that turns feed bytes into the `RawFeed` intermediate representation lives here.
//! The crate is deliberately free of database and web concerns so it can be exercised entirely
//! from fixtures.

pub mod entities;
pub mod error;
pub mod guid;
pub mod hash;
pub mod kind;
pub mod raw;
pub mod text;

pub use entities::{encode_specials, html_only_entity_decode};
pub use error::FeedError;
pub use guid::{decide_guid, load_guids, GuidInput, GuidResolution, UnicityCriteria};
pub use hash::{content_hash, content_hash_hex, HashInput};
pub use kind::FeedKind;
pub use raw::{FeedFormat, RawEnclosure, RawFeed, RawItem, RawThumbnail};
