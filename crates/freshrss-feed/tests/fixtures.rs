//! Fixture-driven tests over `fixtures/feeds/`.
//!
//! These assert the corpus as a whole, so adding a fixture file without an assertion is a visible
//! failure rather than silent dead weight. The per-shape unit tests live next to each parser; this
//! file is about the properties that must hold for *every* feed we claim to support.

use std::path::{Path, PathBuf};

use freshrss_feed::guid::{decide_guid, load_guids, GuidInput, UnicityCriteria};
use freshrss_feed::http::body_hash;
use freshrss_feed::sanitize::{sanitize_html, SanitizeOptions};
use freshrss_feed::{parse_feed, FeedKind, RawFeed};

/// An event-handler attribute: `on` followed by a name and an `=`, inside a tag.
static EVENT_HANDLER: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(r"<[^>]*\son[a-z]+\s*=").expect("static pattern")
});

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/feeds")
        .canonicalize()
        .expect("fixtures/feeds exists")
}

/// Every fixture directory, so a new one is picked up automatically.
fn all_feeds() -> Vec<(String, Vec<u8>, FeedKind)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).expect("readable") {
        let dir = entry.expect("readable entry").path();
        if !dir.is_dir() {
            continue;
        }
        let slug = dir.file_name().unwrap().to_string_lossy().into_owned();
        let body = ["body.xml", "body.json"]
            .iter()
            .map(|name| dir.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{slug} has no body.xml or body.json"));
        let bytes = std::fs::read(&body).expect("readable body");
        let kind = if slug.starts_with("jsonfeed") {
            FeedKind::JsonFeed
        } else {
            FeedKind::Rss
        };
        out.push((slug, bytes, kind));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        out.len() >= 9,
        "expected the whole corpus, found {}",
        out.len()
    );
    out
}

fn parse(slug: &str, body: &[u8], kind: FeedKind) -> RawFeed {
    parse_feed(body, kind).unwrap_or_else(|e| panic!("{slug}: {e}"))
}

fn options_for(feed: &RawFeed) -> SanitizeOptions {
    SanitizeOptions {
        base_url: feed
            .base_url
            .clone()
            .or_else(|| feed.self_url.clone())
            .unwrap_or_else(|| "https://example.com/feed".to_owned()),
        force_https_domains: Vec::new(),
        max_length: None,
    }
}

#[test]
fn every_fixture_parses_and_has_a_title() {
    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        assert!(
            feed.title.as_deref().is_some_and(|t| !t.trim().is_empty()),
            "{slug}: no feed title"
        );
    }
}

#[test]
fn every_fixture_keeps_every_item_it_declares() {
    // Guards against a walk that runs past its own end tag and swallows the rest of the document,
    // which is exactly what a `media:group` regression did.
    let expected = [
        ("atom-basic", 3),
        ("atom-xhtml", 3),
        ("hostile", 15),
        ("jsonfeed-basic", 4),
        ("rdf-basic", 2),
        ("rss2-bad-dates", 12),
        ("rss2-basic", 2),
        ("rss2-content-encoded", 4),
        ("rss2-media", 4),
        ("rss2-no-guid", 5),
        ("rss2-werss", 3),
    ];
    for (slug, want) in expected {
        let dir = fixtures_dir().join(slug);
        let path = ["body.xml", "body.json"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{slug} has no body"));
        let body = std::fs::read(&path).unwrap();
        let kind = if slug.starts_with("jsonfeed") {
            FeedKind::JsonFeed
        } else {
            FeedKind::Rss
        };
        let feed = parse(slug, &body, kind);
        let declared = String::from_utf8_lossy(&body).matches("<item>").count()
            + String::from_utf8_lossy(&body).matches("<entry>").count();
        let json_items = if kind == FeedKind::JsonFeed {
            serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("items").and_then(|i| i.as_array()).map(|a| a.len()))
                .unwrap_or(0)
        } else {
            0
        };
        let want = want.max(declared).max(json_items);
        assert_eq!(
            feed.items.len(),
            want,
            "{slug}: parsed {} items but the document declares {want}",
            feed.items.len()
        );
    }
}

#[test]
fn parsing_the_same_body_twice_is_identical() {
    for (slug, body, kind) in all_feeds() {
        let a = parse(&slug, &body, kind);
        let b = parse(&slug, &body, kind);
        assert_eq!(a, b, "{slug}: parsing is not deterministic");
    }
}

#[test]
fn every_item_has_a_non_empty_guid() {
    // An empty GUID cannot be stored: `UNIQUE(id_feed, guid)` would collapse every such article into
    // one row, and the reader would silently lose entries.
    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        for (i, item) in feed.items.iter().enumerate() {
            let guid = decide_guid(
                &GuidInput::from_item(item, item.effective_content()),
                UnicityCriteria::Id,
                true,
            );
            assert!(
                !guid.trim().is_empty(),
                "{slug} item {i} has no usable GUID"
            );
        }
    }
}

#[test]
fn guids_are_stable_across_two_parses_of_the_same_body() {
    for (slug, body, kind) in all_feeds() {
        let first = parse(&slug, &body, kind);
        let second = parse(&slug, &body, kind);
        for criteria in [
            UnicityCriteria::Id,
            UnicityCriteria::Link,
            UnicityCriteria::Sha1LinkPublished,
            UnicityCriteria::Sha1LinkPublishedTitleContent,
        ] {
            let guids = |feed: &RawFeed| {
                feed.items
                    .iter()
                    .map(|it| {
                        decide_guid(
                            &GuidInput::from_item(it, it.effective_content()),
                            criteria,
                            true,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                guids(&first),
                guids(&second),
                "{slug} / {:?}: GUIDs moved between two parses of identical bytes",
                criteria
            );
        }
    }
}

#[test]
fn the_unicity_policy_never_needs_degrading_for_a_well_behaved_feed() {
    for (slug, body, kind) in all_feeds() {
        if slug == "rss2-no-guid" {
            // That fixture exists to contain duplicate and missing GUIDs on purpose.
            continue;
        }
        let feed = parse(&slug, &body, kind);
        let resolution = load_guids(&feed.items, UnicityCriteria::Id, false, 0.05);
        assert_eq!(
            resolution.invalid, 0,
            "{slug}: {} invalid GUIDs would degrade the policy to {:?}",
            resolution.invalid, resolution.criteria
        );
        assert!(!resolution.degraded, "{slug}");
    }
}

#[test]
fn the_duplicate_guid_fixture_degrades_the_policy_until_the_batch_is_clean() {
    let dir = fixtures_dir().join("rss2-no-guid");
    let body = std::fs::read(dir.join("body.xml")).unwrap();
    let feed = parse("rss2-no-guid", &body, FeedKind::Rss);

    let resolution = load_guids(&feed.items, UnicityCriteria::Id, false, 0.05);
    // Two items share `urn:dup`, so `id` cannot separate them and the policy steps down. It keeps
    // stepping until the batch is clean, which is why `invalid` is 0 on the way out: degradation is
    // the mechanism that fixes it.
    assert!(
        resolution.degraded,
        "the policy should have stepped down from `id`"
    );
    assert_ne!(resolution.criteria, UnicityCriteria::Id);
    assert_eq!(
        resolution.invalid, 0,
        "degrading should have produced a usable GUID for every item"
    );
    let unique: std::collections::HashSet<_> = resolution.guids.iter().collect();
    assert_eq!(
        unique.len(),
        feed.items.len(),
        "every article is distinguishable again: {:?}",
        resolution.guids
    );

    // A user who explicitly chose `id` has said they know what this feed does, so it is not
    // overruled — but the caller still learns the batch is broken.
    let forced = load_guids(&feed.items, UnicityCriteria::Id, true, 0.05);
    assert!(!forced.degraded);
    assert_eq!(forced.criteria, UnicityCriteria::Id);
    assert!(forced.invalid > 0);
}

#[test]
fn every_item_has_something_to_open_or_to_read() {
    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        for (i, item) in feed.items.iter().enumerate() {
            assert!(
                !item.is_empty(),
                "{slug} item {i} is empty and should have been dropped"
            );
        }
    }
}

#[test]
fn every_enclosure_and_thumbnail_is_remote() {
    // These URLs end up in an `<audio src>` or an `<img src>` the reader loads, so a `file:` or
    // `data:` value that survived parsing would be a live problem rather than a cosmetic one.
    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        for item in &feed.items {
            for e in &item.enclosures {
                assert!(
                    e.url.starts_with("http://") || e.url.starts_with("https://"),
                    "{slug}: non-remote enclosure {:?}",
                    e.url
                );
            }
            if let Some(t) = &item.thumbnail {
                assert!(
                    t.url.starts_with("http://") || t.url.starts_with("https://"),
                    "{slug}: non-remote thumbnail {:?}",
                    t.url
                );
            }
        }
    }
}

#[test]
fn sanitizing_every_body_removes_the_dangerous_constructs() {
    const BANNED: &[&str] = &[
        "<script",
        "javascript:",
        "vbscript:",
        "file://",
        "blob:",
        "moz-extension:",
        "<object",
        "<embed",
        "<applet",
        "<form",
        "<input",
        "<base",
        "<meta",
        "<link",
        "<style",
        "style=",
        "srcdoc=",
        "data:image/svg",
        "<!--",
    ];

    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        let options = options_for(&feed);
        for (i, item) in feed.items.iter().enumerate() {
            let Some(raw) = item.effective_content() else {
                continue;
            };
            let clean = sanitize_html(raw, &options).to_ascii_lowercase();
            for banned in BANNED {
                assert!(
                    !clean.contains(banned),
                    "{slug} item {i}: {banned:?} survived sanitization\n{clean}"
                );
            }
            // Event handlers, matched structurally so `onerror=`/`onclick=`/`onload=` are all caught.
            for attr in clean.split_whitespace() {
                let name = attr.split('=').next().unwrap_or("");
                let name = name.trim_start_matches('<');
                assert!(
                    !name.starts_with("on") || !attr.contains('='),
                    "{slug} item {i}: event handler {attr:?} survived"
                );
            }
            assert!(
                !clean.contains(" id=") && !clean.contains(" class="),
                "{slug} item {i}: an id or class survived, so a feed could target the reader's CSS"
            );
        }
    }
}

#[test]
fn sanitizing_converges_after_one_pass() {
    // The property that actually matters: a stored body that gets sanitized again must not keep
    // changing, or its change-detection hash churns and every article looks updated.
    //
    // Strict idempotence does not hold, and cannot cheaply: an attribute already present in the
    // input keeps its position in html5ever's attribute map, while one the sanitizer adds is
    // appended. So `<iframe src=…>` and the same iframe re-sanitized differ in *attribute order* on
    // the first pass and agree from the second onward. Semantics are identical; only byte order
    // moves, and only once.
    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        let options = options_for(&feed);
        for (i, item) in feed.items.iter().enumerate() {
            let Some(raw) = item.effective_content() else {
                continue;
            };
            let twice = sanitize_html(&sanitize_html(raw, &options), &options);
            let thrice = sanitize_html(&twice, &options);
            assert_eq!(
                twice, thrice,
                "{slug} item {i}: sanitizing does not converge\n--- 2 ---\n{twice}\n--- 3 ---\n{thrice}"
            );
        }
    }
}

#[test]
fn the_body_fingerprint_ignores_only_the_volatile_parts() {
    for (slug, body, _kind) in all_feeds() {
        let hash = body_hash(&body);
        assert_eq!(hash.len(), 40, "{slug}: expected a SHA-1 hex digest");
        assert_eq!(hash, body_hash(&body), "{slug}: not stable");

        // Re-serializing the same items with a different `<lastBuildDate>` must not look like a
        // change, or every refresh re-sanitizes and re-hashes the whole backlog.
        let text = String::from_utf8_lossy(&body).into_owned();
        if let Some(at) = text.find("<lastBuildDate>") {
            let end = text[at..].find("</lastBuildDate>").map(|e| at + e).unwrap();
            let edited = format!(
                "{}<lastBuildDate>Tue, 03 Jan 2206 00:00:00 +0000</lastBuildDate>{}",
                &text[..at],
                &text[end + "</lastBuildDate>".len()..]
            );
            assert_eq!(
                body_hash(edited.as_bytes()),
                hash,
                "{slug}: lastBuildDate leaked into the fingerprint"
            );
        }
    }
}

#[test]
fn no_item_title_carries_an_executable_construct() {
    // Atom `type="html"` titles legitimately contain markup — `<title type="html">An &lt;em&gt;x
    // &lt;/em&gt;</title>` is real text, and stripping it here would lose the italics. What must not
    // appear is anything executable: the reader renders a title through `strip_html_tags`, so a
    // `<script>` would be inert, but the title also feeds the GUID and the change-detection hash,
    // where an unexpected construct is a correctness problem.
    for (slug, body, kind) in all_feeds() {
        if slug == "hostile" {
            // Its titles *describe* the attacks ("javascript: URLs in every URL-bearing attribute"),
            // so the attack strings legitimately appear in them as prose.
            continue;
        }
        let feed = parse(&slug, &body, kind);
        for item in &feed.items {
            if let Some(title) = &item.title {
                let lowered = title.to_ascii_lowercase();
                for banned in ["<script", "javascript:", "<iframe", "<object", "<embed"] {
                    assert!(
                        !lowered.contains(banned),
                        "{slug}: title {title:?} contains {banned:?}"
                    );
                }
                // A real event handler, not a bare " on" substring — "Summary only" would match that.
                assert!(
                    !EVENT_HANDLER.is_match(&lowered),
                    "{slug}: title {title:?} carries an event handler"
                );
                assert!(
                    !title.contains('\n') && !title.contains('\t'),
                    "{slug}: title {title:?} was not whitespace-normalized"
                );
            }
        }
    }
}

#[test]
fn dates_that_parse_are_within_a_sane_range() {
    // A date parsed as milliseconds-instead-of-seconds lands thousands of years out and then sorts
    // above every real article forever, so this is the check that catches that mistake.
    // The upper bound is loose on purpose: `rss2-bad-dates` contains a year-2206 date, and FreshRSS
    // keeps future dates rather than clamping them. What this test actually catches is a
    // milliseconds-value read as seconds, which lands somewhere around 10^13 — four orders of
    // magnitude beyond anything here.
    let min = -2_208_988_800; // 1900-01-01
    let max = 10_000_000_000; // year 2286
    for (slug, body, kind) in all_feeds() {
        let feed = parse(&slug, &body, kind);
        for item in &feed.items {
            for (label, value) in [("date", item.date), ("updated", item.updated)] {
                if let Some(t) = value {
                    assert!(
                        (min..=max).contains(&t),
                        "{slug}: {label} {t} is outside 1900..2100"
                    );
                }
            }
        }
    }
}

#[test]
fn a_feed_that_declares_no_items_is_still_a_feed() {
    // An empty feed is a legitimate state; treating it as a parse failure would mark a healthy
    // subscription as broken.
    let empty =
        br#"<?xml version="1.0"?><rss version="2.0"><channel><title>Empty</title></channel></rss>"#;
    let feed = parse_feed(empty, FeedKind::Rss).unwrap();
    assert!(feed.items.is_empty());
    assert_eq!(feed.title.as_deref(), Some("Empty"));
}

#[test]
fn items_stay_in_document_order_and_chronological_reverses_them() {
    let dir = fixtures_dir().join("rss2-basic");
    let body = std::fs::read(dir.join("body.xml")).unwrap();
    let feed = parse("rss2-basic", &body, FeedKind::Rss);
    let titles: Vec<_> = feed
        .items
        .iter()
        .filter_map(|i| i.title.as_deref())
        .collect();
    assert_eq!(
        titles,
        vec!["Second article", "First article"],
        "newest first"
    );

    let reversed: Vec<_> = feed
        .chronological()
        .iter()
        .filter_map(|i| i.title.as_deref())
        .collect();
    assert_eq!(reversed, vec!["First article", "Second article"]);
}

/// Keeps the helper honest: a fixture with no items would make several tests above pass vacuously.
#[test]
fn the_corpus_actually_contains_items() {
    let total: usize = all_feeds()
        .iter()
        .map(|(slug, body, kind)| parse(slug, body, *kind).items.len())
        .sum();
    assert!(total >= 50, "corpus only produced {total} items");
}

/// The WeChat bridge (we-mp-rss) shape: everything about it is slightly off-spec, and every one of
/// these quirks is something a stricter parser would trip over — a non-standard per-item `<id>`,
/// `guid` equal to the link, `content:encoded` escaped rather than CDATA-wrapped, covers as
/// zero-length enclosures and `+0800` on every date.
#[test]
fn a_wechat_bridge_feed_ingests_cleanly() {
    let dir = fixtures_dir().join("rss2-werss");
    let body = std::fs::read(dir.join("body.xml")).unwrap();
    let feed = parse("rss2-werss", &body, FeedKind::Rss);
    assert_eq!(feed.items.len(), 3);

    let first = &feed.items[0];
    // The stray `<id>` must not shadow the real identity: the guid is the link, as the bridge emits.
    assert_eq!(
        first.guid.as_deref(),
        Some("https://mp.weixin.qq.com/s/abc123")
    );
    // Escaped HTML in content:encoded comes back as markup, not as visible entities.
    let content = first.effective_content().expect("a body");
    assert!(content.contains("<strong>WeChat</strong>"), "{content}");
    assert!(!content.contains("&lt;p&gt;"), "{content}");
    // The zero-length enclosure is still a usable cover image.
    assert_eq!(
        first.thumbnail.as_ref().map(|t| t.url.as_str()),
        Some("https://mmbiz.qpic.cn/cover1.jpg")
    );
    // 10:00 +0800 is 02:00Z; a parser that assumed UTC would be eight hours off.
    assert_eq!(first.date, Some(1_791_856_800));

    assert!(
        feed.items[2].thumbnail.is_none(),
        "an item without an enclosure must not invent a cover"
    );
}
