//! Turning a parsed feed into entries.
//!
//! The step between "bytes became a [`RawFeed`]" and "rows went into `entrytmp`". Everything that
//! has to be *decided* rather than copied happens here: which GUID policy applies and whether it had
//! to be degraded, what the change-detection hash covers, where a relative link points, and what the
//! sanitizer was allowed to keep.
//!
//! It is a pure function — no database, and no clock other than the `now` it is handed — because
//! these are the decisions that determine whether a refresh preserves a user's read state, and those
//! have to be testable without either.

use freshrss_feed::guid::{load_guids_with, UnicityCriteria};
use freshrss_feed::hash::{content_hash, enclosures_field, original_content, HashInput};
use freshrss_feed::sanitize::uri::absolutize;
use freshrss_feed::{sanitize_html, RawEnclosure, RawFeed, RawItem, RawThumbnail, SanitizeOptions};

use crate::domain::attributes::{keys, Attributes};
use crate::domain::entry::{Enclosure, Entry, Thumbnail};
use crate::domain::feed::Feed;
use crate::domain::ids::EntryId;

/// The share of blank or duplicate GUIDs one batch may contain before the policy is degraded.
///
/// FreshRSS's default. It is a ratio rounded to a count, which is what makes it behave on short
/// feeds: nothing is tolerated up to 9 articles, one up to 29, two up to 49. A plain percentage
/// would either reject every ten-item feed with one oddity, or accept a ten-thousand-item feed that
/// is half garbage.
pub const INVALID_GUID_TOLERANCE: f64 = 0.05;

/// The sanitizer settings that do not depend on the document being sanitized.
///
/// Kept apart from [`SanitizeOptions`] because the part that *does* depend on the document — the base
/// URL its relative links resolve against — comes from the feed being ingested, and letting a caller
/// supply that would be a way to get it wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sanitize {
    /// Hosts whose `http://` links are upgraded to `https://`.
    pub force_https_domains: Vec<String>,
    /// A byte budget for one article body, or `None` for unlimited.
    pub max_length: Option<usize>,
}

/// One parse, turned into entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Ingested {
    /// The entries, in the feed's own document order.
    ///
    /// Their ids are **provisional**: unique within the batch and ordered by position, which is all
    /// `commit_staged` needs to break ties between articles sharing a publication date. The real ids
    /// come from the global allocator at commit time.
    pub entries: Vec<Entry>,
    /// The GUID policy in force after any degradation.
    pub criteria: UnicityCriteria,
    /// True when [`Self::criteria`] is not what the feed asked for.
    ///
    /// The caller must persist the degraded policy, or the same unusable ids will be rediscovered on
    /// every refresh and the feed will degrade again each time.
    pub degraded: bool,
    /// How many GUIDs in the batch were blank or duplicated. Any value above zero marks the feed as
    /// errored, which is what surfaces it in the problems dialog.
    pub invalid_guids: usize,
}

/// Builds the entries one parse produced.
///
/// `now` is the refresh's timestamp in unix seconds: it becomes every article's `lastSeen`, and the
/// publication date of any article whose feed supplied none.
pub fn ingest(feed: &Feed, raw: &RawFeed, sanitize: &Sanitize, now: i64) -> Ingested {
    // The document's own `<base>` (or `xml:base`) wins over the feed URL, because that is what the
    // document says its relative links mean — and a feed served from a CDN often says something
    // different from where it was fetched.
    let base = raw
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .unwrap_or(feed.url.as_str());
    let options = SanitizeOptions {
        base_url: base.to_owned(),
        force_https_domains: sanitize.force_https_domains.clone(),
        max_length: sanitize.max_length,
    };

    // Sanitize once and reuse the result. The body is both what gets stored and an input to a
    // `sha1:…content…` GUID policy, so sanitizing it twice would let the stored article and the
    // article's identity disagree.
    let contents: Vec<Option<String>> = raw
        .items
        .iter()
        .map(|item| {
            item.effective_content()
                .map(|content| sanitize_html(content, &options))
        })
        .collect();

    let resolution = load_guids_with(
        &raw.items,
        UnicityCriteria::parse(feed.unicity_criteria().unwrap_or_default()),
        feed.unicity_criteria_forced(),
        INVALID_GUID_TOLERANCE,
        Some(&contents),
    );

    let provisional = EntryId::now().micros();
    let entries = raw
        .items
        .iter()
        .zip(&resolution.guids)
        .enumerate()
        .map(|(index, (item, guid))| {
            entry(
                feed,
                item,
                guid,
                contents[index].as_deref(),
                base,
                now,
                provisional + index as i64,
            )
        })
        .collect();

    Ingested {
        entries,
        criteria: resolution.criteria,
        degraded: resolution.degraded,
        invalid_guids: resolution.invalid,
    }
}

/// Builds one entry.
#[allow(clippy::too_many_arguments)]
fn entry(
    feed: &Feed,
    item: &RawItem,
    guid: &str,
    content: Option<&str>,
    base: &str,
    now: i64,
    provisional_id: i64,
) -> Entry {
    let mut entry = Entry {
        id: EntryId::from_micros(provisional_id),
        id_user: feed.id_user,
        id_feed: feed.id,
        guid: guid.to_owned(),
        // Trimmed but not whitespace-collapsed: a title is a hash input, so normalizing it further
        // than the feed's own parser did would mark every article as edited.
        title: item.title.as_deref().unwrap_or_default().trim().to_owned(),
        authors: item.authors.clone(),
        content: content.unwrap_or_default().to_owned(),
        link: item
            .link
            .as_deref()
            .map(|link| absolutize(link.trim(), base))
            .unwrap_or_default(),
        // A feed that supplies no date gets the ingestion time rather than the epoch, so the article
        // sorts where the user found it instead of at the bottom of every list.
        date: item.date.unwrap_or(now),
        last_seen: now,
        last_modified: None,
        last_user_modified: None,
        hash: None,
        is_read: false,
        is_favorite: false,
        tags: item.tags.clone(),
        attributes: Attributes::new(),
    };

    entry.set_thumbnail(item.thumbnail.as_ref().map(thumbnail));
    if !item.enclosures.is_empty() {
        // Serializing a `Vec<Enclosure>` cannot fail — every field is a string, an integer or an
        // option of one — so the error is discarded the way `Entry::set_thumbnail` discards it.
        let _ = entry.attributes.set_typed(
            keys::ENCLOSURES,
            &item.enclosures.iter().map(enclosure).collect::<Vec<_>>(),
        );
    }

    // Computed here, before any full-text scrape could replace the body: the hash is what the next
    // refresh compares the *feed's* output against, so it must describe what the feed said and not
    // what a scraper made of it.
    entry.hash = Some(content_hash(&HashInput {
        link: &entry.link,
        title: &entry.title,
        author: &entry.author_to_db(),
        content: &original_content(
            &entry.content,
            entry.attributes.get_str(keys::ORIGINAL_CONTENT),
        ),
        tags: &entry.tags_to_db(),
        enclosures: &enclosures_field(&item.enclosures),
    }));
    entry
}

fn thumbnail(raw: &RawThumbnail) -> Thumbnail {
    Thumbnail {
        url: raw.url.clone(),
        height: raw.height,
        width: raw.width,
        time: raw.time.clone(),
    }
}

fn enclosure(raw: &RawEnclosure) -> Enclosure {
    Enclosure {
        url: raw.url.clone(),
        mime: raw.mime.clone(),
        medium: raw.medium.clone(),
        length: raw.length,
        title: raw.title.clone(),
        description: raw.description.clone(),
        height: raw.height,
        width: raw.width,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use freshrss_feed::{FeedFormat, RawThumbnail};

    use super::*;

    const NOW: i64 = 1_790_000_000;

    fn feed() -> Feed {
        let mut feed = Feed::new(3, "https://e.example/feed.xml", "Example");
        feed.id = 7;
        feed
    }

    fn item() -> RawItem {
        RawItem {
            guid: Some("id-1".to_owned()),
            title: Some("  A title  ".to_owned()),
            link: Some("/post/1".to_owned()),
            date: Some(1_789_000_000),
            content_html: Some("<p>Body <script>alert(1)</script></p>".to_owned()),
            authors: vec!["Alice".to_owned()],
            tags: vec!["news".to_owned()],
            ..RawItem::default()
        }
    }

    fn raw(items: Vec<RawItem>) -> RawFeed {
        RawFeed {
            title: Some("Example".to_owned()),
            website: Some("https://e.example/".to_owned()),
            items,
            format: FeedFormat::Rss2,
            ..RawFeed::default()
        }
    }

    fn one(item: RawItem) -> Entry {
        let ingested = ingest(&feed(), &raw(vec![item]), &Sanitize::default(), NOW);
        assert_eq!(ingested.entries.len(), 1);
        ingested.entries.into_iter().next().expect("one entry")
    }

    #[test]
    fn an_entry_carries_its_owners_and_the_feed_identity() {
        let entry = one(item());
        assert_eq!(entry.id_user, 3);
        assert_eq!(entry.id_feed, 7);
        assert_eq!(entry.guid, "id-1");
        assert_eq!(entry.title, "A title", "trimmed");
        assert_eq!(entry.authors, ["Alice"]);
        assert_eq!(entry.tags, ["news"]);
        assert_eq!(entry.date, 1_789_000_000);
        assert_eq!(entry.last_seen, NOW);
        assert!(!entry.is_read && !entry.is_favorite);
        assert!(entry.hash.is_some());
    }

    #[test]
    fn a_relative_link_resolves_against_the_feed_url() {
        assert_eq!(one(item()).link, "https://e.example/post/1");
    }

    #[test]
    fn the_documents_own_base_wins_over_the_feed_url() {
        let with_relative = RawItem {
            link: Some("post/1".to_owned()),
            content_html: Some(r#"<p>Body <img src="a.png"></p>"#.to_owned()),
            ..item()
        };
        let mut raw = raw(vec![with_relative]);
        raw.base_url = Some("https://cdn.example/blog/".to_owned());
        let ingested = ingest(&feed(), &raw, &Sanitize::default(), NOW);
        let entry = &ingested.entries[0];
        assert_eq!(entry.link, "https://cdn.example/blog/post/1");
        // The sanitizer resolves against the same base, so an article's relative image follows it
        // rather than the URL the feed was fetched from.
        assert!(
            entry.content.contains("https://cdn.example/blog/a.png"),
            "{}",
            entry.content
        );
    }

    #[test]
    fn the_body_is_sanitized() {
        let entry = one(item());
        assert!(!entry.content.contains("<script"), "{}", entry.content);
        assert!(!entry.content.contains("alert"), "{}", entry.content);
        assert!(entry.content.contains("<p>Body"), "{}", entry.content);
    }

    #[test]
    fn a_missing_date_falls_back_to_the_ingestion_time() {
        let mut item = item();
        item.date = None;
        assert_eq!(one(item).date, NOW);
    }

    #[test]
    fn a_missing_body_is_empty_not_absent() {
        let mut item = item();
        item.content_html = None;
        item.content_text = None;
        let entry = one(item);
        assert_eq!(entry.content, "");
        assert!(entry.hash.is_some(), "an empty body still hashes");
    }

    #[test]
    fn the_hash_excludes_the_date() {
        // A feed that republishes the same article with a corrected timestamp must not look like an
        // edit, or every refresh would clear the read state of the whole backlog.
        let mut earlier = item();
        earlier.date = Some(1_700_000_000);
        let mut later = item();
        later.date = Some(1_790_000_000);
        assert_eq!(one(earlier).hash, one(later).hash);
    }

    #[test]
    fn the_hash_covers_every_content_bearing_field() {
        let baseline = one(item());
        let mut cases: Vec<(&str, RawItem)> = Vec::new();

        let mut changed = item();
        changed.title = Some("A different title".to_owned());
        cases.push(("title", changed));

        let mut changed = item();
        changed.link = Some("/post/2".to_owned());
        cases.push(("link", changed));

        let mut changed = item();
        changed.authors = vec!["Bob".to_owned()];
        cases.push(("author", changed));

        let mut changed = item();
        changed.content_html = Some("<p>Different</p>".to_owned());
        cases.push(("content", changed));

        let mut changed = item();
        changed.tags = vec!["other".to_owned()];
        cases.push(("tags", changed));

        let mut changed = item();
        changed.enclosures = vec![RawEnclosure {
            url: "https://e.example/a.mp3".to_owned(),
            ..RawEnclosure::default()
        }];
        cases.push(("enclosures", changed));

        for (field, changed) in cases {
            assert_ne!(
                baseline.hash,
                one(changed).hash,
                "{field} is part of the hash"
            );
        }
    }

    #[test]
    fn an_identical_item_hashes_identically_across_two_ingestions() {
        // The hash is the refresh pipeline's only evidence of an upstream edit, so it has to be a
        // function of the item and nothing else — not of the clock, and not of the batch it arrived
        // in.
        let alone = one(item());
        let in_a_batch = ingest(
            &feed(),
            &raw(vec![item(); 3]),
            &Sanitize::default(),
            NOW + 999,
        );
        assert_eq!(in_a_batch.entries[0].hash, alone.hash);
        assert_eq!(in_a_batch.entries[2].hash, alone.hash);
    }

    #[test]
    fn thumbnail_and_enclosures_land_in_the_attributes() {
        let mut item = item();
        item.thumbnail = Some(RawThumbnail {
            url: "https://e.example/t.png".to_owned(),
            width: Some(640),
            ..RawThumbnail::default()
        });
        item.enclosures = vec![RawEnclosure {
            url: "https://e.example/a.mp3".to_owned(),
            mime: Some("audio/mpeg".to_owned()),
            medium: Some("audio".to_owned()),
            length: Some(1024),
            ..RawEnclosure::default()
        }];
        let entry = one(item);
        let thumbnail = entry.thumbnail().expect("a thumbnail");
        assert_eq!(thumbnail.url, "https://e.example/t.png");
        assert_eq!(thumbnail.width, Some(640));
        let enclosures = entry.enclosures();
        assert_eq!(enclosures.len(), 1);
        assert_eq!(enclosures[0].mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(enclosures[0].length, Some(1024));
        assert!(entry.has_enclosures());
        assert_eq!(
            entry.playable_enclosure().map(|e| e.url),
            Some("https://e.example/a.mp3".to_owned())
        );
    }

    #[test]
    fn no_attributes_are_written_for_an_ordinary_article() {
        // An empty `attributes` column stays NULL rather than becoming `{}`, which keeps the common
        // case cheap to scan and keeps the column meaning "this article has configuration".
        assert!(one(item()).attributes.is_empty());
    }

    #[test]
    fn provisional_ids_are_unique_and_in_document_order() {
        let ingested = ingest(&feed(), &raw(vec![item(); 5]), &Sanitize::default(), NOW);
        let ids: Vec<i64> = ingested.entries.iter().map(|e| e.id.micros()).collect();
        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    }

    #[test]
    fn absent_guids_fall_back_without_degrading_the_policy() {
        // A feed that supplies no ids at all is not a broken feed: the fallback chain synthesizes a
        // digest per article, they come out unique, and nothing is reported as invalid.
        let items = (1..=3)
            .map(|n| RawItem {
                guid: None,
                link: Some(format!("https://e.example/{n}")),
                ..item()
            })
            .collect();
        let ingested = ingest(&feed(), &raw(items), &Sanitize::default(), NOW);
        assert!(!ingested.degraded, "{ingested:?}");
        assert_eq!(ingested.criteria, UnicityCriteria::Id);
        assert_eq!(ingested.invalid_guids, 0);
        let guids: HashSet<&str> = ingested.entries.iter().map(|e| e.guid.as_str()).collect();
        assert_eq!(guids.len(), 3, "each article kept its own identity");
    }

    #[test]
    fn duplicated_guids_degrade_the_policy() {
        // This is the case degradation exists for: three articles sharing one guid would collapse
        // into a single row, silently losing two of them on every refresh.
        let items = (1..=3)
            .map(|n| RawItem {
                guid: Some("same".to_owned()),
                link: Some(format!("https://e.example/{n}")),
                ..item()
            })
            .collect();
        let ingested = ingest(&feed(), &raw(items), &Sanitize::default(), NOW);
        assert!(ingested.degraded, "{ingested:?}");
        assert_ne!(ingested.criteria, UnicityCriteria::Id);
        let guids: HashSet<&str> = ingested.entries.iter().map(|e| e.guid.as_str()).collect();
        assert_eq!(guids.len(), 3, "the degraded policy told them apart again");
        // And because it did, nothing is reported invalid: the count describes the policy in force,
        // not the one that was abandoned.
        assert_eq!(ingested.invalid_guids, 0);
        assert!(
            ingested.entries[0]
                .guid
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "a degraded guid is a digest: {}",
            ingested.entries[0].guid
        );
    }

    #[test]
    fn a_forced_policy_is_never_degraded() {
        let mut feed = feed();
        feed.attributes
            .set_str(keys::UNICITY_CRITERIA, Some("link"))
            .set_bool(keys::UNICITY_CRITERIA_FORCED, Some(true));
        let blank = RawItem {
            guid: None,
            ..item()
        };
        let ingested = ingest(&feed, &raw(vec![blank; 3]), &Sanitize::default(), NOW);
        assert_eq!(ingested.criteria, UnicityCriteria::Link);
        assert!(!ingested.degraded, "{ingested:?}");
    }

    #[test]
    fn a_forced_policy_with_unusable_ids_reports_them() {
        // Forcing a policy means "do not second-guess me", so the duplicates cannot be resolved by
        // degrading. This is the case that has to reach the caller: it is what marks the feed as
        // errored and puts it in the problems dialog.
        let mut feed = feed();
        feed.attributes
            .set_str(keys::UNICITY_CRITERIA, Some("link"))
            .set_bool(keys::UNICITY_CRITERIA_FORCED, Some(true));
        let items = vec![
            RawItem {
                guid: None,
                ..item()
            };
            3
        ];
        let ingested = ingest(&feed, &raw(items), &Sanitize::default(), NOW);
        assert!(!ingested.degraded, "{ingested:?}");
        assert_eq!(ingested.criteria, UnicityCriteria::Link);
        assert!(
            ingested.invalid_guids > 0,
            "three identical links are three identical guids: {ingested:?}"
        );
        let guids: HashSet<&str> = ingested.entries.iter().map(|e| e.guid.as_str()).collect();
        assert_eq!(guids.len(), 1, "and they really did collapse into one");
    }

    #[test]
    fn a_configured_policy_is_honoured() {
        let mut feed = feed();
        feed.attributes
            .set_str(keys::UNICITY_CRITERIA, Some("sha1:title"));
        let without_id = RawItem {
            guid: Some("ignored".to_owned()),
            ..item()
        };
        let ingested = ingest(&feed, &raw(vec![without_id]), &Sanitize::default(), NOW);
        assert!(!ingested.degraded, "{ingested:?}");
        let guid = ingested.entries[0].guid.clone();
        assert_ne!(guid, "ignored");
        assert_eq!(guid.len(), 40, "a sha1 digest: {guid}");

        // The same title under the same policy is the same guid, which is the point of asking for it.
        let again = ingest(&feed, &raw(vec![item()]), &Sanitize::default(), NOW);
        assert_eq!(again.entries[0].guid, guid);
    }

    #[test]
    fn force_https_reaches_article_bodies() {
        let sanitize = Sanitize {
            force_https_domains: vec!["e.example".to_owned()],
            max_length: None,
        };
        let with_image = RawItem {
            content_html: Some(r#"<img src="http://e.example/a.png">"#.to_owned()),
            ..item()
        };
        let ingested = ingest(&feed(), &raw(vec![with_image]), &sanitize, NOW);
        assert!(
            ingested.entries[0]
                .content
                .contains("https://e.example/a.png"),
            "{}",
            ingested.entries[0].content
        );
    }

    #[test]
    fn a_body_budget_is_honoured() {
        let sanitize = Sanitize {
            force_https_domains: Vec::new(),
            max_length: Some(64),
        };
        let long = RawItem {
            content_html: Some(format!("<p>{}</p>", "x".repeat(4096))),
            ..item()
        };
        let ingested = ingest(&feed(), &raw(vec![long]), &sanitize, NOW);
        let len = ingested.entries[0].content.len();
        assert!(len <= 64, "{len} bytes");
    }
}
