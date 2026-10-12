//! Building the sidebar tree and its unread counts.
//!
//! Everything the reader's `nav#rd-nav` shows comes from one [`TreeDto`], assembled in a fixed
//! number of queries regardless of how many feeds the user has. Building it from per-feed counts
//! would be an N+1 — and that N+1 is precisely why FreshRSS keeps `cache_nbEntries` and
//! `cache_nbUnreads` columns on `feed` in the first place.

use sqlx::SqliteConnection;

use crate::api::dto::{
    CategoryDto, CountsDto, LabelCountDto, ProblemCountDto, ScopeCounts, TreeDto,
};
use crate::db::dao::{category as cat_dao, entry as entry_dao, feed as feed_dao, tag as tag_dao};
use crate::domain::feed::Feed;
use crate::domain::priority::Priority;
use crate::error::Result;

/// Assembles the whole sidebar for one user.
pub async fn build(conn: &mut SqliteConnection, id_user: i64) -> Result<TreeDto> {
    let categories = cat_dao::list(conn, id_user).await?;
    let feeds = feed_dao::list(conn, id_user).await?;

    // Bucket feeds by folder. A feed whose folder was deleted has `category = None` and becomes an
    // orphan; it must still be listed, or removing a folder would hide subscriptions without
    // actually unsubscribing.
    let mut by_category: std::collections::HashMap<i64, Vec<Feed>> =
        std::collections::HashMap::new();
    let mut orphans = Vec::new();
    for feed in feeds {
        match feed.category {
            Some(cat) => {
                by_category.entry(cat).or_default().push(feed);
            }
            None => orphans.push(feed),
        }
    }

    let mut category_dtos = Vec::with_capacity(categories.len());
    for category in &categories {
        let feeds = by_category.remove(&category.id).unwrap_or_default();
        category_dtos.push(CategoryDto::new(category, feeds));
    }
    // Feeds pointing at a folder that no longer exists for this user. Cannot normally happen
    // (`category.id_user` matches), but a hand-edited database should not lose them silently.
    for (_, feeds) in by_category {
        orphans.extend(feeds);
    }
    let orphan_dtos: Vec<_> = orphans.iter().map(crate::api::dto::FeedDto::new).collect();

    let counts = counts_from(conn, id_user, &category_dtos, &orphan_dtos).await?;
    let problems = ProblemCountDto {
        count: feed_dao::list_with_problems(conn, id_user).await?.len() as i64,
    };

    Ok(TreeDto {
        categories: category_dtos,
        orphans: orphan_dtos,
        counts,
        problems,
    })
}

/// The sidebar's headline numbers, computed from already-built DTOs.
///
/// Shared by [`build`] and the standalone `GET /counts` endpoint so the two cannot report
/// different numbers for the same database state — a divergence there shows up as a badge that
/// disagrees with the list, which is the most confusing bug a reader can have.
async fn counts_from(
    conn: &mut SqliteConnection,
    id_user: i64,
    categories: &[CategoryDto],
    orphans: &[crate::api::dto::FeedDto],
) -> Result<CountsDto> {
    // `all` and `starred` come from one aggregate over `entry`, because they are exact and the
    // query is a single index-backed pass. The priority-filtered scopes come from the feeds'
    // cached counters, which are recomputed after every refresh and every entry mutation, so
    // summing them here is O(feeds) rather than O(entries).
    let states = entry_dao::count_states(conn, id_user).await?;

    let mut main_stream = ScopeCounts::default();
    let mut important = ScopeCounts::default();
    let mut accumulate = |priority: i8, unread: i64, total: i64| {
        if priority >= Priority::MAIN_STREAM.get() {
            main_stream.unread += unread;
            main_stream.total += total;
        }
        if priority >= Priority::IMPORTANT.get() {
            important.unread += unread;
            important.total += total;
        }
    };
    for category in categories {
        for feed in &category.feeds {
            accumulate(feed.priority, feed.unread, feed.total);
        }
    }
    for feed in orphans {
        accumulate(feed.priority, feed.unread, feed.total);
    }

    let labels = tag_dao::list_with_counts(conn, id_user)
        .await?
        .into_iter()
        .map(|(tag, total, unread)| LabelCountDto {
            id: tag.id,
            name: tag.name,
            unread,
            total,
        })
        .collect();

    Ok(CountsDto {
        all: ScopeCounts {
            unread: states.unread,
            total: states.total,
        },
        main_stream,
        important,
        starred: ScopeCounts {
            unread: 0,
            total: states.starred,
        },
        labels,
    })
}

/// The counts on their own, for `GET /api/v1/counts`.
pub async fn counts(conn: &mut SqliteConnection, id_user: i64) -> Result<CountsDto> {
    let categories = cat_dao::list(conn, id_user).await?;
    let feeds = feed_dao::list(conn, id_user).await?;

    let mut by_category: std::collections::HashMap<i64, Vec<Feed>> =
        std::collections::HashMap::new();
    let mut orphans = Vec::new();
    for feed in feeds {
        match feed.category {
            Some(cat) => {
                by_category.entry(cat).or_default().push(feed);
            }
            None => orphans.push(feed),
        }
    }
    let category_dtos: Vec<_> = categories
        .iter()
        .map(|c| CategoryDto::new(c, by_category.remove(&c.id).unwrap_or_default()))
        .collect();
    for (_, feeds) in by_category {
        orphans.extend(feeds);
    }
    let orphan_dtos: Vec<_> = orphans.iter().map(crate::api::dto::FeedDto::new).collect();

    counts_from(conn, id_user, &category_dtos, &orphan_dtos).await
}

/// Recomputes the cached counters for every feed touched by a mutation.
///
/// Called after entry inserts, reads and stars. Recomputing from `entry` for the affected feeds is
/// one query and cannot drift, which is preferable to incrementally adjusting counters in every
/// write path — an adjustment that misses a path silently poisons the sidebar until the next
/// refresh.
pub async fn refresh_counters(conn: &mut SqliteConnection, feed_ids: &[i64]) -> Result<()> {
    if feed_ids.is_empty() {
        return Ok(());
    }
    feed_dao::update_cached_values(conn, feed_ids).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dao::user as user_dao;
    use crate::db::support::{db, exec, scalar};
    use crate::db::Db;
    use crate::domain::attributes::Attributes;
    use crate::domain::entry::Entry;
    use crate::domain::ids::EntryId;

    async fn user(db: &Db, name: &str) -> i64 {
        let name = name.to_owned();
        db.with(|mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: &name,
                    email: None,
                    password_hash: "x",
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
        db.with(|mut conn| async move {
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

    async fn feed(
        db: &Db,
        id_user: i64,
        url: &str,
        category: Option<i64>,
        priority: Priority,
    ) -> i64 {
        let url = url.to_owned();
        db.with(|mut conn| async move {
            feed_dao::insert(
                &mut conn,
                &Feed {
                    category,
                    priority,
                    ..Feed::new(id_user, &url, "Example")
                },
            )
            .await
        })
        .await
        .expect("feed")
    }

    async fn add_entries(db: &Db, id_user: i64, id_feed: i64, n: usize, read: usize) {
        db.with_writer(move |mut conn| async move {
            for i in 0..n {
                let mut entry = Entry::new(id_user, id_feed, format!("g{i}"));
                entry.id = EntryId::now();
                entry.title = format!("Title {i}");
                entry.link = format!("https://example.com/{i}");
                entry.date = 1_700_000_000 + i as i64;
                entry.last_seen = entry.date;
                entry.is_read = i < read;
                entry_dao::stage(&mut conn, &entry).await?;
            }
            entry_dao::commit_staged(&mut conn, id_user).await?;
            Ok(())
        })
        .await
        .expect("stage");
        db.with(move |mut conn| async move {
            feed_dao::update_cached_values(&mut conn, &[id_feed]).await
        })
        .await
        .expect("counters");
    }

    async fn tree(db: &Db, id_user: i64) -> TreeDto {
        db.with(move |mut conn| async move { build(&mut conn, id_user).await })
            .await
            .expect("tree")
    }

    #[tokio::test]
    async fn an_empty_account_still_produces_a_usable_tree() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let tree = tree(&store, id).await;

        assert_eq!(tree.categories.len(), 1, "the seeded Uncategorized folder");
        assert!(tree.categories[0].feeds.is_empty());
        assert!(tree.orphans.is_empty());
        assert_eq!(
            tree.counts.all,
            ScopeCounts {
                unread: 0,
                total: 0
            }
        );
        assert_eq!(tree.counts.starred.total, 0);
        assert_eq!(tree.problems.count, 0);
    }

    #[tokio::test]
    async fn feeds_are_grouped_under_their_folder_and_counts_roll_up() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let news = folder(&store, id, "News").await;
        let f1 = feed(
            &store,
            id,
            "https://a.example/f",
            Some(news),
            Priority::MAIN_STREAM,
        )
        .await;
        let f2 = feed(
            &store,
            id,
            "https://b.example/f",
            Some(news),
            Priority::MAIN_STREAM,
        )
        .await;
        add_entries(&store, id, f1, 10, 4).await;
        add_entries(&store, id, f2, 5, 5).await;

        let tree = tree(&store, id).await;
        let news_dto = tree
            .categories
            .iter()
            .find(|c| c.name == "News")
            .expect("News folder");

        assert_eq!(news_dto.feeds.len(), 2);
        assert_eq!(news_dto.total, 15);
        assert_eq!(
            news_dto.unread, 6,
            "4 unread in the first feed, 0 in the second"
        );
        assert_eq!(tree.counts.all.total, 15);
        assert_eq!(tree.counts.all.unread, 6);
        assert_eq!(tree.counts.main_stream.total, 15);
    }

    #[tokio::test]
    async fn priority_filters_which_feeds_count_toward_the_main_stream() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let main = feed(
            &store,
            id,
            "https://main.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        let important = feed(
            &store,
            id,
            "https://imp.example/f",
            None,
            Priority::IMPORTANT,
        )
        .await;
        let hidden = feed(&store, id, "https://hid.example/f", None, Priority::HIDDEN).await;
        add_entries(&store, id, main, 3, 0).await;
        add_entries(&store, id, important, 2, 0).await;
        add_entries(&store, id, hidden, 7, 0).await;

        let tree = tree(&store, id).await;

        assert_eq!(
            tree.counts.all.total, 12,
            "`all` counts everything, hidden included"
        );
        assert_eq!(
            tree.counts.main_stream.total, 5,
            "main stream excludes hidden"
        );
        assert_eq!(tree.counts.important.total, 2);
        assert_eq!(tree.counts.main_stream.unread, 5);
    }

    #[tokio::test]
    async fn orphaned_feeds_are_listed_rather_than_dropped() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let news = folder(&store, id, "News").await;
        let f = feed(
            &store,
            id,
            "https://a.example/f",
            Some(news),
            Priority::MAIN_STREAM,
        )
        .await;
        add_entries(&store, id, f, 4, 1).await;

        // Deleting the folder leaves the feed with category = NULL (ON DELETE SET NULL).
        store
            .with(move |mut conn| async move { cat_dao::delete(&mut conn, news, id).await })
            .await
            .expect("delete folder");

        let tree = tree(&store, id).await;
        assert_eq!(
            tree.orphans.len(),
            1,
            "the subscription must stay reachable"
        );
        assert_eq!(tree.orphans[0].total, 4);
        assert_eq!(
            tree.counts.main_stream.total, 4,
            "orphans still count toward the sidebar totals"
        );
    }

    #[tokio::test]
    async fn starred_counts_come_from_the_entries_not_the_cache() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let f = feed(
            &store,
            id,
            "https://a.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        add_entries(&store, id, f, 5, 0).await;

        let ids: Vec<EntryId> = store
            .with(move |mut conn| async move {
                let rows: Vec<i64> =
                    sqlx::query_scalar("SELECT id FROM entry WHERE id_user = ?1 LIMIT 2")
                        .bind(id)
                        .fetch_all(&mut *conn)
                        .await?;
                Ok(rows.into_iter().map(EntryId::from_micros).collect())
            })
            .await
            .expect("ids");
        store
            .with(move |mut conn| async move {
                entry_dao::mark_favorite(&mut conn, id, &ids, true).await
            })
            .await
            .expect("star");

        let tree = tree(&store, id).await;
        assert_eq!(tree.counts.starred.total, 2);
        assert_eq!(
            tree.counts.all.total, 5,
            "starring does not change the total"
        );
    }

    #[tokio::test]
    async fn labels_appear_with_their_own_counts() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let f = feed(
            &store,
            id,
            "https://a.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        add_entries(&store, id, f, 3, 1).await;

        store
            .with(move |mut conn| async move {
                let todo = tag_dao::find_or_create(&mut conn, id, "todo").await?;
                let ids: Vec<i64> =
                    sqlx::query_scalar("SELECT id FROM entry WHERE id_user = ?1 ORDER BY id")
                        .bind(id)
                        .fetch_all(&mut *conn)
                        .await?;
                tag_dao::tag_entries(&mut conn, todo, &ids[..2]).await
            })
            .await
            .expect("label");

        let tree = tree(&store, id).await;
        assert_eq!(tree.counts.labels.len(), 1);
        assert_eq!(tree.counts.labels[0].name, "todo");
        assert_eq!(tree.counts.labels[0].total, 2);
        assert_eq!(
            tree.counts.labels[0].unread, 1,
            "of the two labelled articles, the first was created read"
        );
    }

    #[tokio::test]
    async fn problem_feeds_are_counted_for_the_dialog_badge() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let failing = feed(
            &store,
            id,
            "https://a.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        feed(
            &store,
            id,
            "https://b.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        store
            .with(move |mut conn| async move {
                feed_dao::update_last_error(&mut conn, failing, 1_700_000_000).await
            })
            .await
            .expect("fail");

        let tree = tree(&store, id).await;
        assert_eq!(tree.problems.count, 1);
    }

    #[tokio::test]
    async fn the_tree_never_shows_another_users_rows() {
        let store = db().await;
        let alice = user(&store, "alice").await;
        let bob = user(&store, "bob").await;
        let bob_folder = folder(&store, bob, "Bob News").await;
        let bob_feed = feed(
            &store,
            bob,
            "https://bob.example/f",
            Some(bob_folder),
            Priority::MAIN_STREAM,
        )
        .await;
        add_entries(&store, bob, bob_feed, 9, 0).await;

        let tree = tree(&store, alice).await;
        assert_eq!(tree.categories.len(), 1, "only alice's seeded folder");
        assert!(tree.categories.iter().all(|c| c.feeds.is_empty()));
        assert!(tree.orphans.is_empty());
        assert_eq!(tree.counts.all.total, 0, "bob's articles are invisible");
    }

    #[tokio::test]
    async fn refresh_counters_recomputes_only_the_feeds_named() {
        let store = db().await;
        let id = user(&store, "alice").await;
        let f1 = feed(
            &store,
            id,
            "https://a.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        let f2 = feed(
            &store,
            id,
            "https://b.example/f",
            None,
            Priority::MAIN_STREAM,
        )
        .await;
        add_entries(&store, id, f1, 3, 0).await;

        // Stale the counters, then repair only f1.
        exec(
            &store,
            "UPDATE feed SET cache_nbEntries = 999, cache_nbUnreads = 999",
        )
        .await;

        store
            .with(move |mut conn| async move { refresh_counters(&mut conn, &[f1]).await })
            .await
            .expect("refresh");

        store
            .with(move |mut conn| async move {
                let a = feed_dao::find_by_id(&mut conn, f1, id).await?.expect("f1");
                let b = feed_dao::find_by_id(&mut conn, f2, id).await?.expect("f2");
                assert_eq!(a.cache_nb_entries, 3, "recomputed");
                assert_eq!(b.cache_nb_entries, 999, "left alone, as asked");
                Ok(())
            })
            .await
            .expect("assert");
    }

    #[tokio::test]
    async fn refresh_counters_with_no_feeds_is_a_noop() {
        let store = db().await;
        store
            .with(|mut conn| async move { refresh_counters(&mut conn, &[]).await })
            .await
            .expect("noop");
        // Nothing was written, so the schema is still the only thing in there.
        let feeds: i64 = scalar(&store, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, 0);
    }
}
