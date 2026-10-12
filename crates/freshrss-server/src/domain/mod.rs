//! Domain value types and entities.
//!
//! Nothing in here touches SQL or HTTP: these are the shapes the DAOs read into and the services
//! reason about. Keeping them separate means the invariants (ttl sign-encoding, priority ordering,
//! entry-id scale, the tag and author column formats) are stated once and testable without a
//! database.

pub mod attributes;
pub mod category;
pub mod entry;
pub mod feed;
pub mod filter_action;
pub mod ids;
pub mod kind;
pub mod priority;
pub mod tag;
pub mod ttl;
pub mod user;

pub use attributes::{Attributes, keys as attribute_keys};
pub use category::{Category, CategoryKind, DEFAULT_CATEGORY_NAME};
pub use entry::{Enclosure, Entry, Thumbnail};
pub use feed::{Feed, LetterAvatar};
pub use filter_action::{FilterAction, FilterActionType};
pub use ids::{EntryId, MICROS_PER_SEC, now_micros, now_unix};
pub use kind::FeedKind;
pub use priority::Priority;
pub use tag::Tag;
pub use ttl::Ttl;
pub use user::{SortOrder, User, UserConfig};
