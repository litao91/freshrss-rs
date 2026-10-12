//! Deciding what one refresh does to each article the feed published.
//!
//! The three outcomes are FreshRSS's, and the reason there are three rather than two is that an
//! article can be *unchanged* and still matter: its `lastSeen` has to slide forward, or retention
//! would eventually delete an article from a feed that simply has not published anything new.

use std::collections::HashMap;

use crate::domain::entry::Entry;

/// What the database already knows about this feed: guid → stored hash, for the guids in the batch.
///
/// A guid missing from the map means "new article". A guid present with a `NULL` or malformed hash
/// is omitted by [`list_hashes_for_guids`](crate::db::dao::entry::list_hashes_for_guids), so it is
/// treated as new too — re-writing an article is recoverable, deciding it is unchanged when its
/// fingerprint was never recorded is not.
pub type Known = HashMap<String, [u8; 16]>;

/// One refresh's write plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Articles this feed has never published before.
    ///
    /// Staged into `entrytmp` and committed in one statement, which is where they get their real ids
    /// from the global allocator. Their ids up to that point are provisional and must not be read.
    pub insert: Vec<Entry>,
    /// Articles whose content changed upstream.
    ///
    /// Updated in place and keyed on `(id_feed, guid)`, never on id: an edit must not move an
    /// article, or the user's read and starred state would be orphaned on the old row.
    pub update: Vec<Entry>,
    /// Articles already stored with the same hash. Nothing is written for these except `lastSeen`.
    pub unchanged: usize,
}

impl Plan {
    /// True when the feed published nothing this refresh needed to act on.
    ///
    /// Distinct from "the fetch failed" and from "the feed is empty": a healthy feed that has
    /// nothing new is the common case, and the caller reports it differently.
    pub fn is_noop(&self) -> bool {
        self.insert.is_empty() && self.update.is_empty()
    }

    /// How many articles the refresh added or changed, for the job report.
    pub fn touched(&self) -> usize {
        self.insert.len() + self.update.len()
    }
}

/// Splits one batch of ingested entries against what the database already holds.
///
/// `now` becomes `lastModified` on every updated article — the timestamp of the refresh that noticed
/// the edit, not of the edit itself, which is all a reader can know.
///
/// An updated article keeps `is_read` and `is_favorite` as ingest left them (`false`), which
/// [`update_existing`](crate::db::dao::entry::update_existing) turns into `COALESCE(NULL, column)`:
/// the stored value survives. An upstream edit must not silently un-read an article, because the
/// user's attention is the one thing a feed has no business revoking.
pub fn reconcile(entries: Vec<Entry>, known: &Known, now: i64) -> Plan {
    let mut plan = Plan::default();
    for mut entry in entries {
        match known.get(&entry.guid) {
            None => plan.insert.push(entry),
            Some(stored) if entry.hash.is_some_and(|hash| hash == *stored) => plan.unchanged += 1,
            Some(_) => {
                entry.last_modified = Some(now);
                plan.update.push(entry);
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use freshrss_feed::hash::content_hash;

    use super::*;

    const NOW: i64 = 1_790_000_000;

    fn entry(guid: &str, title: &str) -> Entry {
        let mut entry = Entry::new(3, 7, guid);
        entry.title = title.to_owned();
        entry.link = format!("https://e.example/{guid}");
        entry.content = format!("<p>{title}</p>");
        entry.hash = Some(content_hash(&freshrss_feed::hash::HashInput {
            link: &entry.link,
            title: &entry.title,
            author: &entry.author_to_db(),
            content: &entry.content,
            tags: &entry.tags_to_db(),
            enclosures: "",
        }));
        entry
    }

    fn known(entries: &[Entry]) -> Known {
        entries
            .iter()
            .map(|e| (e.guid.clone(), e.hash.expect("hashed")))
            .collect()
    }

    #[test]
    fn a_new_guid_is_inserted() {
        let plan = reconcile(vec![entry("a", "A")], &Known::new(), NOW);
        assert_eq!(plan.insert.len(), 1);
        assert!(plan.update.is_empty());
        assert_eq!(plan.unchanged, 0);
        assert!(!plan.is_noop());
        assert_eq!(plan.touched(), 1);
    }

    #[test]
    fn an_identical_hash_changes_nothing() {
        let stored = entry("a", "A");
        let plan = reconcile(vec![entry("a", "A")], &known(&[stored]), NOW);
        assert!(plan.insert.is_empty());
        assert!(plan.update.is_empty());
        assert_eq!(plan.unchanged, 1);
        assert!(plan.is_noop(), "the common case on a quiet feed");
        assert_eq!(plan.touched(), 0);
    }

    #[test]
    fn a_changed_hash_updates_in_place_and_stamps_last_modified() {
        let stored = entry("a", "A");
        let edited = entry("a", "A, corrected");
        let id_before = edited.id;
        let plan = reconcile(vec![edited], &known(&[stored]), NOW);

        assert!(plan.insert.is_empty(), "an edit is not a new article");
        assert_eq!(plan.update.len(), 1);
        let updated = &plan.update[0];
        assert_eq!(updated.last_modified, Some(NOW));
        assert_eq!(updated.title, "A, corrected");
        assert_eq!(
            updated.id, id_before,
            "the provisional id is untouched; the UPDATE keys on (id_feed, guid)"
        );
        assert!(
            !updated.is_read && !updated.is_favorite,
            "so COALESCE keeps the user's state"
        );
    }

    #[test]
    fn a_stored_article_without_a_new_one_is_left_alone() {
        // Vanished articles are never deleted by a refresh; retention acts on `lastSeen` instead,
        // which is a separate schedule with its own keep rules.
        let plan = reconcile(Vec::new(), &known(&[entry("gone", "G")]), NOW);
        assert!(plan.is_noop());
        assert_eq!(plan.unchanged, 0);
    }

    #[test]
    fn a_batch_splits_three_ways_in_document_order() {
        let unchanged = entry("b", "B");
        let changed = entry("c", "C");
        let store = [unchanged.clone(), changed.clone(), entry("d", "D old")];
        let mut known = known(&store);
        // `d` was stored under a different title, so its hash differs and it counts as an edit.
        known.insert("d".to_owned(), [0u8; 16]);

        let plan = reconcile(
            vec![
                entry("a", "A"),
                entry("b", "B"),
                entry("c", "C edited"),
                entry("d", "D"),
            ],
            &known,
            NOW,
        );
        assert_eq!(
            plan.insert
                .iter()
                .map(|e| e.guid.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(
            plan.update
                .iter()
                .map(|e| e.guid.as_str())
                .collect::<Vec<_>>(),
            ["c", "d"],
            "document order, so a feed's own ordering survives into the write"
        );
        assert_eq!(plan.unchanged, 1);
        assert_eq!(plan.touched(), 3);
    }

    #[test]
    fn a_repeated_guid_in_one_batch_is_not_an_error_here() {
        // A feed that publishes the same item twice reaches staging twice, and `INSERT OR IGNORE`
        // on `UNIQUE(id_feed, guid)` makes the second a no-op. Deduplicating here instead would
        // hide the repetition that `invalid_guids` exists to report.
        let plan = reconcile(vec![entry("a", "A"), entry("a", "A")], &Known::new(), NOW);
        assert_eq!(plan.insert.len(), 2);
    }

    #[test]
    fn an_entry_with_no_hash_is_treated_as_changed() {
        let stored = entry("a", "A");
        let mut no_hash = entry("a", "A");
        no_hash.hash = None;
        let plan = reconcile(vec![no_hash], &known(&[stored]), NOW);
        assert_eq!(
            plan.update.len(),
            1,
            "re-writing is recoverable; assuming unchanged is not"
        );
    }
}
