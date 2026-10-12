//! What a listing is ordered by, and in which direction.
//!
//! The order is not cosmetic: it is half of the pagination key. Every sort except [`Sort::Rand`] is
//! followed by `e.id` in the same direction, so `(sort, id)` is a total order and a cursor can resume
//! from it without skipping or repeating a row. That is why [`Sort::Rand`] is refused when a cursor is
//! presented rather than quietly returning the same page forever.

use serde::{Deserialize, Serialize};

/// The primary sort.
///
/// The wire names are the reader's; the FreshRSS spellings (`c.name`, `lastModified`) are accepted as
/// aliases so an imported preference or a hand-written saved query still parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Sort {
    /// Ingestion order — the entry id, which is a microsecond wall clock. The default.
    #[serde(rename = "id")]
    #[default]
    Id,
    /// Publication date, `entry.date`.
    #[serde(rename = "date")]
    Date,
    /// Folder name, which joins `category` and carries an internal secondary sort on feed name.
    #[serde(rename = "category")]
    CategoryName,
    /// Feed name, which joins `feed`.
    #[serde(rename = "feed")]
    FeedName,
    /// When the article was last seen changed upstream. Nullable, so the listing has to exclude NULLs
    /// or the ordering is undefined for them.
    #[serde(rename = "modified")]
    LastModified,
    /// When the reader last changed the article. Also nullable.
    #[serde(rename = "user_modified")]
    LastUserModified,
    /// Length of the body in characters.
    ///
    /// SQLite's `LENGTH()` counts characters, not bytes, so this is what a person means by "longest
    /// article". FreshRSS on MySQL gets bytes; the difference is only visible on non-ASCII bodies and
    /// the character count is the more useful one.
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "link")]
    Link,
    #[serde(rename = "title")]
    Title,
    /// Random. Not resumable.
    #[serde(rename = "rand")]
    Rand,
}

/// The tie-breaker the reader may choose, for the sorts where ties are common.
///
/// Only meaningful for [`Sort::CategoryName`] and [`Sort::FeedName`]; every other sort already ends in
/// `e.id`, which is unique and so cannot tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondarySort {
    #[default]
    Id,
    Date,
    Link,
    Title,
}

/// Ascending or descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    Asc,
    /// The default, because a reader wants the newest article first.
    #[default]
    Desc,
}

impl Order {
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "asc" | "ascending" | "up" => Some(Order::Asc),
            "desc" | "descending" | "down" => Some(Order::Desc),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        }
    }

    pub fn is_ascending(self) -> bool {
        matches!(self, Order::Asc)
    }

    pub fn reverse(self) -> Self {
        match self {
            Order::Asc => Order::Desc,
            Order::Desc => Order::Asc,
        }
    }

    /// The strict comparison this order resumes from: `>` going up, `<` going down.
    pub fn sign(self) -> &'static str {
        match self {
            Order::Asc => ">",
            Order::Desc => "<",
        }
    }
}

impl Sort {
    /// Accepts the reader's name and FreshRSS's spelling.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "id" => Sort::Id,
            "date" => Sort::Date,
            "category" | "c.name" | "category_name" => Sort::CategoryName,
            "feed" | "f.name" | "feed_name" => Sort::FeedName,
            "modified" | "lastModified" | "last_modified" | "mdate" => Sort::LastModified,
            "user_modified" | "lastUserModified" | "last_user_modified" | "userdate" => {
                Sort::LastUserModified
            }
            "length" | "size" => Sort::Length,
            "link" | "url" => Sort::Link,
            "title" => Sort::Title,
            "rand" | "random" => Sort::Rand,
            _ => return None,
        })
    }

    /// The wire name, which is what a cursor and a saved query store.
    pub fn as_str(self) -> &'static str {
        match self {
            Sort::Id => "id",
            Sort::Date => "date",
            Sort::CategoryName => "category",
            Sort::FeedName => "feed",
            Sort::LastModified => "modified",
            Sort::LastUserModified => "user_modified",
            Sort::Length => "length",
            Sort::Link => "link",
            Sort::Title => "title",
            Sort::Rand => "rand",
        }
    }

    /// True when the sort can resume from a cursor.
    ///
    /// [`Sort::Rand`] cannot: there is no value to compare against, so a second page would be drawn
    /// from a fresh shuffle and would overlap the first.
    pub fn is_resumable(self) -> bool {
        !matches!(self, Sort::Rand)
    }

    /// True when the sorted column can be NULL, so the listing has to say what to do with those rows.
    ///
    /// SQLite sorts NULLs first ascending and last descending, which is a valid order but not a useful
    /// one for "longest edited articles" — and it makes the cursor boundary meaningless, since no
    /// comparison against NULL is true. FreshRSS excludes them outright and so does this.
    pub fn column_is_nullable(self) -> bool {
        matches!(self, Sort::LastModified | Sort::LastUserModified)
    }

    /// How many boundary values a cursor has to carry.
    ///
    /// One per level of the sort ladder *below* the id: [`Sort::CategoryName`] orders by folder name,
    /// then feed name, then the reader's secondary sort, then the id — so three values, or two when the
    /// secondary sort is the id and the final rung already covers it.
    pub fn continuation_arity(self, secondary: SecondarySort) -> usize {
        let extra = usize::from(self.takes_secondary_sort() && secondary != SecondarySort::Id);
        match self {
            Sort::CategoryName => 2 + extra,
            Sort::FeedName => 1 + extra,
            Sort::Id | Sort::Rand => 0,
            _ => 1,
        }
    }

    /// True when the sort needs the `feed` table joined into the outer query as well as the inner one.
    pub fn joins_feed(self) -> bool {
        matches!(self, Sort::CategoryName | Sort::FeedName)
    }

    /// True when the sort needs the `category` table joined.
    pub fn joins_category(self) -> bool {
        matches!(self, Sort::CategoryName)
    }

    /// True when the reader's secondary sort applies.
    ///
    /// Only the two name sorts can tie: every other sort already ends in `e.id`, which is unique.
    pub fn takes_secondary_sort(self) -> bool {
        matches!(self, Sort::CategoryName | Sort::FeedName)
    }

    /// True when the sorted expression yields a number, so a cursor binds its boundary value as one.
    ///
    /// SQLite orders every INTEGER below every TEXT, so binding a numeric boundary as a string would
    /// compare true against every row and the listing would restart from the top.
    pub fn value_is_numeric(self) -> bool {
        matches!(
            self,
            Sort::Id | Sort::Date | Sort::LastModified | Sort::LastUserModified | Sort::Length
        )
    }
}

impl SecondarySort {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "id" => SecondarySort::Id,
            "date" => SecondarySort::Date,
            "link" | "url" => SecondarySort::Link,
            "title" => SecondarySort::Title,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SecondarySort::Id => "id",
            SecondarySort::Date => "date",
            SecondarySort::Link => "link",
            SecondarySort::Title => "title",
        }
    }

    /// True when the secondary column yields a number. See [`Sort::value_is_numeric`].
    pub fn value_is_numeric(self) -> bool {
        matches!(self, SecondarySort::Id | SecondarySort::Date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sort_round_trips_its_wire_name() {
        for sort in [
            Sort::Id,
            Sort::Date,
            Sort::CategoryName,
            Sort::FeedName,
            Sort::LastModified,
            Sort::LastUserModified,
            Sort::Length,
            Sort::Link,
            Sort::Title,
            Sort::Rand,
        ] {
            assert_eq!(Sort::parse(sort.as_str()), Some(sort), "{sort:?}");
        }
    }

    #[test]
    fn the_freshrss_spellings_are_accepted() {
        assert_eq!(Sort::parse("c.name"), Some(Sort::CategoryName));
        assert_eq!(Sort::parse("f.name"), Some(Sort::FeedName));
        assert_eq!(Sort::parse("lastModified"), Some(Sort::LastModified));
        assert_eq!(
            Sort::parse("lastUserModified"),
            Some(Sort::LastUserModified)
        );
        assert_eq!(Sort::parse("random"), Some(Sort::Rand));
        assert_eq!(Sort::parse("nope"), None);
        assert_eq!(Sort::parse(""), None);
    }

    #[test]
    fn only_rand_is_unresumable() {
        assert!(!Sort::Rand.is_resumable());
        for sort in [Sort::Id, Sort::Date, Sort::Title, Sort::Length, Sort::Link] {
            assert!(sort.is_resumable(), "{sort:?}");
        }
    }

    #[test]
    fn the_nullable_sorts_are_the_two_edit_timestamps() {
        assert!(Sort::LastModified.column_is_nullable());
        assert!(Sort::LastUserModified.column_is_nullable());
        assert!(!Sort::Date.column_is_nullable(), "entry.date is NOT NULL");
        assert!(!Sort::Id.column_is_nullable());
        assert!(!Sort::Title.column_is_nullable());
    }

    #[test]
    fn continuation_arity_counts_the_levels_below_the_id() {
        // category → (c.name, f.name, secondary, id).
        assert_eq!(
            Sort::CategoryName.continuation_arity(SecondarySort::Title),
            3
        );
        // feed → (f.name, secondary, id).
        assert_eq!(Sort::FeedName.continuation_arity(SecondarySort::Date), 2);
        // A secondary sort of `id` *is* the final rung, so it adds no value of its own.
        assert_eq!(Sort::CategoryName.continuation_arity(SecondarySort::Id), 2);
        assert_eq!(Sort::FeedName.continuation_arity(SecondarySort::Id), 1);
        // anything else → (sort value, id).
        assert_eq!(Sort::Date.continuation_arity(SecondarySort::Id), 1);
        assert_eq!(Sort::Title.continuation_arity(SecondarySort::Title), 1);
        // id *is* the key, and rand has none.
        assert_eq!(Sort::Id.continuation_arity(SecondarySort::Id), 0);
        assert_eq!(Sort::Rand.continuation_arity(SecondarySort::Id), 0);
    }

    #[test]
    fn the_name_sorts_need_the_outer_joins_too() {
        // The late-row-lookup re-sorts the page it fetched, so it has to be able to evaluate the same
        // ORDER BY against the rows it joined back.
        assert!(Sort::CategoryName.joins_feed() && Sort::CategoryName.joins_category());
        assert!(Sort::FeedName.joins_feed() && !Sort::FeedName.joins_category());
        assert!(!Sort::Id.joins_feed() && !Sort::Id.joins_category());
        assert!(Sort::CategoryName.takes_secondary_sort());
        assert!(Sort::FeedName.takes_secondary_sort());
        assert!(!Sort::Date.takes_secondary_sort());
    }

    #[test]
    fn the_numeric_sorts_are_the_ones_a_cursor_binds_as_numbers() {
        for sort in [
            Sort::Id,
            Sort::Date,
            Sort::LastModified,
            Sort::LastUserModified,
            Sort::Length,
        ] {
            assert!(sort.value_is_numeric(), "{sort:?}");
        }
        for sort in [Sort::CategoryName, Sort::FeedName, Sort::Link, Sort::Title] {
            assert!(!sort.value_is_numeric(), "{sort:?}");
        }
        assert!(SecondarySort::Id.value_is_numeric());
        assert!(SecondarySort::Date.value_is_numeric());
        assert!(!SecondarySort::Link.value_is_numeric());
        assert!(!SecondarySort::Title.value_is_numeric());
    }

    #[test]
    fn order_signs_are_consistent() {
        // Strict at every level, the id included: an inclusive id comparison would return the boundary
        // row again on every page.
        assert_eq!(Order::Asc.sign(), ">");
        assert_eq!(Order::Desc.sign(), "<");
        assert_eq!(Order::Asc.reverse(), Order::Desc);
        assert_eq!(Order::Desc.reverse(), Order::Asc);
        assert_eq!(Order::default(), Order::Desc, "newest first");
    }

    #[test]
    fn order_parsing_is_lenient_about_case_and_wording() {
        assert_eq!(Order::parse("asc"), Some(Order::Asc));
        assert_eq!(Order::parse("ASC"), Some(Order::Asc));
        assert_eq!(Order::parse("descending"), Some(Order::Desc));
        assert_eq!(Order::parse("sideways"), None);
    }

    #[test]
    fn secondary_sorts_round_trip() {
        for s in [
            SecondarySort::Id,
            SecondarySort::Date,
            SecondarySort::Link,
            SecondarySort::Title,
        ] {
            assert_eq!(SecondarySort::parse(s.as_str()), Some(s), "{s:?}");
        }
        assert_eq!(SecondarySort::parse("url"), Some(SecondarySort::Link));
        assert_eq!(SecondarySort::parse("length"), None);
    }

    #[test]
    fn the_wire_name_is_the_name_the_api_parses() {
        // Two spellings of the same enum would let a client send what it was just given and get a 400
        // back, so `as_str()` and serde have to agree exactly.
        for sort in [
            Sort::Id,
            Sort::Date,
            Sort::CategoryName,
            Sort::FeedName,
            Sort::LastModified,
            Sort::LastUserModified,
            Sort::Length,
            Sort::Link,
            Sort::Title,
            Sort::Rand,
        ] {
            let json = serde_json::to_string(&sort).unwrap();
            assert_eq!(json, format!("\"{}\"", sort.as_str()), "{sort:?}");
            assert_eq!(serde_json::from_str::<Sort>(&json).unwrap(), sort);
        }
        for s in [
            SecondarySort::Id,
            SecondarySort::Date,
            SecondarySort::Link,
            SecondarySort::Title,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()), "{s:?}");
            assert_eq!(serde_json::from_str::<SecondarySort>(&json).unwrap(), s);
        }
        for o in [Order::Asc, Order::Desc] {
            assert_eq!(
                serde_json::to_string(&o).unwrap(),
                format!("\"{}\"", o.as_str().to_ascii_lowercase())
            );
        }
    }
}
