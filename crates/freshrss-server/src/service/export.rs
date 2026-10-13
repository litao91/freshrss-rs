//! Exporting a user's whole subscription tree as an OPML document.
//!
//! This is the read half of the portability story: the same `frss:` vocabulary the importer honours
//! is written back out, so a subscription list round-trips through this service without losing the
//! feed kind, the scraper selector maps, the priority, the signed refresh interval or the GUID
//! policy. Those are exactly the settings a generic OPML tool drops, and losing them silently turns a
//! carefully configured list into a pile of default-priority RSS feeds on the next import.
//!
//! The shape is PHP's `exportOpml`: folders become nested `<outline>`s carrying their feeds, and a
//! feed whose folder was deleted (`feed.category` is `ON DELETE SET NULL`) is written at the top
//! level rather than dropped — an orphan is still a subscription the user is entitled to take with
//! them. The one deliberate divergence is encoding: PHP HTML-encodes names at rest and decodes them
//! on the way out, while this port stores clean UTF-8, so a name is written exactly as stored.

use std::collections::{BTreeMap, HashMap};

use freshrss_feed::opml::{Opml, OpmlPriority, Outline};
use freshrss_feed::FeedKind as WireKind;

use crate::db::dao::{category as cat_dao, feed as feed_dao};
use crate::db::Db;
use crate::domain::attributes::keys;
use crate::domain::feed::Feed;
use crate::domain::ids::now_unix;
use crate::error::Result;

/// Builds the OPML document for one user's subscriptions.
///
/// # Errors
///
/// Only when the database cannot be read; there is nothing about a subscription list that can be
/// invalid enough to refuse to export it.
pub async fn opml(db: &Db, id_user: i64) -> Result<Opml> {
    let outlines = db
        .with(move |mut conn| async move {
            let categories = cat_dao::list(&mut conn, id_user).await?;
            let feeds = feed_dao::list(&mut conn, id_user).await?;

            // Bucket feeds by folder. A feed whose folder was deleted has `category = None` and is
            // written at the top level, matching the sidebar's orphan list.
            let mut by_category: HashMap<i64, Vec<&Feed>> = HashMap::new();
            let mut orphans: Vec<&Feed> = Vec::new();
            for feed in &feeds {
                match feed.category {
                    Some(cat) => {
                        by_category.entry(cat).or_default().push(feed);
                    }
                    None => orphans.push(feed),
                }
            }

            let mut outlines = Vec::with_capacity(categories.len() + orphans.len());
            for category in &categories {
                let children = by_category
                    .remove(&category.id)
                    .unwrap_or_default()
                    .iter()
                    .map(|feed| feed_outline(feed))
                    .collect();
                outlines.push(Outline::folder(category.name.as_str(), children));
            }
            // Feeds pointing at a folder that no longer exists for this user. Cannot normally happen,
            // but a hand-edited database must not lose them from the export.
            for (_, feeds) in by_category {
                orphans.extend(feeds);
            }
            for feed in orphans {
                outlines.push(feed_outline(feed));
            }
            Ok(outlines)
        })
        .await?;

    Ok(Opml {
        title: "FreshRSS subscriptions".to_owned(),
        date_created: Some(freshrss_feed::parse::date::to_rfc822(now_unix())),
        outlines,
    })
}

/// One feed as an `<outline>`, carrying every `frss:` field the writer knows how to emit.
fn feed_outline(feed: &Feed) -> Outline {
    Outline {
        text: feed.display_name().to_owned(),
        xml_url: Some(feed.url.clone()),
        html_url: feed.website.clone(),
        description: feed.description.clone(),
        // The domain and wire kinds are two enums over the same on-disk codes; the wire one degrades
        // an unknown code to plain RSS rather than failing, which is right for an export.
        kind: WireKind::from_i16(feed.kind.as_i16()),
        // `from_i8` yields `None` for the main-stream default, which the writer then omits.
        priority: OpmlPriority::from_i8(feed.priority.get()),
        // Signed: a negative interval means muted, and the writer drops only the zero default.
        ttl: Some(feed.ttl.raw()),
        unicity_criteria: feed.unicity_criteria().map(str::to_owned),
        unicity_criteria_forced: feed.unicity_criteria_forced(),
        xpath: string_map(feed.xpath()),
        json_dotnotation: string_map(feed.json_dotnotation()),
        xpath_to_json: feed
            .attributes
            .get_str(keys::XPATH_TO_JSON)
            .map(str::to_owned),
        path_entries: feed.path_entries.clone(),
        path_entries_conditions: feed
            .attributes
            .get_typed::<Vec<String>>(keys::PATH_ENTRIES_CONDITIONS)
            .unwrap_or_default(),
        path_entries_filter: feed.path_entries_filter.clone(),
        // Only the read filters survive OPML: that is the one action the format has an attribute for,
        // so star and label rules are not exported — the same asymmetry PHP has.
        filters_action_read: feed
            .filter_actions()
            .into_iter()
            .filter(|rule| rule.performs(crate::domain::filter_action::FilterActionType::Read))
            .map(|rule| rule.search)
            .collect(),
        ..Outline::default()
    }
}

/// A stored selector map (JSON object) narrowed to the string entries the OPML writer emits.
fn string_map(
    map: Option<&serde_json::Map<String, serde_json::Value>>,
) -> BTreeMap<String, String> {
    map.map(|m| {
        m.iter()
            .filter_map(|(key, value)| {
                value
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| (key.clone(), s.to_owned()))
            })
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use freshrss_feed::opml::write;

    use crate::db::dao::{category as cat_dao, feed as feed_dao, user as user_dao};
    use crate::db::support::db;
    use crate::domain::attributes::Attributes;
    use crate::domain::filter_action::{FilterAction, FilterActionType};
    use crate::domain::kind::FeedKind;
    use crate::domain::priority::Priority;
    use crate::domain::ttl::Ttl;

    use super::*;

    async fn user(db: &Db) -> i64 {
        db.with(|mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: "alice",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await
        })
        .await
        .expect("user")
    }

    async fn folder(db: &Db, id_user: i64, name: &str) -> i64 {
        let name = name.to_owned();
        db.with(move |mut conn| async move {
            let position = cat_dao::next_position(&mut conn, id_user).await?;
            cat_dao::insert(
                &mut conn,
                &cat_dao::NewCategory {
                    id_user,
                    name: &name,
                    kind: Default::default(),
                    position,
                    attributes: Attributes::new(),
                },
            )
            .await
        })
        .await
        .expect("folder")
    }

    async fn insert(db: &Db, feed: &Feed) -> Feed {
        let feed = feed.clone();
        db.with(move |mut conn| async move {
            let id = feed_dao::insert(&mut conn, &feed).await?;
            Ok(feed_dao::find_by_id(&mut conn, id, feed.id_user)
                .await?
                .expect("re-read"))
        })
        .await
        .expect("inserted")
    }

    #[tokio::test]
    async fn folders_nest_their_feeds_and_orphans_stay_at_the_top() {
        let store = db().await;
        let id_user = user(&store).await;
        let news = folder(&store, id_user, "News").await;
        insert(
            &store,
            &Feed {
                category: Some(news),
                ..Feed::new(id_user, "https://a.example/feed", "A")
            },
        )
        .await;
        // An orphan: no folder.
        insert(&store, &Feed::new(id_user, "https://b.example/feed", "B")).await;

        let doc = opml(&store, id_user).await.expect("exported");
        // The seeded Uncategorized folder, the News folder, then the orphan feed.
        let news_outline = doc
            .outlines
            .iter()
            .find(|o| o.name() == "News")
            .expect("News folder exported");
        assert_eq!(news_outline.children.len(), 1);
        assert_eq!(news_outline.children[0].name(), "A");
        assert!(
            doc.outlines.iter().any(|o| o.name() == "B" && o.is_feed()),
            "the orphan is written at the top level, not dropped: {:?}",
            doc.outlines
        );
    }

    #[tokio::test]
    async fn every_frss_field_survives_the_write() {
        let store = db().await;
        let id_user = user(&store).await;
        let mut feed = Feed::new(id_user, "https://s.example/page", "Scraper");
        feed.kind = FeedKind::HtmlXpath;
        feed.priority = Priority::IMPORTANT;
        feed.ttl = Ttl::new(3600, true);
        feed.website = "https://s.example/".into();
        feed.description = "A scraped feed".into();
        feed.path_entries = Some("article.main".into());
        feed.path_entries_filter = Some(".share".into());
        let mut xpath = serde_json::Map::new();
        xpath.insert("item".into(), "//article".into());
        xpath.insert("itemTitle".into(), ".//h1".into());
        feed.attributes.set(keys::XPATH, xpath);
        feed.attributes
            .set_str(keys::UNICITY_CRITERIA, Some("sha1:link_published"));
        feed.attributes
            .set_bool(keys::UNICITY_CRITERIA_FORCED, Some(true));
        let _ = feed.attributes.set_typed(
            keys::PATH_ENTRIES_CONDITIONS,
            &vec!["s.example/sport".to_owned()],
        );
        feed.set_filter_actions(vec![FilterAction {
            search: "intitle:ad".into(),
            actions: vec![FilterActionType::Read],
        }]);
        insert(&store, &feed).await;

        let doc = opml(&store, id_user).await.expect("exported");
        let text = write(&doc);
        // Re-parse through the wire format to assert what actually lands in the file.
        let back = freshrss_feed::opml::parse(&text).expect("parses");
        let outline = back
            .outlines
            .iter()
            .find(|o| o.name() == "Scraper")
            .expect("feed exported");
        assert_eq!(outline.kind, WireKind::HtmlXpath);
        assert_eq!(outline.priority, Some(OpmlPriority::Important));
        assert_eq!(outline.ttl, Some(-3600), "muted, signed");
        assert_eq!(outline.html_url, "https://s.example/");
        assert_eq!(outline.description, "A scraped feed");
        assert_eq!(outline.xpath["item"], "//article");
        assert_eq!(outline.xpath["itemTitle"], ".//h1");
        assert_eq!(
            outline.unicity_criteria.as_deref(),
            Some("sha1:link_published")
        );
        assert!(outline.unicity_criteria_forced);
        assert_eq!(outline.path_entries.as_deref(), Some("article.main"));
        assert_eq!(outline.path_entries_filter.as_deref(), Some(".share"));
        assert_eq!(outline.path_entries_conditions, ["s.example/sport"]);
        assert_eq!(outline.filters_action_read, ["intitle:ad"]);
    }

    #[tokio::test]
    async fn the_default_priority_and_ttl_are_omitted() {
        let store = db().await;
        let id_user = user(&store).await;
        insert(&store, &Feed::new(id_user, "https://a.example/feed", "A")).await;
        let doc = opml(&store, id_user).await.expect("exported");
        let outline = doc.outlines.iter().find(|o| o.name() == "A").expect("A");
        assert_eq!(outline.priority, None, "main stream is the omitted default");
        assert_eq!(
            outline.ttl,
            Some(0),
            "0 is written then dropped by the writer"
        );
        assert!(
            !write(&doc).contains("frss:ttl"),
            "a default ttl is not emitted"
        );
    }

    #[tokio::test]
    async fn an_empty_account_exports_a_valid_document() {
        let store = db().await;
        let id_user = user(&store).await;
        let doc = opml(&store, id_user).await.expect("exported");
        // Only the seeded Uncategorized folder, with no feeds in it.
        assert_eq!(doc.outlines.len(), 1);
        assert!(doc.outlines[0].children.is_empty());
        assert!(freshrss_feed::opml::parse(&write(&doc)).is_ok());
    }
}
