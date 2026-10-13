//! Starter packs: small curated subscription lists a new account can install in one click.
//!
//! A pack is a compile-time constant rather than a row in a table or a file on disk, because the set
//! is short, changes with the product rather than per user, and shipping it in the binary means there
//! is nothing to deploy or migrate alongside it. Installing one builds an in-memory OPML document and
//! runs it through the ordinary [`crate::service::import`] path, so a pack behaves exactly like a
//! user-supplied subscription list: the same duplicate skipping, the same folder creation, the same
//! report.

use freshrss_feed::opml::{write, Opml, Outline};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::service::import::{self, ImportReport};

/// One feed in a pack: the address to subscribe to and the name to file it under.
pub struct PackFeed {
    pub url: &'static str,
    pub title: &'static str,
}

/// A curated set of feeds offered as a single install.
pub struct Pack {
    /// The stable identifier used in the install path.
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub feeds: &'static [PackFeed],
}

/// The registry. Every entry uses addresses that are published, stable and well known, so a pack
/// installs into something a reader will recognize rather than a set of dead links.
pub const PACKS: &[Pack] = &[
    Pack {
        slug: "tech-news",
        name: "Tech News",
        description: "General technology coverage, from daily aggregators to long-form.",
        feeds: &[
            PackFeed {
                url: "https://feeds.arstechnica.com/arstechnica/index",
                title: "Ars Technica",
            },
            PackFeed {
                url: "https://hnrss.org/frontpage",
                title: "Hacker News",
            },
            PackFeed {
                url: "https://www.theverge.com/rss/index.xml",
                title: "The Verge",
            },
            PackFeed {
                url: "https://techcrunch.com/feed/",
                title: "TechCrunch",
            },
        ],
    },
    Pack {
        slug: "rust",
        name: "Rust",
        description:
            "The Rust language: official announcements, the weekly digest and the community.",
        feeds: &[
            PackFeed {
                url: "https://blog.rust-lang.org/feeds/all.atom.xml",
                title: "The Rust Blog",
            },
            PackFeed {
                url: "https://this-week-in-rust.org/rss.xml",
                title: "This Week in Rust",
            },
            PackFeed {
                url: "https://www.reddit.com/r/rust/.rss",
                title: "r/rust",
            },
        ],
    },
    Pack {
        slug: "science",
        name: "Science",
        description: "Space, nature and general science news from established sources.",
        feeds: &[
            PackFeed {
                url: "https://www.nasa.gov/rss/dyn/breaking_news.rss",
                title: "NASA Breaking News",
            },
            PackFeed {
                url: "https://www.nature.com/nature.rss",
                title: "Nature",
            },
            PackFeed {
                url: "https://www.sciencedaily.com/rss/all.xml",
                title: "ScienceDaily",
            },
            PackFeed {
                url: "https://phys.org/rss-feed/",
                title: "Phys.org",
            },
        ],
    },
    Pack {
        slug: "newspapers",
        name: "Newspapers",
        description: "World news from major broadsheet outlets.",
        feeds: &[
            PackFeed {
                url: "https://feeds.bbci.co.uk/news/world/rss.xml",
                title: "BBC News — World",
            },
            PackFeed {
                url: "https://rss.nytimes.com/services/xml/rss/nyt/World.xml",
                title: "The New York Times — World",
            },
            PackFeed {
                url: "https://www.theguardian.com/world/rss",
                title: "The Guardian — World",
            },
            PackFeed {
                url: "https://feeds.npr.org/1001/rss.xml",
                title: "NPR News",
            },
        ],
    },
];

/// Looks a pack up by slug.
pub fn get(slug: &str) -> Option<&'static Pack> {
    PACKS.iter().find(|pack| pack.slug == slug)
}

/// Builds the OPML document for a pack, filing its feeds under a folder named after the pack so an
/// install lands in one place rather than scattered across the default folder.
pub fn to_opml(pack: &Pack) -> Opml {
    let feeds = pack
        .feeds
        .iter()
        .map(|feed| Outline::feed(feed.title, feed.url))
        .collect();
    Opml {
        title: pack.name.to_owned(),
        outlines: vec![Outline::folder(pack.name, feeds)],
        ..Opml::default()
    }
}

/// Installs a pack into one user's subscriptions.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown slug (the handler turns this into a 404), plus anything the
/// import path can fail with. Feeds the user already has are skipped and counted, so installing a pack
/// twice adds nothing the second time.
pub async fn install(db: &Db, id_user: i64, slug: &str, dry_run: bool) -> Result<ImportReport> {
    let pack = get(slug).ok_or_else(|| Error::NotFound(format!("starter pack {slug:?}")))?;
    let body = write(&to_opml(pack));
    import::opml(db, id_user, body.as_bytes(), dry_run, None).await
}

#[cfg(test)]
mod tests {
    use crate::db::dao::{category as cat_dao, feed as feed_dao, user as user_dao};
    use crate::db::support::{db, scalar};
    use crate::db::Db;

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

    #[test]
    fn the_registry_is_well_formed() {
        assert!(PACKS.len() >= 3, "a few packs to choose from");
        for pack in PACKS {
            assert!(!pack.slug.is_empty());
            assert!(!pack.name.is_empty());
            assert!(!pack.description.is_empty());
            assert!(
                (3..=6).contains(&pack.feeds.len()),
                "{} has {} feeds",
                pack.slug,
                pack.feeds.len()
            );
            for feed in pack.feeds {
                assert!(
                    feed.url.starts_with("http://") || feed.url.starts_with("https://"),
                    "{} is not an absolute address",
                    feed.url
                );
                assert!(!feed.title.is_empty());
            }
        }
        // Slugs are unique, or `get` would be ambiguous.
        let mut slugs: Vec<&str> = PACKS.iter().map(|p| p.slug).collect();
        let before = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), before, "duplicate slug");
    }

    #[test]
    fn get_finds_a_known_pack_and_misses_an_unknown_one() {
        assert_eq!(get("rust").map(|p| p.name), Some("Rust"));
        assert!(get("no-such-pack").is_none());
    }

    #[test]
    fn to_opml_files_the_feeds_under_a_named_folder() {
        let pack = get("tech-news").expect("pack");
        let doc = to_opml(pack);
        assert_eq!(doc.outlines.len(), 1);
        let folder = &doc.outlines[0];
        assert_eq!(folder.name(), pack.name);
        assert_eq!(folder.children.len(), pack.feeds.len());
        assert!(folder.children.iter().all(Outline::is_feed));
        // It must serialize and parse back, since install runs it through the importer.
        let text = write(&doc);
        assert!(freshrss_feed::opml::parse(&text).is_ok());
    }

    #[tokio::test]
    async fn install_creates_the_packs_feeds_in_one_folder() {
        let store = db().await;
        let id_user = user(&store).await;

        let pack = get("science").expect("pack");
        let report = install(&store, id_user, "science", false)
            .await
            .expect("installed");
        assert_eq!(report.created_feeds, pack.feeds.len());
        assert_eq!(report.created_categories, 1, "the pack's folder");
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let feeds: i64 = scalar(&store, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, pack.feeds.len() as i64);
        let folder = store
            .with(move |mut conn| async move {
                cat_dao::find_by_name(&mut conn, id_user, pack.name).await
            })
            .await
            .expect("query")
            .expect("folder created");
        let filed = store
            .with(move |mut conn| async move {
                feed_dao::list_by_category(&mut conn, id_user, folder.id).await
            })
            .await
            .expect("query");
        assert_eq!(filed.len(), pack.feeds.len());
    }

    #[tokio::test]
    async fn installing_twice_skips_the_second_time() {
        let store = db().await;
        let id_user = user(&store).await;
        install(&store, id_user, "rust", false)
            .await
            .expect("first");
        let again = install(&store, id_user, "rust", false)
            .await
            .expect("second");
        assert_eq!(again.created_feeds, 0);
        assert_eq!(again.skipped_feeds, get("rust").expect("pack").feeds.len());
        let feeds: i64 = scalar(&store, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, get("rust").expect("pack").feeds.len() as i64);
    }

    #[tokio::test]
    async fn a_dry_install_writes_nothing() {
        let store = db().await;
        let id_user = user(&store).await;
        let report = install(&store, id_user, "newspapers", true)
            .await
            .expect("dry run");
        assert_eq!(
            report.created_feeds,
            get("newspapers").expect("pack").feeds.len()
        );
        let feeds: i64 = scalar(&store, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, 0);
    }

    #[tokio::test]
    async fn an_unknown_slug_is_not_found() {
        let store = db().await;
        let id_user = user(&store).await;
        let err = install(&store, id_user, "does-not-exist", false)
            .await
            .expect_err("unknown slug");
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }
}
