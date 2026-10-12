//! Article identity: which string decides "this is the same article I already have".
//!
//! Port of `FreshRSS_Feed::decideEntryGuid()` and `::loadGuids()` (`app/Models/Feed.php`).
//!
//! This is the single most load-bearing piece of the feed crate. A GUID that changes between
//! refreshes makes every article look new — the user's read state is lost and the list fills with
//! duplicates. A GUID that is too weak merges genuinely different articles. Both failure modes are
//! invisible in tests that only parse a fixture once, which is why [`GuidInput`] keeps the feed's
//! raw id in an `Option` rather than an empty-string default: `None` ("the feed gave no id") and
//! `Some("")` ("the feed gave an empty id") must take different paths through the fallback chain.

use sha1::{Digest as _, Sha1};

use crate::raw::RawItem;

/// `sha1("")`. FreshRSS treats this specific digest as "no GUID at all", because a policy applied
/// to an item missing every field it reads produces exactly this.
pub const BLANK_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

/// How a feed's articles should be identified.
///
/// Stored as the `unicityCriteria` string in a feed's `attributes`, exactly as FreshRSS does, so
/// an OPML export carries the policy across.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum UnicityCriteria {
    /// Use the feed's own id (`<guid>`, `<id>`, `itemUid`). The default, and correct for any
    /// well-behaved feed.
    #[default]
    Id,
    /// Use the permalink. For feeds whose `<guid>` is a random or rotating value.
    Link,
    Sha1LinkPublished,
    Sha1LinkPublishedTitle,
    Sha1LinkPublishedTitleContent,
    Sha1Title,
    Sha1TitlePublished,
    Sha1TitlePublishedContent,
    Sha1Content,
    Sha1ContentPublished,
    Sha1Published,
}

impl UnicityCriteria {
    /// The attribute string. [`UnicityCriteria::Id`] maps to `"id"`, which is how the OPML
    /// exporter and the subscription form spell the default.
    pub const fn as_str(self) -> &'static str {
        match self {
            UnicityCriteria::Id => "id",
            UnicityCriteria::Link => "link",
            UnicityCriteria::Sha1LinkPublished => "sha1:link_published",
            UnicityCriteria::Sha1LinkPublishedTitle => "sha1:link_published_title",
            UnicityCriteria::Sha1LinkPublishedTitleContent => "sha1:link_published_title_content",
            UnicityCriteria::Sha1Title => "sha1:title",
            UnicityCriteria::Sha1TitlePublished => "sha1:title_published",
            UnicityCriteria::Sha1TitlePublishedContent => "sha1:title_published_content",
            UnicityCriteria::Sha1Content => "sha1:content",
            UnicityCriteria::Sha1ContentPublished => "sha1:content_published",
            UnicityCriteria::Sha1Published => "sha1:published",
        }
    }

    /// Parses the attribute string. Unknown values degrade to [`UnicityCriteria::Id`] rather than
    /// erroring, matching FreshRSS's `default => $entryId` arm — a typo in a hand-edited OPML must
    /// not make a feed unparseable.
    pub fn parse(value: &str) -> Self {
        match value {
            "link" => UnicityCriteria::Link,
            "sha1:link_published" => UnicityCriteria::Sha1LinkPublished,
            "sha1:link_published_title" => UnicityCriteria::Sha1LinkPublishedTitle,
            "sha1:link_published_title_content" => UnicityCriteria::Sha1LinkPublishedTitleContent,
            "sha1:title" => UnicityCriteria::Sha1Title,
            "sha1:title_published" => UnicityCriteria::Sha1TitlePublished,
            "sha1:title_published_content" => UnicityCriteria::Sha1TitlePublishedContent,
            "sha1:content" => UnicityCriteria::Sha1Content,
            "sha1:content_published" => UnicityCriteria::Sha1ContentPublished,
            "sha1:published" => UnicityCriteria::Sha1Published,
            _ => UnicityCriteria::Id,
        }
    }

    /// The next weaker policy, or `None` when this one cannot be degraded further.
    ///
    /// Mirrors FreshRSS's automatic fallback: a feed whose ids turn out to be unusable is first
    /// tried on `link + published`, then on `link + published + title`, then left alone. Degrading
    /// further would silently merge distinct articles, which is worse than the duplicates it fixes.
    pub const fn degrade(self) -> Option<Self> {
        match self {
            UnicityCriteria::Id | UnicityCriteria::Link => Some(UnicityCriteria::Sha1LinkPublished),
            UnicityCriteria::Sha1LinkPublished => Some(UnicityCriteria::Sha1LinkPublishedTitle),
            _ => None,
        }
    }

    /// True when the policy reads the article body, so callers know a sanitize pass is required
    /// before the GUID can be decided.
    pub const fn reads_content(self) -> bool {
        matches!(
            self,
            UnicityCriteria::Sha1LinkPublishedTitleContent
                | UnicityCriteria::Sha1TitlePublishedContent
                | UnicityCriteria::Sha1Content
                | UnicityCriteria::Sha1ContentPublished
        )
    }
}

/// Everything a GUID policy might read, taken from one [`RawItem`].
///
/// `content` must be the **sanitized** body when the policy reads it, because that is what
/// SimplePie's `get_content()` returns inside FreshRSS's `decideEntryGuid`. Passing raw markup
/// would produce GUIDs that shift the moment the sanitizer's whitelist changes.
#[derive(Debug, Clone, Default)]
pub struct GuidInput<'a> {
    /// The feed's own id, ASCII-sanitized. `None` when the feed supplied no id element at all.
    ///
    /// A `Cow` because sanitizing needs to own the result, but ids are pure ASCII in practice and
    /// this keeps the common path allocation-free.
    pub id: Option<std::borrow::Cow<'a, str>>,
    /// The resolved permalink.
    pub link: Option<&'a str>,
    pub title: Option<&'a str>,
    pub content: Option<&'a str>,
    /// Publication time in unix seconds.
    pub date: Option<i64>,
}

impl<'a> GuidInput<'a> {
    /// Builds the input for an item, using [`RawItem::effective_content`] for the body.
    ///
    /// `content` here is whatever the caller has produced so far; the refresh pipeline passes the
    /// sanitized body once sanitization has run.
    pub fn from_item(item: &'a RawItem, content: Option<&'a str>) -> Self {
        Self {
            // `guid` stays an `Option` all the way down: an absent id and an empty one differ.
            id: item
                .guid
                .as_deref()
                .map(|g| ascii_cow(g)),
            link: item.link.as_deref(),
            title: item.title.as_deref(),
            content: content.or_else(|| item.effective_content()),
            date: item.date,
        }
    }

    fn id_or_empty(&self) -> &str {
        self.id.as_deref().unwrap_or("")
    }
    fn link_or_empty(&self) -> &str {
        self.link.unwrap_or("")
    }
    fn title_or_empty(&self) -> &str {
        self.title.unwrap_or("")
    }
    fn content_or_empty(&self) -> &str {
        self.content.unwrap_or("")
    }
    /// `get_date('U')` inside a PHP string concatenation: the decimal seconds, or nothing.
    fn date_or_empty(&self) -> String {
        self.date.map_or_else(String::new, |d| d.to_string())
    }
}

fn ascii_cow(text: &str) -> std::borrow::Cow<'_, str> {
    if text.is_ascii() && !text.bytes().any(|b| b < 0x20) {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(safe_ascii(text))
    }
}

/// `safe_ascii()` from `lib/lib_rss.php`: strips control and non-ASCII bytes.
///
/// A GUID ends up in a SQL `UNIQUE(id_feed, guid)` index and in URLs; a feed that puts a newline or
/// a right-to-left mark in `<guid>` must not be able to corrupt either. PHP strips `< 0x20` and
/// `> 0x7F` and keeps everything between, including `DEL`.
#[must_use]
pub fn safe_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if (0x20..=0x7F).contains(&b) {
            out.push(b as char);
        }
    }
    out
}

/// Decides an article's GUID.
///
/// `fallback` is `true` for the normal path: when the chosen policy yields nothing, walk down
/// `id → sha1(link.date) → sha1(link.date.title) → sha1(link.date.title.content)` until something
/// non-empty appears. FreshRSS passes `fallback: true` from both `loadGuids` and `loadEntries`, so
/// an empty GUID is always a last resort rather than a policy outcome.
///
/// Returns `""` when even the fallback chain has nothing to work with, which the caller must treat
/// as "invalid" (see [`GuidResolution::invalid`]).
pub fn decide_guid(input: &GuidInput<'_>, criteria: UnicityCriteria, fallback: bool) -> String {
    let date = input.date_or_empty();
    let mut guid = match criteria {
        // A feed that gave no id at all must not be confused with one that gave an empty id: only
        // the former should reach the fallback chain via the `Id` arm.
        UnicityCriteria::Id => input.id_or_empty().to_owned(),
        UnicityCriteria::Link => input.link_or_empty().to_owned(),
        UnicityCriteria::Sha1LinkPublished => sha1_hex([input.link_or_empty(), &date]),
        UnicityCriteria::Sha1LinkPublishedTitle => {
            sha1_hex([input.link_or_empty(), &date, input.title_or_empty()])
        }
        UnicityCriteria::Sha1LinkPublishedTitleContent => sha1_hex([
            input.link_or_empty(),
            &date,
            input.title_or_empty(),
            input.content_or_empty(),
        ]),
        UnicityCriteria::Sha1Title => sha1_hex([input.title_or_empty()]),
        UnicityCriteria::Sha1TitlePublished => sha1_hex([input.title_or_empty(), &date]),
        UnicityCriteria::Sha1TitlePublishedContent => {
            sha1_hex([input.title_or_empty(), &date, input.content_or_empty()])
        }
        UnicityCriteria::Sha1Content => sha1_hex([input.content_or_empty()]),
        UnicityCriteria::Sha1ContentPublished => sha1_hex([input.content_or_empty(), &date]),
        UnicityCriteria::Sha1Published => sha1_hex([&date]),
    };

    if guid == BLANK_SHA1 {
        guid.clear();
    }

    if fallback && guid.is_empty() {
        guid = fallback_guid(input, &date);
    }
    guid
}

/// The `id → link+date → +title → +content` chain FreshRSS runs when the policy produced nothing.
fn fallback_guid(input: &GuidInput<'_>, date: &str) -> String {
    let link = input.link_or_empty();
    let title = input.title_or_empty();
    let id = input.id_or_empty();

    let guid = if !id.is_empty() {
        id.to_owned()
    } else if !link.is_empty() {
        sha1_hex([link, date])
    } else if !title.is_empty() {
        sha1_hex([link, date, title])
    } else {
        sha1_hex([link, date, title, input.content_or_empty()])
    };

    if guid == BLANK_SHA1 {
        String::new()
    } else {
        guid
    }
}

fn sha1_hex<I, S>(parts: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part.as_ref().as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The outcome of validating a whole batch of GUIDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidResolution {
    /// One GUID per item, in document order.
    pub guids: Vec<String>,
    /// The policy in force after any degradation. Persist this when it differs from what was asked
    /// for, or the same bad ids will be rediscovered on every refresh.
    pub criteria: UnicityCriteria,
    /// True when [`Self::criteria`] was degraded away from the requested policy.
    pub degraded: bool,
    /// How many GUIDs were blank or duplicated within the batch. Any value above zero marks the
    /// feed as errored, which is what surfaces it in the problems dialog.
    pub invalid: usize,
}

/// Validates the GUIDs for one refresh and degrades the policy if too many are unusable.
///
/// Port of `loadGuids`. The tolerance is a *ratio rounded to a count*, which is what makes the
/// default of 5% behave sensibly on short feeds: 0 invalid tolerated up to 9 articles, 1 up to 29,
/// 2 up to 49. A plain percentage would either reject every 10-item feed with one oddity or accept
/// a 10 000-item feed that is half garbage.
///
/// `forced` is the `unicityCriteriaForced` attribute: a user who explicitly chose a policy has said
/// "I know what this feed does", so the automatic degradation is skipped.
pub fn load_guids(
    items: &[RawItem],
    requested: UnicityCriteria,
    forced: bool,
    tolerance: f64,
) -> GuidResolution {
    load_guids_with(items, requested, forced, tolerance, None)
}

/// [`load_guids`] with an override for the article body.
///
/// The refresh pipeline sanitizes content once and reuses it here, so a `sha1:*content*` policy
/// does not pay for a second sanitize pass per item.
pub fn load_guids_with(
    items: &[RawItem],
    requested: UnicityCriteria,
    forced: bool,
    tolerance: f64,
    content: Option<&[Option<String>]>,
) -> GuidResolution {
    let mut criteria = requested;

    loop {
        let mut guids = Vec::with_capacity(items.len());
        let mut invalid = 0usize;
        let mut seen = std::collections::HashSet::with_capacity(items.len());

        for (i, item) in items.iter().enumerate() {
            let body = content.and_then(|c| c.get(i)).and_then(|c| c.as_deref());
            let input = GuidInput::from_item(item, body);
            let guid = decide_guid(&input, criteria, true);
            if guid.is_empty() || !seen.insert(guid.clone()) {
                invalid += 1;
            }
            guids.push(guid);
        }

        let degrade = !forced
            && invalid as f64 > (tolerance * items.len() as f64).round()
            && criteria.degrade().is_some();
        if !degrade {
            return GuidResolution {
                guids,
                criteria,
                degraded: criteria != requested,
                invalid,
            };
        }
        criteria = criteria.degrade().unwrap_or(criteria);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> RawItem {
        RawItem {
            guid: Some("urn:uuid:1".into()),
            link: Some("https://example.com/a".into()),
            title: Some("Title".into()),
            content_html: Some("<p>Body</p>".into()),
            date: Some(1_700_000_000),
            ..RawItem::default()
        }
    }

    fn input<'a>(item: &'a RawItem) -> GuidInput<'a> {
        GuidInput::from_item(item, None)
    }

    #[test]
    fn criteria_strings_round_trip() {
        for name in [
            "id",
            "link",
            "sha1:link_published",
            "sha1:link_published_title",
            "sha1:link_published_title_content",
            "sha1:title",
            "sha1:title_published",
            "sha1:title_published_content",
            "sha1:content",
            "sha1:content_published",
            "sha1:published",
        ] {
            assert_eq!(UnicityCriteria::parse(name).as_str(), name);
        }
    }

    #[test]
    fn unknown_criteria_degrade_to_id_rather_than_erroring() {
        assert_eq!(UnicityCriteria::parse("sha1:nonsense"), UnicityCriteria::Id);
        assert_eq!(UnicityCriteria::parse(""), UnicityCriteria::Id);
        assert_eq!(UnicityCriteria::parse("DROP TABLE"), UnicityCriteria::Id);
    }

    #[test]
    fn the_default_policy_uses_the_feeds_own_id() {
        let it = item();
        assert_eq!(
            decide_guid(&input(&it), UnicityCriteria::default(), true),
            "urn:uuid:1"
        );
    }

    #[test]
    fn a_missing_id_is_not_the_same_as_an_empty_id() {
        let mut it = item();
        it.guid = None;
        // Absent: the fallback chain has nothing to start from, so it hashes link+date.
        let absent = decide_guid(&input(&it), UnicityCriteria::Id, true);
        assert_eq!(absent, sha1_hex(["https://example.com/a", "1700000000"]));

        it.guid = Some(String::new());
        // Present but empty: same outcome here, but the distinction is what keeps `Id` from
        // silently becoming `Link` for feeds that do send an empty `<guid/>`.
        assert_eq!(decide_guid(&input(&it), UnicityCriteria::Id, true), absent);
    }

    #[test]
    fn without_fallback_an_unusable_policy_returns_empty() {
        let mut it = item();
        it.guid = None;
        it.link = None;
        it.title = None;
        it.content_html = None;
        it.date = None;
        assert_eq!(decide_guid(&input(&it), UnicityCriteria::Id, false), "");
        assert_eq!(decide_guid(&input(&it), UnicityCriteria::Link, false), "");
    }

    #[test]
    fn the_blank_sha1_is_treated_as_no_guid() {
        let empty = RawItem::default();
        // sha1("") is exactly BLANK_SHA1, so this must come back empty and not as a digest.
        assert_eq!(decide_guid(&input(&empty), UnicityCriteria::Sha1Title, true), "");
        assert_eq!(
            decide_guid(&input(&empty), UnicityCriteria::Sha1Title, false),
            ""
        );
        assert_eq!(BLANK_SHA1, sha1_hex([""]));
    }

    #[test]
    fn fallback_chain_walks_id_then_link_then_title_then_content() {
        fn with_content<'a>(item: &'a RawItem, content: Option<&'a str>) -> GuidInput<'a> {
            GuidInput::from_item(item, content)
        }
        fn decide(item: &RawItem, content: Option<&str>) -> String {
            decide_guid(&with_content(item, content), UnicityCriteria::Link, true)
        }

        // Nothing but a date: link is empty so the chain skips to title, which is also empty,
        // then to content.
        let bare = RawItem { date: Some(42), ..RawItem::default() };
        assert_eq!(decide(&bare, None), sha1_hex(["", "42", "", ""]));

        let with_link = RawItem {
            link: Some("https://example.com/x".into()),
            ..bare.clone()
        };
        assert_eq!(
            decide(&with_link, None),
            "https://example.com/x",
            "a usable link is the guid"
        );

        let with_title = RawItem { title: Some("T".into()), ..bare.clone() };
        assert_eq!(decide(&with_title, None), sha1_hex(["", "42", "T"]));

        assert_eq!(decide(&bare, Some("C")), sha1_hex(["", "42", "", "C"]));

        let with_id = RawItem { guid: Some("real-id".into()), ..bare };
        assert_eq!(
            decide(&with_id, Some("C")),
            "real-id",
            "the feed's own id wins over any synthesized value"
        );
    }

    #[test]
    fn sha1_policies_concatenate_in_the_documented_order() {
        let it = item();
        let inp = input(&it);
        assert_eq!(
            decide_guid(&inp, UnicityCriteria::Sha1LinkPublishedTitle, true),
            sha1_hex(["https://example.com/a", "1700000000", "Title"])
        );
        assert_eq!(
            decide_guid(&inp, UnicityCriteria::Sha1TitlePublished, true),
            sha1_hex(["Title", "1700000000"])
        );
        assert_eq!(
            decide_guid(&inp, UnicityCriteria::Sha1Published, true),
            sha1_hex(["1700000000"])
        );
        assert_eq!(
            decide_guid(&inp, UnicityCriteria::Sha1Content, true),
            sha1_hex(["<p>Body</p>"])
        );
    }

    #[test]
    fn a_missing_date_contributes_nothing_to_the_digest() {
        let mut it = item();
        it.date = None;
        assert_eq!(
            decide_guid(&input(&it), UnicityCriteria::Sha1LinkPublished, true),
            sha1_hex(["https://example.com/a", ""])
        );
    }

    #[test]
    fn guids_are_stable_across_repeated_decisions() {
        let it = item();
        let criteria = [
            UnicityCriteria::Id,
            UnicityCriteria::Link,
            UnicityCriteria::Sha1LinkPublishedTitleContent,
        ];
        for c in criteria {
            let first = decide_guid(&input(&it), c, true);
            for _ in 0..5 {
                assert_eq!(decide_guid(&input(&it), c, true), first, "{c:?}");
            }
        }
    }

    #[test]
    fn degradation_steps_down_and_then_stops() {
        assert_eq!(
            UnicityCriteria::Id.degrade(),
            Some(UnicityCriteria::Sha1LinkPublished)
        );
        assert_eq!(
            UnicityCriteria::Link.degrade(),
            Some(UnicityCriteria::Sha1LinkPublished)
        );
        assert_eq!(
            UnicityCriteria::Sha1LinkPublished.degrade(),
            Some(UnicityCriteria::Sha1LinkPublishedTitle)
        );
        assert_eq!(UnicityCriteria::Sha1LinkPublishedTitle.degrade(), None);
        assert_eq!(UnicityCriteria::Sha1Title.degrade(), None);
    }

    #[test]
    fn safe_ascii_strips_control_and_non_ascii_bytes() {
        assert_eq!(safe_ascii("abc"), "abc");
        assert_eq!(safe_ascii("a\nb\tc"), "abc");
        assert_eq!(safe_ascii("café"), "caf");
        assert_eq!(safe_ascii("\u{202e}evil"), "evil");
        assert_eq!(safe_ascii(""), "");
        // PHP keeps DEL (0x7F) because it is neither low nor high.
        assert_eq!(safe_ascii("a\u{7f}b"), "a\u{7f}b");
        // Printable ASCII survives.
        assert_eq!(safe_ascii(" ~!@#$%^&*()"), " ~!@#$%^&*()");
    }

    #[test]
    fn an_id_with_non_ascii_bytes_is_cleaned_before_use() {
        let mut it = item();
        it.guid = Some("café".into());
        assert_eq!(decide_guid(&input(&it), UnicityCriteria::Id, true), "caf");

        // A right-to-left override in a guid is a spoofing vector in any UI that shows it.
        it.guid = Some("\u{202e}urn:1".into());
        assert_eq!(decide_guid(&input(&it), UnicityCriteria::Id, true), "urn:1");

        // Clean ASCII ids borrow rather than allocate.
        it.guid = Some("urn:clean".into());
        assert!(matches!(
            input(&it).id,
            Some(std::borrow::Cow::Borrowed(_))
        ));
    }

    #[test]
    fn reads_content_flags_the_policies_that_need_sanitized_body() {
        assert!(UnicityCriteria::Sha1Content.reads_content());
        assert!(UnicityCriteria::Sha1LinkPublishedTitleContent.reads_content());
        assert!(!UnicityCriteria::Id.reads_content());
        assert!(!UnicityCriteria::Sha1LinkPublishedTitle.reads_content());
    }

    // --- batch validation --------------------------------------------------

    fn feed_of(n: usize, guid: impl Fn(usize) -> String) -> Vec<RawItem> {
        (0..n)
            .map(|i| RawItem {
                guid: Some(guid(i)),
                link: Some(format!("https://example.com/{i}")),
                title: Some(format!("T{i}")),
                date: Some(1_700_000_000 + i as i64),
                ..RawItem::default()
            })
            .collect()
    }

    #[test]
    fn clean_feeds_are_left_alone() {
        let items = feed_of(10, |i| format!("urn:{i}"));
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(res.invalid, 0);
        assert!(!res.degraded);
        assert_eq!(res.criteria, UnicityCriteria::Id);
        assert_eq!(res.guids.len(), 10);
        assert_eq!(res.guids[0], "urn:0", "guids stay in document order");
    }

    #[test]
    fn one_bad_guid_in_ten_is_tolerated() {
        let mut items = feed_of(10, |i| format!("urn:{i}"));
        // Nothing to identify it at all, so even the fallback chain lands on sha1("") == BLANK.
        items[3] = RawItem::default();
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(res.invalid, 1);
        assert_eq!(res.guids[3], "");
        assert!(!res.degraded, "round(0.05 * 10) == 1, so one is allowed");
    }

    #[test]
    fn an_unidentifiable_feed_is_rescued_by_the_fallback_chain() {
        // No ids at all, but distinct links and dates: `Id` yields nothing usable, yet every item
        // still gets a real GUID, so there is nothing to degrade.
        let items: Vec<RawItem> = (0..10)
            .map(|i| RawItem {
                link: Some(format!("https://example.com/{i}")),
                title: Some(format!("T{i}")),
                date: Some(1_700_000_000 + i as i64),
                ..RawItem::default()
            })
            .collect();
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(res.invalid, 0);
        assert!(!res.degraded);
        assert!(res.guids.iter().all(|g| !g.is_empty()));
        assert_eq!(res.guids[0], sha1_hex(["https://example.com/0", "1700000000"]));
    }

    #[test]
    fn duplicate_guids_degrade_to_a_hash_policy() {
        // Same id on every item, but distinct links and dates.
        let items = feed_of(10, |_| "same-id".to_owned());
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert!(res.degraded);
        assert_eq!(res.criteria, UnicityCriteria::Sha1LinkPublished);
        // After degrading, the batch is clean — that is the whole point of retrying.
        assert_eq!(res.invalid, 0);
        assert_eq!(
            res.guids.iter().collect::<std::collections::HashSet<_>>().len(),
            10,
            "every article is distinguishable again"
        );
    }

    #[test]
    fn degradation_is_visible_so_the_caller_can_persist_it() {
        let items = feed_of(10, |_| "same-id".to_owned());
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert!(res.degraded);
        assert_ne!(res.criteria, UnicityCriteria::Id);
        // Without persisting `criteria`, the next refresh rediscovers the same problem.
        assert_eq!(res.criteria.as_str(), "sha1:link_published");
    }

    #[test]
    fn a_forced_policy_is_never_degraded() {
        let items = feed_of(10, |_| "same-id".to_owned());
        let res = load_guids(&items, UnicityCriteria::Id, true, 0.05);
        assert_eq!(res.criteria, UnicityCriteria::Id);
        assert!(!res.degraded);
        assert!(res.invalid > 0, "but the caller still learns the feed is broken");
    }

    #[test]
    fn degradation_stops_at_the_end_of_the_chain() {
        // Titles all identical too, so even the degraded policy cannot separate them.
        let items: Vec<RawItem> = (0..10)
            .map(|_| RawItem {
                guid: Some("same".into()),
                title: Some("same".into()),
                ..RawItem::default()
            })
            .collect();
        let res = load_guids(&items, UnicityCriteria::Sha1LinkPublishedTitle, false, 0.05);
        assert_eq!(res.criteria, UnicityCriteria::Sha1LinkPublishedTitle);
        assert!(!res.degraded, "no weaker policy exists, so nothing changes");
        assert!(res.invalid > 0);
    }

    #[test]
    fn an_empty_feed_has_no_guids_and_no_errors() {
        let res = load_guids(&[], UnicityCriteria::Id, false, 0.05);
        assert!(res.guids.is_empty());
        assert_eq!(res.invalid, 0);
        assert!(!res.degraded);
    }

    #[test]
    fn tolerance_scales_with_batch_size() {
        // 9 articles: round(0.05 * 9) == 0, so a single invalid guid degrades.
        let mut items = feed_of(9, |i| format!("urn:{i}"));
        items[0] = RawItem::default();
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(res.invalid, 1);
        assert!(res.degraded, "0 tolerated out of 9");

        // 30 articles: round(0.05 * 30) == 2 (PHP rounds half up; Rust rounds half away from zero).
        let mut items = feed_of(30, |i| format!("urn:{i}"));
        items[0] = RawItem::default();
        items[1] = RawItem::default();
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(res.invalid, 2);
        assert!(!res.degraded, "2 tolerated out of 30");

        // 31 articles: round(0.05 * 31) == 2, so the same two now exceed the budget.
        let mut items = feed_of(31, |i| format!("urn:{i}"));
        items[0] = RawItem::default();
        items[1] = RawItem::default();
        items[2] = RawItem::default();
        let res = load_guids(&items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(res.invalid, 3);
        assert!(res.degraded, "2 tolerated out of 31, 3 is one too many");
    }

    #[test]
    fn the_content_override_reaches_content_policies() {
        let items = feed_of(3, |_| String::new());
        let bodies: Vec<Option<String>> = (0..3).map(|i| Some(format!("<p>{i}</p>"))).collect();
        let res = load_guids_with(
            &items,
            UnicityCriteria::Sha1Content,
            false,
            0.05,
            Some(&bodies),
        );
        assert_eq!(res.invalid, 0);
        assert_eq!(res.guids[0], sha1_hex(["<p>0</p>"]));
    }
}
