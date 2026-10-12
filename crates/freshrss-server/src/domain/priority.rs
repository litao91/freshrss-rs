//! Feed priority — FreshRSS's `feed.priority` column.
//!
//! The value doubles as a visibility control, which is why the listing scopes compare against it
//! with `>=` rather than `=`. Confined to this module so the "negative means less visible"
//! ordering is never re-derived ad hoc in a `WHERE` clause.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Priority(pub i8);

impl Priority {
    /// Shown in the "important" stream as well as the main stream.
    pub const IMPORTANT: Self = Self(20);
    /// Default: shown in the main stream and everything above it.
    pub const MAIN_STREAM: Self = Self(10);
    /// Only visible when its category is browsed directly.
    pub const CATEGORY: Self = Self(0);
    /// Only visible when the feed itself is opened.
    pub const FEED: Self = Self(-5);
    /// Never listed; still reachable by a search that names the feed explicitly.
    /// FreshRSS calls this `PRIORITY_ARCHIVED` as well.
    pub const HIDDEN: Self = Self(-10);

    pub const fn new(value: i8) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i8 {
        self.0
    }

    /// The visibility floor for a scope, given the search's own visibility requirement.
    ///
    /// Ports FreshRSS's `min($scope_base, $search->needVisibility() ?? PRIORITY_IMPORTANT)`: a
    /// search that names feeds or categories explicitly widens the floor so hidden and
    /// category-only feeds become reachable — otherwise `f:12` could return nothing for a feed
    /// the user can see in their own sidebar.
    pub fn floor(scope_base: Priority, visibility: Option<Priority>) -> Priority {
        let want = visibility.unwrap_or(Self::IMPORTANT);
        if want.0 < scope_base.0 {
            want
        } else {
            scope_base
        }
    }

    pub const fn is_hidden(self) -> bool {
        self.0 <= Self::HIDDEN.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self::MAIN_STREAM
    }
}

impl From<i8> for Priority {
    fn from(v: i8) -> Self {
        Self(v)
    }
}

impl From<Priority> for i8 {
    fn from(p: Priority) -> Self {
        p.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_matches_visibility() {
        assert!(Priority::IMPORTANT > Priority::MAIN_STREAM);
        assert!(Priority::MAIN_STREAM > Priority::CATEGORY);
        assert!(Priority::CATEGORY > Priority::FEED);
        assert!(Priority::FEED > Priority::HIDDEN);
    }

    #[test]
    fn default_is_main_stream_matching_the_schema() {
        assert_eq!(Priority::default(), Priority::MAIN_STREAM);
        assert_eq!(Priority::default().get(), 10);
    }

    #[test]
    fn floor_is_the_scope_base_without_a_search() {
        // `needVisibility() ?? IMPORTANT` → min(10, 20) == 10.
        assert_eq!(
            Priority::floor(Priority::MAIN_STREAM, None),
            Priority::MAIN_STREAM
        );
        assert_eq!(
            Priority::floor(Priority::CATEGORY, None),
            Priority::CATEGORY
        );
        assert_eq!(
            Priority::floor(Priority::IMPORTANT, None),
            Priority::IMPORTANT
        );
    }

    #[test]
    fn a_search_naming_feeds_widens_the_floor_to_hidden() {
        // min(10, -10) == -10, so hidden feeds become searchable.
        assert_eq!(
            Priority::floor(Priority::MAIN_STREAM, Some(Priority::HIDDEN)),
            Priority::HIDDEN
        );
    }

    #[test]
    fn a_search_naming_categories_widens_only_to_category() {
        assert_eq!(
            Priority::floor(Priority::MAIN_STREAM, Some(Priority::CATEGORY)),
            Priority::CATEGORY
        );
    }

    #[test]
    fn floor_never_narrows_below_the_scope_base() {
        // min(0, 20) == 0 — an unimportant search must not hide category-visible feeds.
        assert_eq!(
            Priority::floor(Priority::CATEGORY, Some(Priority::IMPORTANT)),
            Priority::CATEGORY
        );
        assert_eq!(
            Priority::floor(Priority::HIDDEN, Some(Priority::IMPORTANT)),
            Priority::HIDDEN
        );
    }

    #[test]
    fn serializes_as_a_bare_number() {
        assert_eq!(serde_json::to_string(&Priority::IMPORTANT).unwrap(), "20");
        let back: Priority = serde_json::from_str("-10").unwrap();
        assert_eq!(back, Priority::HIDDEN);
    }
}
