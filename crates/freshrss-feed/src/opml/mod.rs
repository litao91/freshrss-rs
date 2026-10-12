//! OPML: the subscription list format every reader speaks.
//!
//! Hand-rolled on `quick-xml` rather than using an OPML crate, for one reason: the parts that matter
//! to this project live in FreshRSS's `frss:` namespace — feed kind, priority, TTL, the XPath and
//! dot-notation maps, the full-text CSS selectors, the read filters — and a generic crate has nowhere
//! to put them. Round-tripping a subscription list through a tool that drops those attributes turns a
//! careful setup into a pile of default-priority RSS feeds, which is the one import failure a user
//! cannot see until weeks later.
//!
//! The attribute names and the omission rules are FreshRSS's, read off its export view and import
//! service: `frss:priority` is a word rather than a number and is omitted when it is the default,
//! `frss:ttl` is signed with negative meaning muted, and `frss:unicityCriteria` is omitted when it is
//! `id`. Matching those exactly is what makes an export from a PHP FreshRSS import cleanly here and
//! vice versa.

mod model;
mod parse;
mod write;

pub use model::{kind_of_type, type_of_kind, Opml, OpmlPriority, Outline, XPATH_KEYS};
pub use parse::{parse, OpmlError};
pub use write::write;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::kind::FeedKind;

    /// Every field the exporter writes, filled in — with the two selector maps populated only for
    /// the kinds that read them, so the result is exactly what a round trip can give back.
    fn decorated(kind: FeedKind) -> Outline {
        let uses_xpath = matches!(kind, FeedKind::HtmlXpath | FeedKind::XmlXpath);
        let uses_json = matches!(
            kind,
            FeedKind::JsonDotNotation | FeedKind::HtmlXpathJsonDotNotation
        );
        Outline {
            text: "Café & Thé <3".to_owned(),
            xml_url: Some("https://e.example/feed.xml?a=1&b=2".to_owned()),
            html_url: "https://e.example/".to_owned(),
            description: "An example \"feed\"".to_owned(),
            kind,
            priority: Some(OpmlPriority::Hidden),
            ttl: Some(-3600),
            unicity_criteria: Some("sha1:link_published_title".to_owned()),
            unicity_criteria_forced: true,
            xpath: if uses_xpath {
                BTreeMap::from([
                    ("item".to_owned(), "//article".to_owned()),
                    ("itemUid".to_owned(), ".//data-id".to_owned()),
                ])
            } else {
                BTreeMap::new()
            },
            json_dotnotation: if uses_json {
                BTreeMap::from([("item".to_owned(), "items".to_owned())])
            } else {
                BTreeMap::new()
            },
            xpath_to_json: (kind == FeedKind::HtmlXpathJsonDotNotation)
                .then(|| "//script[@type='application/json']".to_owned()),
            path_entries: Some("article.main".to_owned()),
            path_entries_conditions: vec!["html.example/sport".to_owned()],
            path_entries_filter: Some(".share".to_owned()),
            filters_action_read: vec!["intitle:ad".to_owned(), "author:bot".to_owned()],
            ..Outline::default()
        }
    }

    #[test]
    fn a_document_survives_a_round_trip() {
        let doc = Opml {
            title: "Café & subscriptions".to_owned(),
            date_created: Some("Mon, 12 Oct 2026 08:30:00 +0000".to_owned()),
            outlines: vec![Outline {
                text: "News".to_owned(),
                children: vec![
                    decorated(FeedKind::HtmlXpath),
                    decorated(FeedKind::HtmlXpathJsonDotNotation),
                    decorated(FeedKind::Rss),
                ],
                ..Outline::default()
            }],
        };

        let text = write(&doc);
        let back = parse(&text).expect("our own output must parse");
        assert_eq!(back, doc, "{text}");
    }

    #[test]
    fn every_kind_that_has_a_type_word_round_trips() {
        for kind in FeedKind::ALL {
            let doc = Opml {
                outlines: vec![decorated(kind)],
                ..Opml::default()
            };
            let back = parse(&write(&doc)).unwrap();
            let expected = kind_of_type(type_of_kind(kind));
            assert_eq!(back.outlines[0].kind, expected, "{kind:?}");
            // Only the two kinds with no word of their own come back as plain RSS.
            assert!(
                expected == kind || matches!(kind, FeedKind::RssForced | FeedKind::JsonXpath),
                "{kind:?} exported as {:?} and came back as {expected:?}",
                type_of_kind(kind)
            );
        }
    }

    #[test]
    fn each_selector_map_survives_only_under_the_kind_that_uses_it() {
        let xpath_kind = parse(&write(&Opml {
            outlines: vec![decorated(FeedKind::XmlXpath)],
            ..Opml::default()
        }))
        .unwrap();
        assert_eq!(xpath_kind.outlines[0].xpath.len(), 2);
        assert!(xpath_kind.outlines[0].json_dotnotation.is_empty());

        let json_kind = parse(&write(&Opml {
            outlines: vec![decorated(FeedKind::JsonDotNotation)],
            ..Opml::default()
        }))
        .unwrap();
        assert!(json_kind.outlines[0].xpath.is_empty());
        assert_eq!(json_kind.outlines[0].json_dotnotation["item"], "items");
        assert_eq!(json_kind.outlines[0].xpath_to_json, None);
    }

    #[test]
    fn nesting_is_preserved() {
        let doc = Opml {
            outlines: vec![Outline::folder(
                "outer",
                vec![Outline::folder(
                    "inner",
                    vec![Outline::feed("leaf", "https://l.example/")],
                )],
            )],
            ..Opml::default()
        };
        let back = parse(&write(&doc)).unwrap();
        assert_eq!(back.outlines[0].name(), "outer");
        assert_eq!(back.outlines[0].children[0].name(), "inner");
        assert_eq!(back.outlines[0].children[0].children[0].name(), "leaf");
    }

    #[test]
    fn a_php_freshrss_export_imports() {
        // Written by hand in the shape `app/views/helpers/export/opml.phtml` produces: tab-indented,
        // attributes in insertion order, defaults omitted.
        let php = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                   <opml xmlns:frss=\"https://freshrss.org/opml\" version=\"2.0\">\n\
                   \t<head>\n\
                   \t\t<title>FreshRSS</title>\n\
                   \t\t<dateCreated>Sun, 11 Oct 2026 19:04:22 +0000</dateCreated>\n\
                   \t</head>\n\
                   \t<body>\n\
                   \t\t<outline text=\"Uncategorized\">\n\
                   \t\t\t<outline text=\"Example\" type=\"rss\" xmlUrl=\"https://e.example/feed\" htmlUrl=\"https://e.example/\" description=\"Desc\" frss:priority=\"important\" frss:ttl=\"900\"/>\n\
                   \t\t\t<outline text=\"Muted\" type=\"rss\" xmlUrl=\"https://m.example/feed\" htmlUrl=\"https://m.example/\" frss:ttl=\"-3600\"/>\n\
                   \t\t</outline>\n\
                   \t</body>\n\
                   </opml>\n";
        let doc = parse(php).unwrap();
        assert_eq!(doc.title, "FreshRSS");
        let feeds = &doc.outlines[0].children;
        assert_eq!(feeds[0].priority, Some(OpmlPriority::Important));
        assert_eq!(feeds[0].priority.map(|p| p.as_i8()), Some(20));
        assert_eq!(feeds[0].ttl, Some(900));
        assert_eq!(feeds[1].ttl, Some(-3600));
        assert_eq!(feeds[1].priority, None, "absent means the main stream");
    }
}
