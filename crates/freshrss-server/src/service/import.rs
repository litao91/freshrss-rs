//! Importing a subscription list from an OPML document.
//!
//! This is the write half of the portability story, and the mirror of [`crate::service::export`]:
//! every `frss:` field the exporter writes is honoured on the way back in, so a list round-trips
//! without collapsing to default-priority RSS feeds. The rules are PHP's `importOpml` plus two
//! invariants this port adds:
//!
//! - **Import never fetches.** PHP's `addFeed` loads a feed before inserting it; here an import only
//!   records subscriptions, because pulling a hundred feeds inline would make an upload take minutes
//!   and fail wholesale on the first dead address. The scheduler refreshes them afterwards.
//! - **A duplicate is skipped, not fatal.** Re-importing a list is how people restore a backup, and
//!   the second run must add what is new and quietly leave what already exists, reporting the count.
//!
//! Malformed input is a validation error rather than a panic: an upload is untrusted, and a document
//! that will not parse has to come back as a 400 the UI can show, not as a crashed request.

use std::collections::HashMap;

use freshrss_feed::opml::Outline;

use crate::db::dao::{category as cat_dao, feed as feed_dao};
use crate::db::Db;
use crate::domain::attributes::{keys, Attributes};
use crate::domain::category::Category;
use crate::domain::feed::Feed;
use crate::domain::filter_action::{FilterAction, FilterActionType};
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;
use crate::error::{Error, Result};
use crate::service::subscription::{self, normalize_url, NewSubscription};

/// What one import did, or (in a dry run) would do.
///
/// Returned to the caller verbatim as JSON, so the field names are the API's: the import dialog shows
/// the counts and lists the per-feed problems, which is why a rejection is recorded as a string here
/// rather than aborting the whole run.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportReport {
    pub created_feeds: usize,
    pub skipped_feeds: usize,
    pub created_categories: usize,
    pub errors: Vec<String>,
}

/// Imports an OPML document into one user's subscriptions.
///
/// `dry_run` performs every check — parsing, URL normalization, duplicate detection, folder
/// resolution — and reports what *would* happen while writing nothing, so the dialog can preview an
/// upload before committing it. `force_category` files every feed under one folder regardless of the
/// document's own nesting, which is what the "import into this folder" affordance needs.
///
/// # Errors
///
/// [`Error::Validation`] when the bytes are not UTF-8 or not a parseable OPML document, and
/// [`Error::NotFound`] when `force_category` names a folder the user does not own. Per-feed problems
/// (a rejected URL, an unusable folder name) are collected into [`ImportReport::errors`] instead, so
/// one bad line does not discard the rest of the upload.
pub async fn opml(
    db: &Db,
    id_user: i64,
    document: &[u8],
    dry_run: bool,
    force_category: Option<i64>,
) -> Result<ImportReport> {
    let text = std::str::from_utf8(document)
        .map_err(|err| Error::Validation(format!("the OPML document is not valid UTF-8: {err}")))?;
    let doc = freshrss_feed::opml::parse(text).map_err(|err| Error::Validation(err.to_string()))?;

    // A forced folder is validated once up front rather than per feed: filing everything under a
    // folder the user does not own is a whole-import mistake, not a per-line one.
    if let Some(id) = force_category {
        let owned = db
            .with(move |mut conn| async move {
                Ok(cat_dao::find_by_id(&mut conn, id, id_user).await?.is_some())
            })
            .await?;
        if !owned {
            return Err(Error::NotFound(format!("folder {id}")));
        }
    }

    // Existing folders by name, so a document that names a folder twice (or re-imports one) resolves
    // to the same row instead of tripping the `UNIQUE(id_user, name)` constraint.
    let mut by_name: HashMap<String, i64> = db
        .with(move |mut conn| async move {
            Ok(cat_dao::list(&mut conn, id_user)
                .await?
                .into_iter()
                .map(|category| (category.name, category.id))
                .collect())
        })
        .await?;

    let mut report = ImportReport::default();
    // Negative ids stand in for folders a dry run would create; they are never inserted, only
    // counted, and the sign keeps them from colliding with a real rowid.
    let mut synthetic = 0i64;

    let mut feeds: Vec<(Outline, Option<String>)> = Vec::new();
    collect(&doc.outlines, None, &mut feeds);

    for (outline, folder_name) in feeds {
        let Some(raw_url) = outline.xml_url.as_deref() else {
            continue;
        };
        let url = match normalize_url(raw_url) {
            Ok(url) => url,
            Err(err) => {
                report.errors.push(err.to_string());
                continue;
            }
        };

        let duplicate = {
            let url = url.clone();
            db.with(move |mut conn| async move {
                Ok(feed_dao::find_by_url(&mut conn, id_user, &url)
                    .await?
                    .is_some())
            })
            .await?
        };
        if duplicate {
            report.skipped_feeds += 1;
            continue;
        }

        let category = match force_category {
            Some(id) => Some(id),
            None => match &folder_name {
                Some(name) => {
                    resolve_folder(
                        db,
                        id_user,
                        name,
                        dry_run,
                        &mut by_name,
                        &mut synthetic,
                        &mut report,
                    )
                    .await?
                }
                None => None,
            },
        };

        let spec = NewSubscription {
            id_user,
            url,
            name: display_name(&outline),
            category,
            kind: FeedKind::from_i16(outline.kind.as_i16()).unwrap_or_default(),
            priority: outline
                .priority
                .map(|priority| Priority::new(priority.as_i8()))
                .unwrap_or_default(),
            // Signed: a negative interval is a muted feed, and `Ttl::new` keeps the magnitude while
            // re-applying the sign, so `frss:ttl="-3600"` imports muted exactly as it exported.
            ttl: outline
                .ttl
                .map(|raw| Ttl::new(raw, raw < 0))
                .unwrap_or_default(),
        };

        if dry_run {
            report.created_feeds += 1;
            continue;
        }

        match subscription::create(db, &spec).await {
            Ok(feed) => {
                apply_outline_settings(db, &feed, &outline).await?;
                report.created_feeds += 1;
            }
            // The pre-check above makes this rare; a concurrent import that won the race is still a
            // duplicate, and reporting it as skipped is truer than failing the whole upload.
            Err(Error::Conflict(_)) => report.skipped_feeds += 1,
            Err(err) => return Err(err),
        }
    }

    Ok(report)
}

/// Flattens the outline tree into `(feed, folder-name)` pairs.
///
/// Folders are not rows of their own here: a feed is filed under the name of the folder directly
/// containing it, and a folder with no feeds of its own produces nothing. That matches PHP, whose
/// categories are flat too, so a deeply nested document collapses to its leaf folder names rather
/// than inventing a hierarchy the schema cannot store. A top-level feed with no enclosing folder
/// falls back to the first name in its OPML `category` attribute, which is how the readers that tag
/// instead of nest (Feedly, Inoreader) express a folder.
fn collect<'a>(
    outlines: &'a [Outline],
    parent: Option<&'a str>,
    out: &mut Vec<(Outline, Option<String>)>,
) {
    for outline in outlines {
        let name = outline.name().trim();
        let named = (!name.is_empty()).then_some(name);
        if outline.is_feed() {
            let folder = parent
                .map(str::to_owned)
                .or_else(|| outline.category.first().cloned());
            out.push((outline.clone(), folder));
        }
        if !outline.children.is_empty() {
            collect(&outline.children, named.or(parent), out);
        }
    }
}

/// The stored name for a feed outline: `None` when the document gave none, so the first refresh fills
/// in the title the feed publishes rather than an address-derived guess.
fn display_name(outline: &Outline) -> Option<String> {
    let name = outline.name().trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Resolves a folder name to an id, creating the folder unless this is a dry run.
///
/// An unusable name is recorded as an error and yields `None`, filing the feed under the user's
/// default folder — the same recovery PHP uses when it cannot create the folder it wanted.
async fn resolve_folder(
    db: &Db,
    id_user: i64,
    name: &str,
    dry_run: bool,
    by_name: &mut HashMap<String, i64>,
    synthetic: &mut i64,
    report: &mut ImportReport,
) -> Result<Option<i64>> {
    let normalized = match Category::normalize_name(name) {
        Ok(normalized) => normalized,
        Err(reason) => {
            report.errors.push(format!("folder {name:?}: {reason}"));
            return Ok(None);
        }
    };
    if let Some(id) = by_name.get(&normalized) {
        return Ok(Some(*id));
    }
    if dry_run {
        *synthetic -= 1;
        by_name.insert(normalized, *synthetic);
        report.created_categories += 1;
        return Ok(Some(*synthetic));
    }
    let id = create_folder(db, id_user, &normalized).await?;
    by_name.insert(normalized, id);
    report.created_categories += 1;
    Ok(Some(id))
}

/// Creates a folder, or returns the existing one if a concurrent import got there first.
async fn create_folder(db: &Db, id_user: i64, name: &str) -> Result<i64> {
    let name = name.to_owned();
    db.with(move |mut conn| async move {
        if let Some(existing) = cat_dao::find_by_name(&mut conn, id_user, &name).await? {
            return Ok(existing.id);
        }
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
}

/// Writes the parts of an outline that [`subscription::create`] does not carry: the scraper selector
/// maps, the GUID policy, the full-text selectors and the read filters.
///
/// A second write rather than a wider `NewSubscription`, because that struct is shared with the
/// add-feed dialog, which has none of these to supply. The update is skipped entirely for a plain RSS
/// feed with nothing configured, which is the common case in a typical subscription list.
async fn apply_outline_settings(db: &Db, feed: &Feed, outline: &Outline) -> Result<()> {
    let mut attributes = feed.attributes.clone();
    let mut changed = false;
    if !outline.xpath.is_empty() {
        let _ = attributes.set_typed(keys::XPATH, &outline.xpath);
        changed = true;
    }
    if !outline.json_dotnotation.is_empty() {
        let _ = attributes.set_typed(keys::JSON_DOTNOTATION, &outline.json_dotnotation);
        changed = true;
    }
    if let Some(value) = &outline.xpath_to_json {
        attributes.set_str(keys::XPATH_TO_JSON, Some(value));
        changed = true;
    }
    if let Some(criteria) = &outline.unicity_criteria {
        attributes.set_str(keys::UNICITY_CRITERIA, Some(criteria));
        changed = true;
    }
    if outline.unicity_criteria_forced {
        attributes.set_bool(keys::UNICITY_CRITERIA_FORCED, Some(true));
        changed = true;
    }
    if !outline.path_entries_conditions.is_empty() {
        let _ = attributes.set_typed(
            keys::PATH_ENTRIES_CONDITIONS,
            &outline.path_entries_conditions,
        );
        changed = true;
    }
    if !outline.filters_action_read.is_empty() {
        // OPML carries only the search string, so each line becomes a rule whose sole action is
        // `read`. Star and label rules have no OPML attribute and are neither exported nor imported.
        let filters: Vec<FilterAction> = outline
            .filters_action_read
            .iter()
            .map(|search| FilterAction {
                search: search.clone(),
                actions: vec![FilterActionType::Read],
            })
            .collect();
        let _ = attributes.set_typed(keys::FILTERS, &filters);
        changed = true;
    }

    let path_entries = nonblank_owned(outline.path_entries.as_deref());
    let path_filter = nonblank_owned(outline.path_entries_filter.as_deref());
    // The website and description are not part of `NewSubscription`, so a freshly created feed starts
    // with both blank; carrying them over here means an export re-imports with its human-readable
    // identity intact rather than waiting for the first refresh to rediscover it.
    let website = nonblank_owned(Some(outline.html_url.as_str()));
    let description = nonblank_owned(Some(outline.description.as_str()));
    let scraping = path_entries.is_some() || path_filter.is_some();
    let identity = website.is_some() || description.is_some();
    if !changed && !scraping && !identity {
        return Ok(());
    }

    let (id, id_user) = (feed.id, feed.id_user);
    db.with_writer(move |mut conn| async move {
        if changed {
            feed_dao::update_attributes(&mut conn, id, id_user, &attributes).await?;
        }
        if scraping {
            feed_dao::update_scraping(
                &mut conn,
                id,
                id_user,
                path_entries.as_deref(),
                path_filter.as_deref(),
            )
            .await?;
        }
        if identity {
            feed_dao::update_website_description(
                &mut conn,
                id,
                id_user,
                website.as_deref().unwrap_or_default(),
                description.as_deref().unwrap_or_default(),
            )
            .await?;
        }
        Ok(())
    })
    .await
}

fn nonblank_owned(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use freshrss_feed::opml::{write, Opml};

    use crate::db::dao::{category as cat_dao, feed as feed_dao, user as user_dao};
    use crate::db::support::{db, scalar};
    use crate::domain::feed::Feed as DomainFeed;
    use crate::service::export;

    use super::*;

    async fn user(db: &Db, name: &str) -> i64 {
        let name = name.to_owned();
        db.with(move |mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: &name,
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

    /// A document with one folder holding one configured feed, plus a top-level orphan.
    fn sample() -> Opml {
        let mut scraper = Outline::feed("Scraper", "https://s.example/page");
        scraper.kind = freshrss_feed::FeedKind::HtmlXpath;
        scraper.priority = Some(freshrss_feed::opml::OpmlPriority::Important);
        scraper.ttl = Some(-3600);
        scraper.xpath = [("item".to_owned(), "//article".to_owned())].into();
        scraper.path_entries = Some("article.main".to_owned());
        Opml {
            title: "Sample".to_owned(),
            outlines: vec![
                Outline::folder("News", vec![scraper]),
                Outline::feed("Orphan", "https://o.example/feed.xml"),
            ],
            ..Opml::default()
        }
    }

    async fn feed_count(db: &Db) -> i64 {
        scalar(db, "SELECT COUNT(*) FROM feed").await
    }

    async fn category_count(db: &Db) -> i64 {
        scalar(db, "SELECT COUNT(*) FROM category").await
    }

    #[tokio::test]
    async fn import_creates_feeds_and_the_folders_they_name() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        let body = write(&sample());

        let report = opml(&store, id_user, body.as_bytes(), false, None)
            .await
            .expect("imported");
        assert_eq!(report.created_feeds, 2);
        assert_eq!(report.skipped_feeds, 0);
        assert_eq!(report.created_categories, 1, "the News folder");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(feed_count(&store).await, 2);

        // The folder was created and the feed filed under it.
        let news =
            store
                .with(move |mut conn| async move {
                    cat_dao::find_by_name(&mut conn, id_user, "News").await
                })
                .await
                .expect("query")
                .expect("News folder exists");
        let in_news = store
            .with(move |mut conn| async move {
                feed_dao::list_by_category(&mut conn, id_user, news.id).await
            })
            .await
            .expect("query");
        assert_eq!(in_news.len(), 1);
        assert_eq!(in_news[0].name, "Scraper");
    }

    #[tokio::test]
    async fn the_outlines_settings_land_on_the_feed() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        let body = write(&sample());
        opml(&store, id_user, body.as_bytes(), false, None)
            .await
            .expect("imported");

        let feed = store
            .with(move |mut conn| async move {
                feed_dao::find_by_url(&mut conn, id_user, "https://s.example/page").await
            })
            .await
            .expect("query")
            .expect("scraper imported");
        assert_eq!(feed.kind, FeedKind::HtmlXpath);
        assert_eq!(feed.priority, Priority::IMPORTANT);
        assert!(feed.ttl.is_muted(), "{:?}", feed.ttl);
        assert_eq!(feed.ttl.seconds(), 3600);
        assert_eq!(
            feed.xpath()
                .and_then(|m| m.get("item"))
                .and_then(|v| v.as_str()),
            Some("//article")
        );
        assert_eq!(feed.path_entries.as_deref(), Some("article.main"));
    }

    #[tokio::test]
    async fn a_dry_run_writes_nothing_but_reports_everything() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        let body = write(&sample());

        let report = opml(&store, id_user, body.as_bytes(), true, None)
            .await
            .expect("dry run");
        assert_eq!(report.created_feeds, 2, "what would be added");
        assert_eq!(report.created_categories, 1, "what would be filed");
        assert_eq!(feed_count(&store).await, 0, "nothing was written");
        assert_eq!(category_count(&store).await, 1, "only the seeded folder");
    }

    #[tokio::test]
    async fn duplicates_are_skipped_and_reported() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        // Pre-subscribe to one of the two addresses the document names.
        subscription::create(
            &store,
            &NewSubscription::new(id_user, "https://s.example/page"),
        )
        .await
        .expect("seeded");
        let body = write(&sample());

        let report = opml(&store, id_user, body.as_bytes(), false, None)
            .await
            .expect("imported");
        assert_eq!(report.skipped_feeds, 1, "the address already subscribed");
        assert_eq!(report.created_feeds, 1, "only the orphan is new");
        assert_eq!(feed_count(&store).await, 2);
    }

    #[tokio::test]
    async fn a_re_import_skips_everything() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        let body = write(&sample());
        opml(&store, id_user, body.as_bytes(), false, None)
            .await
            .expect("first");
        let again = opml(&store, id_user, body.as_bytes(), false, None)
            .await
            .expect("second");
        assert_eq!(again.created_feeds, 0);
        assert_eq!(again.skipped_feeds, 2);
        assert_eq!(again.created_categories, 0, "the folders already exist");
        assert_eq!(feed_count(&store).await, 2);
    }

    #[tokio::test]
    async fn force_category_overrides_the_documents_nesting() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        let target = store
            .with(move |mut conn| async move {
                let position = cat_dao::next_position(&mut conn, id_user).await?;
                cat_dao::insert(
                    &mut conn,
                    &cat_dao::NewCategory {
                        id_user,
                        name: "Inbox",
                        kind: Default::default(),
                        position,
                        attributes: Attributes::new(),
                    },
                )
                .await
            })
            .await
            .expect("folder");
        let body = write(&sample());

        let report = opml(&store, id_user, body.as_bytes(), false, Some(target))
            .await
            .expect("imported");
        assert_eq!(report.created_feeds, 2);
        assert_eq!(
            report.created_categories, 0,
            "no folder was created from the document"
        );
        let in_target = store
            .with(move |mut conn| async move {
                feed_dao::list_by_category(&mut conn, id_user, target).await
            })
            .await
            .expect("query");
        assert_eq!(
            in_target.len(),
            2,
            "both feeds filed under the forced folder"
        );
    }

    #[tokio::test]
    async fn a_forced_folder_the_user_does_not_own_is_refused() {
        let store = db().await;
        let alice = user(&store, "alice").await;
        let bob = user(&store, "bob").await;
        let bobs_folder = store
            .with(
                move |mut conn| async move { user_dao::default_category_id(&mut conn, bob).await },
            )
            .await
            .expect("query")
            .expect("bob has a default folder");
        let body = write(&sample());

        let err = opml(&store, alice, body.as_bytes(), false, Some(bobs_folder))
            .await
            .expect_err("must be refused");
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        assert_eq!(feed_count(&store).await, 0);
    }

    #[tokio::test]
    async fn malformed_opml_is_a_validation_error_not_a_panic() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        for bad in [
            "<rss><channel/></rss>",
            "not xml at all",
            "",
            // Truncated inside a tag: the reader cannot know which attributes were meant to follow.
            "<opml><body><outline text=\"a\" xmlUrl=\"https://a/\"",
        ] {
            let result = opml(&store, id_user, bad.as_bytes(), false, None).await;
            assert!(
                matches!(result, Err(Error::Validation(_))),
                "{bad:?} must be a validation error, got {result:?}"
            );
        }
        // Non-UTF-8 bytes are refused rather than decoded lossily.
        let result = opml(&store, id_user, &[0xff, 0xfe, 0xfd], false, None).await;
        assert!(matches!(result, Err(Error::Validation(_))), "{result:?}");
        assert_eq!(feed_count(&store).await, 0);
    }

    #[tokio::test]
    async fn a_rejected_url_is_recorded_and_the_rest_still_imports() {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        let doc = Opml {
            outlines: vec![
                Outline::feed("Bad", "file:///etc/passwd"),
                Outline::feed("Good", "https://g.example/feed.xml"),
            ],
            ..Opml::default()
        };
        let body = write(&doc);
        let report = opml(&store, id_user, body.as_bytes(), false, None)
            .await
            .expect("imported");
        assert_eq!(report.created_feeds, 1, "the http feed");
        assert_eq!(report.errors.len(), 1, "the file:// feed was refused");
        assert_eq!(feed_count(&store).await, 1);
    }

    #[tokio::test]
    async fn export_then_import_reproduces_the_tree() {
        let store = db().await;
        let source = user(&store, "source").await;
        let target = user(&store, "target").await;

        // Build a configured feed under a folder for the source user, straight through the DAO so the
        // attributes are exactly what the exporter is meant to carry.
        let folder = store
            .with(move |mut conn| async move {
                let position = cat_dao::next_position(&mut conn, source).await?;
                cat_dao::insert(
                    &mut conn,
                    &cat_dao::NewCategory {
                        id_user: source,
                        name: "Tech",
                        kind: Default::default(),
                        position,
                        attributes: Attributes::new(),
                    },
                )
                .await
            })
            .await
            .expect("folder");
        let mut feed = DomainFeed::new(source, "https://s.example/page", "Scraper");
        feed.category = Some(folder);
        feed.kind = FeedKind::XmlXpath;
        feed.priority = Priority::HIDDEN;
        feed.ttl = Ttl::new(900, false);
        feed.website = "https://s.example/".into();
        let mut xpath = serde_json::Map::new();
        xpath.insert("item".into(), "//row".into());
        xpath.insert("itemTitle".into(), ".//h2".into());
        feed.attributes.set(keys::XPATH, xpath);
        store
            .with(move |mut conn| async move { feed_dao::insert(&mut conn, &feed).await })
            .await
            .expect("feed");

        let exported = export::opml(&store, source).await.expect("export");
        let body = write(&exported);
        let report = opml(&store, target, body.as_bytes(), false, None)
            .await
            .expect("import");
        assert_eq!(report.created_feeds, 1);
        let reimported = export::opml(&store, target).await.expect("re-export");

        let before = feed_outline_by_name(&exported, "Scraper");
        let after = feed_outline_by_name(&reimported, "Scraper");
        assert_eq!(before.kind, after.kind, "kind survives the round trip");
        assert_eq!(before.priority, after.priority, "priority survives");
        assert_eq!(before.ttl, after.ttl, "signed ttl survives");
        assert_eq!(before.html_url, after.html_url);
        assert_eq!(before.xpath, after.xpath, "the selector map survives");
        assert_eq!(before.xpath["item"], "//row");
        assert_eq!(before.xpath["itemTitle"], ".//h2");
    }

    fn feed_outline_by_name(doc: &Opml, name: &str) -> Outline {
        fn walk(outlines: &[Outline], name: &str) -> Option<Outline> {
            for outline in outlines {
                if outline.is_feed() && outline.name() == name {
                    return Some(outline.clone());
                }
                if let Some(found) = walk(&outline.children, name) {
                    return Some(found);
                }
            }
            None
        }
        walk(&doc.outlines, name).unwrap_or_else(|| panic!("no feed named {name:?}"))
    }
}
