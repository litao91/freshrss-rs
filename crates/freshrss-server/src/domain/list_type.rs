//! The listing scopes — FreshRSS's `$type` parameter, the single letter that decides which articles a
//! view shows.
//!
//! The letters are kept as the canonical spelling because they appear in saved state (`user_query.get_type`,
//! a user's stored view preference) and in FreshRSS's own URLs, so an import has somewhere to land. The
//! reader talks in the longer names; [`ListType::parse`] accepts both.
//!
//! # The priority floor
//!
//! Most scopes are not "everything of this kind" but "everything of this kind that is visible enough".
//! The floor is `min(the scope's own base, what the search itself demands)`, and the second term is
//! what makes `f:12` work: naming a hidden feed in a search has to lift that feed into the main stream,
//! or the query would be unsatisfiable by construction. [`crate::domain::Priority::floor`] is that
//! `min()`, and [`freshrss_search::min_priority`] supplies the demand.

use serde::{Deserialize, Serialize};

use crate::domain::priority::Priority;

/// Which articles a listing admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ListType {
    /// `a` — the main stream: every feed at [`Priority::MAIN_STREAM`] or above. The default.
    #[serde(rename = "main")]
    #[default]
    MainStream,
    /// `A` — everything except feeds explicitly hidden from the main stream.
    #[serde(rename = "all")]
    AllButHidden,
    /// `Z` — everything, hidden feeds included.
    #[serde(rename = "hidden")]
    IncludingHidden,
    /// `i` — only feeds marked important.
    #[serde(rename = "important")]
    Important,
    /// `S` — only starred articles, from any feed at any priority.
    #[serde(rename = "starred")]
    Starred,
    /// `c` — one folder, plus every feed in it that is visible enough.
    #[serde(rename = "category")]
    Category,
    /// `f` — one feed, whatever its priority. Naming a feed is explicit, so no floor applies.
    #[serde(rename = "feed")]
    Feed,
    /// `t` — one label.
    #[serde(rename = "tag")]
    Tag,
    /// `T` — any label at all. Needs `DISTINCT` (or `GROUP BY` under a random sort) because joining
    /// `entrytag` yields one row per label.
    #[serde(rename = "any_tag")]
    AnyTag,
    /// `ST` — starred or labelled: the "things I have touched" view.
    #[serde(rename = "starred_or_tagged")]
    StarredOrTagged,
}

impl ListType {
    /// The canonical single letter.
    pub fn as_char(self) -> &'static str {
        match self {
            ListType::MainStream => "a",
            ListType::AllButHidden => "A",
            ListType::IncludingHidden => "Z",
            ListType::Important => "i",
            ListType::Starred => "S",
            ListType::Category => "c",
            ListType::Feed => "f",
            ListType::Tag => "t",
            ListType::AnyTag => "T",
            ListType::StarredOrTagged => "ST",
        }
    }

    /// The reader's name for this scope, which is what the API accepts and returns.
    pub fn as_scope(self) -> &'static str {
        match self {
            ListType::MainStream => "main",
            ListType::AllButHidden => "all",
            ListType::IncludingHidden => "hidden",
            ListType::Important => "important",
            ListType::Starred => "starred",
            ListType::Category => "category",
            ListType::Feed => "feed",
            ListType::Tag => "tag",
            ListType::AnyTag => "any_tag",
            ListType::StarredOrTagged => "starred_or_tagged",
        }
    }

    /// Accepts the scope name, the canonical letter, and FreshRSS's deprecated `'s'` for starred.
    ///
    /// Case matters for the letters — `A` and `a` are different scopes — so they are matched exactly,
    /// while the long names are folded because nobody means anything by `Main`.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "a" => ListType::MainStream,
            "A" => ListType::AllButHidden,
            "Z" | "z" => ListType::IncludingHidden,
            "i" => ListType::Important,
            // `'s'` is FreshRSS's deprecated starred scope. It differs from `'S'` only in also
            // excluding the most-hidden feeds, which is not a distinction worth carrying: the reader
            // filters visibility through the priority floor, not through the starred view.
            "S" | "s" => ListType::Starred,
            "c" => ListType::Category,
            "f" => ListType::Feed,
            "t" => ListType::Tag,
            "T" => ListType::AnyTag,
            "ST" | "st" => ListType::StarredOrTagged,
            _ => Self::ALL
                .iter()
                .copied()
                .find(|t| t.as_scope().eq_ignore_ascii_case(text))?,
        })
    }

    /// Every scope, for validation errors that list what is allowed.
    pub const ALL: &'static [ListType] = &[
        ListType::MainStream,
        ListType::AllButHidden,
        ListType::IncludingHidden,
        ListType::Important,
        ListType::Starred,
        ListType::Category,
        ListType::Feed,
        ListType::Tag,
        ListType::AnyTag,
        ListType::StarredOrTagged,
    ];

    /// True when this scope needs an `id` (a category, feed or tag) to mean anything.
    pub fn needs_id(self) -> bool {
        matches!(
            self,
            ListType::Category | ListType::Feed | ListType::Tag
        )
    }

    /// True when the scope joins `entrytag`, which multiplies rows for an article with several labels.
    pub fn joins_entrytag(self) -> bool {
        matches!(self, ListType::Tag | ListType::AnyTag)
    }

    /// True when the id-select has to be `DISTINCT`, because [`Self::joins_entrytag`] can return the
    /// same article once per label.
    ///
    /// [`ListType::Tag`] is excluded: filtering on one specific `id_tag` already yields at most one
    /// row per article.
    pub fn needs_distinct(self) -> bool {
        matches!(self, ListType::AnyTag)
    }

    /// The priority floor for this scope, given what the search demands.
    ///
    /// `None` means the scope puts no constraint on priority at all, so no `f.priority` predicate is
    /// emitted.
    pub fn priority_floor(self, visibility: Option<Priority>) -> Option<Priority> {
        let base = match self {
            ListType::MainStream => Priority::MAIN_STREAM,
            // `A` and `c` both mean "anything a person would still want to see", which is everything
            // above the hidden line.
            ListType::AllButHidden | ListType::Category => Priority::CATEGORY,
            ListType::Important => Priority::IMPORTANT,
            // No floor: these scopes name what they want explicitly, or want everything.
            ListType::IncludingHidden
            | ListType::Starred
            | ListType::Feed
            | ListType::Tag
            | ListType::AnyTag
            | ListType::StarredOrTagged => return None,
        };
        Some(Priority::floor(base, visibility))
    }

    /// True when the scope admits starred articles from feeds that would otherwise be filtered out.
    ///
    /// Used by the counts aggregate, which has to know that the starred total is not a subset of the
    /// main-stream total.
    pub fn ignores_priority(self) -> bool {
        self.priority_floor(None).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_round_trips_both_spellings() {
        for t in ListType::ALL {
            assert_eq!(ListType::parse(t.as_scope()), Some(*t), "{t:?} by name");
            assert_eq!(
                ListType::parse(t.as_char()).map(|p| p.as_char()),
                Some(t.as_char()),
                "{t:?} by letter"
            );
        }
    }

    #[test]
    fn the_letters_are_case_sensitive_where_it_matters() {
        // `a` is the main stream and `A` is everything but hidden; folding them would silently widen
        // or narrow the reader's default view.
        assert_eq!(ListType::parse("a"), Some(ListType::MainStream));
        assert_eq!(ListType::parse("A"), Some(ListType::AllButHidden));
        assert_eq!(ListType::parse("t"), Some(ListType::Tag));
        assert_eq!(ListType::parse("T"), Some(ListType::AnyTag));
        // The long names are not case-sensitive, because nobody means anything by the difference.
        assert_eq!(ListType::parse("MAIN"), Some(ListType::MainStream));
        assert_eq!(ListType::parse("Any_Tag"), Some(ListType::AnyTag));
    }

    #[test]
    fn the_deprecated_starred_letter_is_accepted() {
        assert_eq!(ListType::parse("s"), Some(ListType::Starred));
        assert_eq!(ListType::parse("S"), Some(ListType::Starred));
        assert_eq!(ListType::parse("st"), Some(ListType::StarredOrTagged));
    }

    #[test]
    fn an_unknown_scope_is_none() {
        assert_eq!(ListType::parse("Q"), None);
        assert_eq!(ListType::parse("everything"), None);
        assert_eq!(ListType::parse(""), None);
    }

    #[test]
    fn three_scopes_need_an_id() {
        for t in ListType::ALL {
            assert_eq!(
                t.needs_id(),
                matches!(t, ListType::Category | ListType::Feed | ListType::Tag),
                "{t:?}"
            );
        }
    }

    #[test]
    fn only_any_tag_needs_distinct() {
        assert!(ListType::AnyTag.joins_entrytag() && ListType::AnyTag.needs_distinct());
        // One specific label cannot produce two rows for the same article.
        assert!(ListType::Tag.joins_entrytag() && !ListType::Tag.needs_distinct());
        assert!(!ListType::MainStream.joins_entrytag());
        // `ST` reaches labels through an EXISTS rather than a join, so it does not multiply rows.
        assert!(!ListType::StarredOrTagged.joins_entrytag());
    }

    #[test]
    fn the_priority_floors_match_the_freshrss_ladder() {
        // With no visibility demand, `needVisibility() ?? IMPORTANT` makes the min() a no-op.
        assert_eq!(
            ListType::MainStream.priority_floor(None),
            Some(Priority::MAIN_STREAM)
        );
        assert_eq!(
            ListType::AllButHidden.priority_floor(None),
            Some(Priority::CATEGORY)
        );
        assert_eq!(
            ListType::Important.priority_floor(None),
            Some(Priority::IMPORTANT)
        );
        assert_eq!(ListType::IncludingHidden.priority_floor(None), None);
        assert_eq!(ListType::Starred.priority_floor(None), None);
        assert_eq!(ListType::Feed.priority_floor(None), None);
    }

    #[test]
    fn a_search_naming_a_hidden_feed_lifts_the_floor() {
        // The whole point of the min(): `f:12` on a hidden feed has to reach it, or the query the
        // user just wrote returns nothing by construction.
        assert_eq!(
            ListType::MainStream.priority_floor(Some(Priority::HIDDEN)),
            Some(Priority::HIDDEN)
        );
        assert_eq!(
            ListType::AllButHidden.priority_floor(Some(Priority::HIDDEN)),
            Some(Priority::HIDDEN)
        );
        // A demand *above* the base cannot raise the floor — that would hide the scope's own feeds.
        assert_eq!(
            ListType::MainStream.priority_floor(Some(Priority::IMPORTANT)),
            Some(Priority::MAIN_STREAM)
        );
    }

    #[test]
    fn ignoring_priority_and_having_no_floor_are_the_same_thing() {
        for t in ListType::ALL {
            assert_eq!(t.ignores_priority(), t.priority_floor(None).is_none(), "{t:?}");
        }
    }

    #[test]
    fn the_default_scope_is_the_main_stream() {
        assert_eq!(ListType::default(), ListType::MainStream);
    }

    #[test]
    fn scopes_serialize_as_their_long_names() {
        // Two spellings of the same enum would let a client send back what it was just given and get a
        // 400, so `as_scope()` and serde have to agree exactly.
        for t in ListType::ALL {
            let json = serde_json::to_string(t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_scope()), "{t:?}");
            assert_eq!(&serde_json::from_str::<ListType>(&json).unwrap(), t);
        }
    }
}
