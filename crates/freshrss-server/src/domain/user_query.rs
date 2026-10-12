//! A saved search: a named query the reader can re-run, and the thing `S:`/`search:` refers to.
//!
//! FreshRSS stores these in the user's own config file as an array; here they are rows, because a
//! `UNIQUE(id_user, name)` index and a foreign key do the work the array's conventions had to.
//!
//! What is saved is the **query text**, not a compiled tree. That is deliberate: the grammar evolves,
//! and a tree serialized by an older build would have to be migrated or refused, whereas text is
//! re-parsed with whatever grammar is current. It also means the reader can show and edit what will
//! actually run.

use crate::domain::attributes::Attributes;
use crate::domain::list_type::ListType;
use crate::domain::sort::{Order, SecondarySort, Sort};
use crate::domain::state::EntryState;

/// One saved search.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedQuery {
    pub id: i64,
    pub id_user: i64,
    pub name: String,
    /// The BooleanSearch text. May be blank, which [`crate::expand`] treats as missing rather than as
    /// "no constraint" — a filter action keyed on a blank saved search would otherwise match everything.
    pub search: String,
    /// The scope to run it in, so a saved search can be "unread in this folder" rather than just a query.
    pub scope: ListType,
    /// The scope's id, when it needs one.
    pub scope_id: Option<i64>,
    pub state: EntryState,
    pub sort: Sort,
    pub order: Order,
    pub secondary_sort: SecondarySort,
    pub secondary_order: Order,
    /// Sidebar ordering; ascending.
    pub position: i64,
    pub attributes: Attributes,
}

impl SavedQuery {
    pub fn new(id_user: i64, name: impl Into<String>, search: impl Into<String>) -> Self {
        Self {
            id: 0,
            id_user,
            name: name.into(),
            search: search.into(),
            scope: ListType::default(),
            scope_id: None,
            state: EntryState::ALL,
            sort: Sort::default(),
            order: Order::default(),
            secondary_sort: SecondarySort::default(),
            secondary_order: Order::default(),
            position: 0,
            attributes: Attributes::new(),
        }
    }

    /// True when this has a query worth running.
    pub fn is_usable(&self) -> bool {
        !self.search.trim().is_empty()
    }

    /// The `sort_order` column's `"sort:direction"` spelling.
    pub fn sort_order_to_db(&self) -> String {
        format!(
            "{}:{}",
            self.sort.as_str(),
            self.order.as_str().to_ascii_lowercase()
        )
    }

    /// Parses the `"sort:direction"` spelling, falling back to the defaults on anything unrecognised.
    ///
    /// A saved row is read far more often than it is written and a bad value must not make the sidebar
    /// fail to load, so this degrades rather than erroring.
    pub fn sort_order_from_db(raw: &str) -> (Sort, Order) {
        let (sort, order) = raw.split_once(':').unwrap_or((raw, ""));
        (
            Sort::parse(sort.trim()).unwrap_or_default(),
            Order::parse(order.trim()).unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_saved_query_defaults_to_the_main_stream_newest_first() {
        let q = SavedQuery::new(3, "unread rust", "intitle:rust");
        assert_eq!(q.id_user, 3);
        assert_eq!(q.scope, ListType::MainStream);
        assert_eq!(q.sort, Sort::Id);
        assert_eq!(q.order, Order::Desc);
        assert!(q.is_usable());
    }

    #[test]
    fn a_blank_search_is_not_usable() {
        assert!(!SavedQuery::new(1, "blank", "   ").is_usable());
        assert!(SavedQuery::new(1, "real", "intitle:a").is_usable());
    }

    #[test]
    fn the_sort_order_column_round_trips() {
        for (sort, order) in [
            (Sort::Id, Order::Desc),
            (Sort::Date, Order::Asc),
            (Sort::CategoryName, Order::Desc),
            (Sort::LastUserModified, Order::Asc),
        ] {
            let q = SavedQuery {
                sort,
                order,
                ..SavedQuery::new(1, "n", "x")
            };
            let raw = q.sort_order_to_db();
            assert_eq!(SavedQuery::sort_order_from_db(&raw), (sort, order), "{raw}");
        }
    }

    #[test]
    fn a_corrupt_sort_order_degrades_rather_than_failing() {
        assert_eq!(
            SavedQuery::sort_order_from_db("colour:sideways"),
            (Sort::Id, Order::Desc)
        );
        assert_eq!(
            SavedQuery::sort_order_from_db(""),
            (Sort::Id, Order::Desc)
        );
        assert_eq!(
            SavedQuery::sort_order_from_db("date"),
            (Sort::Date, Order::Desc)
        );
    }

    #[test]
    fn the_freshrss_spelling_is_accepted_too() {
        // An imported preference may use the PHP names.
        assert_eq!(
            SavedQuery::sort_order_from_db("lastModified:asc"),
            (Sort::LastModified, Order::Asc)
        );
    }
}
