//! DAO integration tests against the real schema.
//!
//! The unit tests in each DAO module cover row mapping and SQL fragments; these cover the
//! invariants that only show up once the tables, foreign keys and `id_user` scoping are all live.
//! The multi-user isolation cases are the important ones: FreshRSS never had to enforce isolation
//! because each user got their own database file, so nothing upstream will warn us when a query
//! forgets its `id_user` predicate.

use freshrss_server::db::dao::{
    category as cat_dao, entry as entry_dao, feed as feed_dao, meta, session, tag as tag_dao,
    user as user_dao,
};
use freshrss_server::db::Db;
use freshrss_server::domain::attributes::Attributes;
use freshrss_server::domain::category::DEFAULT_CATEGORY_NAME;
use freshrss_server::domain::entry::Entry;
use freshrss_server::domain::feed::Feed;
use freshrss_server::domain::ids::EntryId;
use freshrss_server::domain::priority::Priority;
use freshrss_server::domain::tag::Tag;
use freshrss_server::domain::ttl::Ttl;
use freshrss_server::domain::user::{SortOrder, UserConfig};
use sqlx::Row as _;

async fn db() -> Db {
    Db::in_memory().await.expect("in-memory db")
}

async fn create_user(db: &Db, username: &str) -> i64 {
    db.with(|mut conn| async move {
        user_dao::insert(
            &mut conn,
            &user_dao::NewUser {
                username,
                email: Some(&format!("{username}@example.com")),
                password_hash: "argon2id$hash",
                is_admin: false,
            },
        )
        .await
    })
    .await
    .expect("create user")
}

async fn create_feed(db: &Db, id_user: i64, url: &str, category: Option<i64>) -> i64 {
    db.with(|mut conn| async move {
        let feed = Feed {
            category,
            ..Feed::new(id_user, url, "Example")
        };
        feed_dao::insert(&mut conn, &feed).await
    })
    .await
    .expect("create feed")
}

async fn stage_entry(db: &Db, id_user: i64, id_feed: i64, guid: &str, title: &str, date: i64) {
    db.with(|mut conn| async move {
        let mut entry = Entry::new(id_user, id_feed, guid);
        entry.id = EntryId::now();
        entry.title = title.to_string();
        entry.link = format!("https://example.com/{guid}");
        entry.content = format!("<p>{title}</p>");
        entry.date = date;
        entry.last_seen = date;
        entry.hash = Some([7u8; 16]);
        entry_dao::stage(&mut conn, &entry).await?;
        Ok(())
    })
    .await
    .expect("stage");
}

// --- users -----------------------------------------------------------------

#[tokio::test]
async fn registering_a_user_seeds_their_uncategorized_folder() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let (config, folders): (Option<UserConfig>, Vec<String>) = db
        .with(|mut conn| async move {
            let user = user_dao::find_by_id(&mut conn, id)
                .await?
                .expect("user exists");
            let folders = cat_dao::list(&mut conn, id)
                .await?
                .into_iter()
                .map(|c| c.name)
                .collect();
            Ok((Some(user.config), folders))
        })
        .await
        .expect("read");

    assert_eq!(folders, vec![DEFAULT_CATEGORY_NAME.to_string()]);
    assert_eq!(config.unwrap().ttl_default, 3600);
}

#[tokio::test]
async fn each_user_gets_their_own_uncategorized_folder() {
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;

    let ids: (i64, i64) = db
        .with(|mut conn| async move {
            Ok((
                user_dao::default_category_id(&mut conn, alice)
                    .await?
                    .expect("alice's folder"),
                user_dao::default_category_id(&mut conn, bob)
                    .await?
                    .expect("bob's folder"),
            ))
        })
        .await
        .expect("read");
    assert_ne!(ids.0, ids.1, "the folder must not be shared");
}

#[tokio::test]
async fn username_lookup_is_case_insensitive_but_preserves_the_stored_form() {
    let db = db().await;
    create_user(&db, "Alice").await;
    let found = db
        .with(|mut conn| async move { user_dao::find_by_username(&mut conn, "aLiCe").await })
        .await
        .expect("read")
        .expect("found");
    assert_eq!(found.username, "Alice", "the original casing is kept");
}

#[tokio::test]
async fn deleting_a_user_cascades_to_everything_they_own() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let folder = db
        .with(|mut conn| async move {
            cat_dao::insert(
                &mut conn,
                &cat_dao::NewCategory {
                    id_user: id,
                    name: "News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )
            .await
        })
        .await
        .expect("folder");
    let feed = create_feed(&db, id, "https://example.com/f", Some(folder)).await;
    stage_entry(&db, id, feed, "g1", "Title", 1_700_000_000).await;
    db.with(|mut conn| async move {
        entry_dao::commit_staged(&mut conn, id).await?;
        let tag = tag_dao::find_or_create(&mut conn, id, "todo").await?;
        let entry = entry_dao::find_by_guid(&mut conn, feed, "g1")
            .await?
            .expect("entry");
        tag_dao::tag_entry(&mut conn, tag, entry.id.micros(), true).await?;
        session::create(&mut conn, id, 3600, Some("127.0.0.1"), Some("test")).await?;
        Ok(())
    })
    .await
    .expect("seed");

    db.with(|mut conn| async move { user_dao::delete(&mut conn, id).await })
        .await
        .expect("delete");

    let leftovers = db
        .with(|mut conn| async move {
            let row = sqlx::query(
                "SELECT (SELECT COUNT(*) FROM category) AS category,
                        (SELECT COUNT(*) FROM feed) AS feed,
                        (SELECT COUNT(*) FROM entry) AS entry,
                        (SELECT COUNT(*) FROM tag) AS tag,
                        (SELECT COUNT(*) FROM entrytag) AS entrytag,
                        (SELECT COUNT(*) FROM session) AS session",
            )
            .fetch_one(&mut *conn)
            .await?;
            Ok((
                row.get::<i64, _>("category"),
                row.get::<i64, _>("feed"),
                row.get::<i64, _>("entry"),
                row.get::<i64, _>("tag"),
                row.get::<i64, _>("entrytag"),
                row.get::<i64, _>("session"),
            ))
        })
        .await
        .expect("count");
    assert_eq!(
        leftovers,
        (0, 0, 0, 0, 0, 0),
        "nothing may survive the user"
    );
}

#[tokio::test]
async fn a_disabled_user_is_inert_but_their_rows_survive() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    db.with(|mut conn| async move { user_dao::set_enabled(&mut conn, id, false).await })
        .await
        .expect("disable");
    let user = db
        .with(|mut conn| async move { user_dao::find_by_id(&mut conn, id).await })
        .await
        .expect("read")
        .expect("still present");
    assert!(!user.enabled);
}

// --- multi-user isolation --------------------------------------------------

/// The invariant FreshRSS never needed: one user must not be able to read or write another's rows.
#[tokio::test]
async fn no_dao_leaks_rows_across_users() {
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;

    // Alice owns a folder, a feed, an article and a label.
    let (alice_folder, alice_feed) = db
        .with(|mut conn| async move {
            let folder = cat_dao::insert(
                &mut conn,
                &cat_dao::NewCategory {
                    id_user: alice,
                    name: "Alice News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )
            .await?;
            let feed = feed_dao::insert(
                &mut conn,
                &Feed::new(alice, "https://alice.example/f", "Alice"),
            )
            .await?;
            Ok((folder, feed))
        })
        .await
        .expect("seed alice");
    stage_entry(&db, alice, alice_feed, "g1", "Alice article", 1_700_000_000).await;
    db.with(|mut conn| async move {
        entry_dao::commit_staged(&mut conn, alice).await?;
        tag_dao::find_or_create(&mut conn, alice, "alice-label").await?;
        Ok(())
    })
    .await
    .expect("commit alice");

    let bob_folder = db
        .with(|mut conn| async move {
            cat_dao::insert(
                &mut conn,
                &cat_dao::NewCategory {
                    id_user: bob,
                    name: "Bob News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )
            .await
        })
        .await
        .expect("seed bob");

    db.with(|mut conn| async move {
        // Reads by id must not resolve across users.
        assert!(
            cat_dao::find_by_id(&mut conn, alice_folder, bob)
                .await?
                .is_none(),
            "bob must not see alice's folder"
        );
        assert!(
            feed_dao::find_by_id(&mut conn, alice_feed, bob)
                .await?
                .is_none(),
            "bob must not see alice's feed"
        );
        assert!(
            tag_dao::find_by_name(&mut conn, bob, "alice-label")
                .await?
                .is_none(),
            "bob must not see alice's label"
        );

        // Listings must be scoped.
        assert_eq!(
            cat_dao::list(&mut conn, bob).await?.len(),
            2,
            "bob sees only his own two folders"
        );
        assert_eq!(feed_dao::list(&mut conn, bob).await?.len(), 0);
        assert_eq!(tag_dao::list(&mut conn, bob).await?.len(), 0);
        assert_eq!(
            entry_dao::count_states(&mut conn, bob).await?.total,
            0,
            "bob has no articles"
        );
        assert_eq!(entry_dao::count_states(&mut conn, alice).await?.total, 1);

        // Writes scoped by user must not touch another user's rows.
        let alice_entry = entry_dao::find_by_guid(&mut conn, alice_feed, "g1")
            .await?
            .expect("alice's entry");
        let marked = entry_dao::mark_read(&mut conn, bob, &[alice_entry.id], true).await?;
        assert_eq!(marked, 0, "bob cannot mark alice's article read");

        assert!(!feed_dao::update_name(&mut conn, alice_feed, bob, "hijacked").await?);
        assert!(!feed_dao::update_priority(&mut conn, alice_feed, bob, Priority::HIDDEN).await?);
        assert!(!feed_dao::delete(&mut conn, alice_feed, bob).await?);
        assert!(!cat_dao::delete(&mut conn, alice_folder, bob).await?);
        assert!(!cat_dao::update_name(&mut conn, alice_folder, bob, "hijacked").await?);

        // Everything alice owns is still intact.
        assert_eq!(feed_dao::list(&mut conn, alice).await?.len(), 1);
        let feed = feed_dao::find_by_id(&mut conn, alice_feed, alice)
            .await?
            .expect("feed survives");
        assert_eq!(feed.name, "Alice", "the rename must not have applied");
        assert_eq!(feed.priority, Priority::MAIN_STREAM);
        let entry = entry_dao::find_by_id(&mut conn, alice_entry.id, alice)
            .await?
            .expect("entry survives");
        assert!(!entry.is_read);

        assert!(bob_folder > 0);
        Ok(())
    })
    .await
    .expect("isolation checks");
}

#[tokio::test]
async fn the_same_feed_url_may_be_subscribed_by_two_users() {
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;
    let url = "https://shared.example/feed.xml";

    let (a, b) = db
        .with(|mut conn| async move {
            Ok((
                feed_dao::insert(&mut conn, &Feed::new(alice, url, "Shared")).await?,
                feed_dao::insert(&mut conn, &Feed::new(bob, url, "Shared")).await?,
            ))
        })
        .await
        .expect("insert both");
    assert_ne!(a, b);

    db.with(|mut conn| async move {
        assert!(feed_dao::find_by_url(&mut conn, alice, url)
            .await?
            .is_some());
        assert!(feed_dao::find_by_url(&mut conn, bob, url).await?.is_some());
        let carol = 999;
        assert!(
            feed_dao::find_by_url(&mut conn, carol, url)
                .await?
                .is_none(),
            "a user who has not subscribed sees nothing"
        );
        Ok(())
    })
    .await
    .expect("lookups");
}

#[tokio::test]
async fn the_same_label_name_is_independent_per_user() {
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;
    let (a, b) = db
        .with(|mut conn| async move {
            Ok((
                tag_dao::find_or_create(&mut conn, alice, "todo").await?,
                tag_dao::find_or_create(&mut conn, bob, "todo").await?,
            ))
        })
        .await
        .expect("create");
    assert_ne!(a, b, "each user gets their own row");

    // Calling twice for the same user returns the same row rather than inserting.
    let again = db
        .with(|mut conn| async move { tag_dao::find_or_create(&mut conn, alice, "todo").await })
        .await
        .expect("again");
    assert_eq!(again, a);
}

// --- entry ids and staging -------------------------------------------------

#[tokio::test]
async fn committing_staged_entries_allocates_dense_chronological_ids() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;

    // Staged out of publication order on purpose.
    stage_entry(&db, id, feed, "newest", "Newest", 1_700_000_300).await;
    stage_entry(&db, id, feed, "oldest", "Oldest", 1_700_000_100).await;
    stage_entry(&db, id, feed, "middle", "Middle", 1_700_000_200).await;

    let inserted = db
        .with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");
    assert_eq!(inserted, 3);

    let ordered: Vec<(String, i64)> = db
        .with(|mut conn| async move {
            let rows: Vec<(String, i64)> =
                sqlx::query_as("SELECT guid, id FROM entry WHERE id_user = ?1 ORDER BY id")
                    .bind(id)
                    .fetch_all(&mut *conn)
                    .await?;
            Ok(rows)
        })
        .await
        .expect("read");

    assert_eq!(
        ordered.iter().map(|(g, _)| g.as_str()).collect::<Vec<_>>(),
        vec!["oldest", "middle", "newest"],
        "ids must follow publication date, not insertion order"
    );
    let ids: Vec<i64> = ordered.iter().map(|(_, i)| *i).collect();
    assert_eq!(ids[1] - ids[0], 1);
    assert_eq!(ids[2] - ids[1], 1, "ids are dense");
}

#[tokio::test]
async fn a_second_commit_never_reuses_or_precedes_an_earlier_id() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;

    stage_entry(&db, id, feed, "first", "First", 1_700_000_100).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit 1");
    let high_water_1 = db
        .with(|mut conn| async move { meta::entry_id_high_water(&mut conn).await })
        .await
        .expect("hwm");

    stage_entry(&db, id, feed, "second", "Second", 1_700_000_200).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit 2");

    let ids: Vec<i64> = db
        .with(|mut conn| async move {
            let rows: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM entry WHERE id_user = ?1 ORDER BY id")
                    .bind(id)
                    .fetch_all(&mut *conn)
                    .await?;
            Ok(rows)
        })
        .await
        .expect("read");

    assert_eq!(ids.len(), 2);
    assert!(ids[1] > ids[0], "ids must be monotonic across commits");
    assert!(ids[0] <= high_water_1);
    assert!(
        ids[1] > high_water_1,
        "the second batch starts above the first"
    );
}

#[tokio::test]
async fn concurrent_commits_from_two_users_do_not_collide_or_drop_articles() {
    // This is the regression the global allocator exists to prevent. FreshRSS's
    // `rowid + (MAX(id) - COUNT(*))` renumbering is only safe one-user-per-database; with a shared
    // database two simultaneous refreshes would target overlapping id ranges, and because the
    // commit is INSERT OR IGNORE the loser's articles vanish without an error.
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;
    let alice_feed = create_feed(&db, alice, "https://alice.example/f", None).await;
    let bob_feed = create_feed(&db, bob, "https://bob.example/f", None).await;

    let per_user = 50;
    for i in 0..per_user {
        stage_entry(
            &db,
            alice,
            alice_feed,
            &format!("a{i}"),
            "A",
            1_700_000_000 + i,
        )
        .await;
        stage_entry(&db, bob, bob_feed, &format!("b{i}"), "B", 1_700_000_000 + i).await;
    }

    // Both users commit; the writer mutex serializes them, which is the point.
    let (alice_committed, bob_committed) = db
        .with_writer(|mut conn| async move {
            Ok((
                entry_dao::commit_staged(&mut conn, alice).await?,
                entry_dao::commit_staged(&mut conn, bob).await?,
            ))
        })
        .await
        .expect("commit both");

    assert_eq!(alice_committed, per_user as usize);
    assert_eq!(bob_committed, per_user as usize);

    let (alice_count, bob_count, distinct_ids) = db
        .with(|mut conn| async move {
            Ok((
                entry_dao::count_states(&mut conn, alice).await?.total,
                entry_dao::count_states(&mut conn, bob).await?.total,
                sqlx::query_scalar::<_, i64>("SELECT COUNT(DISTINCT id) FROM entry")
                    .fetch_one(&mut *conn)
                    .await?,
            ))
        })
        .await
        .expect("counts");

    assert_eq!(alice_count, per_user, "alice lost no articles");
    assert_eq!(bob_count, per_user, "bob lost no articles");
    assert_eq!(
        distinct_ids,
        per_user * 2,
        "every id must be unique across users"
    );
}

#[tokio::test]
async fn staging_the_same_guid_twice_inserts_one_article() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;

    stage_entry(&db, id, feed, "dup", "First", 1_700_000_100).await;
    stage_entry(&db, id, feed, "dup", "Second", 1_700_000_200).await;

    let inserted = db
        .with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");
    assert_eq!(inserted, 1, "the duplicate is ignored, not doubled");
    assert_eq!(
        entry_count(&db, id).await,
        1,
        "exactly one article exists for that guid"
    );
}

#[tokio::test]
async fn committing_nothing_is_a_noop_that_does_not_burn_ids() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let before = db
        .with(|mut conn| async move { meta::entry_id_high_water(&mut conn).await })
        .await
        .expect("hwm");
    let inserted = db
        .with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");
    assert_eq!(inserted, 0);
    let after = db
        .with(|mut conn| async move { meta::entry_id_high_water(&mut conn).await })
        .await
        .expect("hwm");
    assert_eq!(
        before, after,
        "an empty refresh must not advance the sequence"
    );
}

#[tokio::test]
async fn discarding_staged_rows_removes_them() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_000).await;

    let discarded = db
        .with(|mut conn| async move { entry_dao::discard_staged(&mut conn, id).await })
        .await
        .expect("discard");
    assert_eq!(discarded, 1);

    let inserted = db
        .with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");
    assert_eq!(inserted, 0);
    assert_eq!(entry_count(&db, id).await, 0);
}

async fn entry_count(db: &Db, id_user: i64) -> i64 {
    db.with(|mut conn| async move { Ok(entry_dao::count_states(&mut conn, id_user).await?.total) })
        .await
        .expect("count")
}

// --- reconciliation --------------------------------------------------------

#[tokio::test]
async fn updating_an_article_keeps_its_id_and_the_users_state() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "Original", 1_700_000_100).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    // The reader marks it read and stars it.
    let original = db
        .with(|mut conn| async move { entry_dao::find_by_guid(&mut conn, feed, "g1").await })
        .await
        .expect("read")
        .expect("entry");
    db.with(|mut conn| async move {
        entry_dao::mark_read(&mut conn, id, &[original.id], true).await?;
        entry_dao::mark_favorite(&mut conn, id, &[original.id], true).await?;
        Ok(())
    })
    .await
    .expect("mark");

    // An upstream edit arrives.
    db.with_writer(|mut conn| async move {
        let mut updated = Entry::new(id, feed, "g1");
        updated.title = "Corrected".into();
        updated.content = "<p>new body</p>".into();
        updated.link = original.link.clone();
        updated.date = 1_700_000_999;
        updated.last_seen = 1_700_001_000;
        updated.last_modified = Some(1_700_001_000);
        updated.hash = Some([9u8; 16]);
        // is_read / is_favorite left at their defaults: `None`-style COALESCE keeps the DB value.
        assert!(entry_dao::update_existing(&mut conn, &updated).await?);
        Ok(())
    })
    .await
    .expect("update");

    let after = db
        .with(|mut conn| async move { entry_dao::find_by_guid(&mut conn, feed, "g1").await })
        .await
        .expect("read")
        .expect("entry");

    assert_eq!(after.id, original.id, "the id must not change on update");
    assert_eq!(after.title, "Corrected");
    assert_eq!(after.content, "<p>new body</p>");
    assert_eq!(after.hash, Some([9u8; 16]));
    assert_eq!(after.last_modified, Some(1_700_001_000));
    assert!(
        after.is_read,
        "an upstream edit must not clear the read flag"
    );
    assert!(after.is_favorite, "an upstream edit must not un-star");
    assert_eq!(
        entry_count(&db, id).await,
        1,
        "an update must not create a second article"
    );
}

#[tokio::test]
async fn list_hashes_for_guids_answers_seen_and_changed_in_one_query() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "One", 1_700_000_100).await;
    stage_entry(&db, id, feed, "g2", "Two", 1_700_000_200).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let hashes = db
        .with(|mut conn| async move {
            entry_dao::list_hashes_for_guids(
                &mut conn,
                feed,
                &["g1".to_string(), "g2".to_string(), "g3".to_string()],
            )
            .await
        })
        .await
        .expect("lookup");

    assert_eq!(hashes.len(), 2, "g3 was never seen");
    assert_eq!(hashes["g1"], [7u8; 16]);
    assert!(!hashes.contains_key("g3"));
}

#[tokio::test]
async fn last_seen_advances_for_guids_still_published_and_stops_for_gone_ones() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "kept", "Kept", 1_700_000_100).await;
    stage_entry(&db, id, feed, "gone", "Gone", 1_700_000_100).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let later = 1_700_009_999;
    let touched = db
        .with(|mut conn| async move {
            entry_dao::update_last_seen(&mut conn, feed, &["kept".to_string()], later).await
        })
        .await
        .expect("update last seen");
    assert_eq!(touched, 1);

    db.with(|mut conn| async move {
        let kept = entry_dao::find_by_guid(&mut conn, feed, "kept")
            .await?
            .expect("kept");
        let gone = entry_dao::find_by_guid(&mut conn, feed, "gone")
            .await?
            .expect("gone");
        assert_eq!(
            kept.last_seen, later,
            "still published, so it slides forward"
        );
        assert!(
            gone.last_seen < later,
            "an article that vanished keeps its old lastSeen; retention acts on that later"
        );
        Ok(())
    })
    .await
    .expect("assert");
}

// --- point mutations -------------------------------------------------------

#[tokio::test]
async fn marking_read_only_touches_rows_that_actually_change() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    for i in 0..3 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i).await;
    }
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let ids = all_ids(&db, id).await;

    // Each borrow needs its own copy: an `async move` block takes what it names.
    let batch = ids[..2].to_vec();
    let first = db
        .with(
            move |mut conn| async move { entry_dao::mark_read(&mut conn, id, &batch, true).await },
        )
        .await
        .expect("mark");
    assert_eq!(first, 2);

    let batch = ids[..2].to_vec();
    let again = db
        .with(
            move |mut conn| async move { entry_dao::mark_read(&mut conn, id, &batch, true).await },
        )
        .await
        .expect("mark again");
    assert_eq!(again, 0, "already-read articles are not rewritten");

    let unread = db
        .with(|mut conn| async move { entry_dao::count_states(&mut conn, id).await })
        .await
        .expect("counts");
    assert_eq!(unread.unread, 1);
    assert_eq!(unread.read(), 2);
}

#[tokio::test]
async fn marking_read_a_second_time_preserves_the_original_user_timestamp() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let ids = all_ids(&db, id).await;
    let all = ids.clone();
    db.with(move |mut conn| async move { entry_dao::mark_read(&mut conn, id, &all, true).await })
        .await
        .expect("mark");
    let first = last_user_modified(&db, ids[0]).await;
    assert!(first.is_some(), "a user action stamps the column");

    // The `is_read <> ?1` guard means a repeat call cannot churn the timestamp.
    let all = ids.clone();
    db.with(move |mut conn| async move { entry_dao::mark_read(&mut conn, id, &all, true).await })
        .await
        .expect("mark again");
    assert_eq!(last_user_modified(&db, ids[0]).await, first);
}

async fn last_user_modified(db: &Db, id: EntryId) -> Option<i64> {
    db.with(move |mut conn| async move {
        Ok(
            sqlx::query_scalar("SELECT lastUserModified FROM entry WHERE id = ?1")
                .bind(id.micros())
                .fetch_one(&mut *conn)
                .await
                .expect("row"),
        )
    })
    .await
    .expect("read")
}

async fn all_ids(db: &Db, id_user: i64) -> Vec<EntryId> {
    db.with(move |mut conn| async move {
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM entry WHERE id_user = ?1 ORDER BY id")
                .bind(id_user)
                .fetch_all(&mut *conn)
                .await?;
        Ok(ids.into_iter().map(EntryId::from_micros).collect())
    })
    .await
    .expect("ids")
}

#[tokio::test]
async fn mark_all_read_respects_the_id_max_snapshot_fence() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    for i in 0..4 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i).await;
    }
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let ids = all_ids(&db, id).await;
    // "Mark all as read" means everything the reader had loaded, not articles that arrived since.
    let fence = ids[1];
    let marked = db
        .with(|mut conn| async move {
            entry_dao::mark_read_all(&mut conn, id, fence, 1_700_099_999).await
        })
        .await
        .expect("mark all");
    assert_eq!(marked, 2, "only the two at or below the fence");

    let states = db
        .with(|mut conn| async move { entry_dao::count_states(&mut conn, id).await })
        .await
        .expect("counts");
    assert_eq!(states.unread, 2);
}

// --- cached counters -------------------------------------------------------

#[tokio::test]
async fn cached_counters_track_entries_and_unreads() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    for i in 0..3 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i).await;
    }
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    db.with(|mut conn| async move { feed_dao::update_cached_values(&mut conn, &[feed]).await })
        .await
        .expect("refresh counters");
    let f = db
        .with(|mut conn| async move { feed_dao::find_by_id(&mut conn, feed, id).await })
        .await
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_entries, 3);
    assert_eq!(f.cache_nb_unreads, 3);

    let ids = all_ids(&db, id).await;
    db.with(|mut conn| async move { entry_dao::mark_read(&mut conn, id, &ids[..1], true).await })
        .await
        .expect("mark");
    db.with(|mut conn| async move { feed_dao::bump_cache_unreads(&mut conn, feed, -1).await })
        .await
        .expect("bump");

    let f = db
        .with(|mut conn| async move { feed_dao::find_by_id(&mut conn, feed, id).await })
        .await
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_unreads, 2);
    assert_eq!(f.cache_nb_entries, 3, "total is unaffected by reading");
}

#[tokio::test]
async fn the_unread_counter_never_goes_negative() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    db.with(|mut conn| async move { feed_dao::bump_cache_unreads(&mut conn, feed, -5).await })
        .await
        .expect("bump");
    let f = db
        .with(|mut conn| async move { feed_dao::find_by_id(&mut conn, feed, id).await })
        .await
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_unreads, 0);
}

#[tokio::test]
async fn truncating_a_feed_empties_it_and_forces_a_full_refetch() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");
    db.with(|mut conn| async move {
        feed_dao::update_last_update(&mut conn, feed, 1_700_000_200).await?;
        feed_dao::update_cached_values(&mut conn, &[feed]).await?;
        Ok(())
    })
    .await
    .expect("counters");

    let deleted = db
        .with(|mut conn| async move { feed_dao::truncate_entries(&mut conn, feed, id).await })
        .await
        .expect("truncate");
    assert_eq!(deleted, 1);

    let f = db
        .with(|mut conn| async move { feed_dao::find_by_id(&mut conn, feed, id).await })
        .await
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_entries, 0);
    assert_eq!(f.cache_nb_unreads, 0);
    assert_eq!(f.last_update, 0, "the next refresh treats the feed as new");
    assert_eq!(entry_count(&db, id).await, 0);
}

// --- refresh scheduling ----------------------------------------------------

#[tokio::test]
async fn a_never_fetched_feed_is_due_immediately() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    create_feed(&db, id, "https://example.com/f", None).await;
    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, 1_700_000_000, 10).await })
        .await
        .expect("due");
    assert_eq!(due.len(), 1);
}

#[tokio::test]
async fn a_recently_refreshed_feed_is_not_due() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    db.with(|mut conn| async move {
        feed_dao::update_last_update(&mut conn, feed, 1_700_000_000).await
    })
    .await
    .expect("refresh");

    // ttl is 0, so the owner's ttl_default (3600) applies.
    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, 1_700_000_100, 10).await })
        .await
        .expect("due");
    assert!(due.is_empty(), "100s into a 3600s ttl is not due");

    let due = db
        .with(
            |mut conn| async move { feed_dao::list_due(&mut conn, 1_700_000_000 + 3600, 10).await },
        )
        .await
        .expect("due");
    assert_eq!(due.len(), 1, "due once the ttl has elapsed");
}

#[tokio::test]
async fn ttl_zero_uses_the_owners_default_not_a_global_one() {
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;

    // Bob's account is configured to poll every 60 seconds.
    db.with(|mut conn| async move {
        let cfg = UserConfig {
            ttl_default: 60,
            ..UserConfig::default()
        };
        user_dao::update_config(&mut conn, bob, &cfg).await
    })
    .await
    .expect("configure bob");

    let alice_feed = create_feed(&db, alice, "https://alice.example/f", None).await;
    let bob_feed = create_feed(&db, bob, "https://bob.example/f", None).await;
    let now = 1_700_000_000;
    db.with(|mut conn| async move {
        feed_dao::update_last_update(&mut conn, alice_feed, now).await?;
        feed_dao::update_last_update(&mut conn, bob_feed, now).await?;
        Ok(())
    })
    .await
    .expect("refresh both");

    // 120 seconds later: past bob's 60s default, well inside alice's 3600s.
    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, now + 120, 10).await })
        .await
        .expect("due");
    let ids: Vec<i64> = due.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![bob_feed], "only bob's feed is due");
    assert!(!ids.contains(&alice_feed));
}

#[tokio::test]
async fn muted_feeds_are_excluded_from_batch_refresh() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    db.with(|mut conn| async move {
        feed_dao::update_ttl(&mut conn, feed, id, Ttl::new(3600, true)).await
    })
    .await
    .expect("mute");

    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, 1_800_000_000, 10).await })
        .await
        .expect("due");
    assert!(
        due.is_empty(),
        "a negative ttl means muted, not 'very often'"
    );

    // Still reachable when named explicitly, so the user can force a refresh.
    let direct = db
        .with(|mut conn| async move { feed_dao::find_by_id(&mut conn, feed, id).await })
        .await
        .expect("read")
        .expect("feed");
    assert!(direct.is_muted());
}

#[tokio::test]
async fn a_failed_feed_retries_on_its_error_timestamp() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    let now = 1_700_000_000;
    db.with(|mut conn| async move {
        // A success long ago, then a recent failure: the failure must dominate.
        feed_dao::update_last_update(&mut conn, feed, now).await?;
        feed_dao::update_last_error(&mut conn, feed, now + 3000).await?;
        Ok(())
    })
    .await
    .expect("fail");

    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, now + 3100, 10).await })
        .await
        .expect("due");
    assert!(
        due.is_empty(),
        "only 100s past the error, inside the 3600s ttl"
    );

    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, now + 3000 + 3600, 10).await })
        .await
        .expect("due");
    assert_eq!(due.len(), 1);
}

#[tokio::test]
async fn due_feeds_are_ordered_oldest_first() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let a = create_feed(&db, id, "https://a.example/f", None).await;
    let b = create_feed(&db, id, "https://b.example/f", None).await;
    db.with(|mut conn| async move {
        feed_dao::update_last_update(&mut conn, b, 1_600_000_000).await?;
        feed_dao::update_last_update(&mut conn, a, 1_690_000_000).await?;
        Ok(())
    })
    .await
    .expect("refresh");

    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, 1_800_000_000, 10).await })
        .await
        .expect("due");
    let ids: Vec<i64> = due.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![b, a], "the stalest feed is refreshed first");
}

#[tokio::test]
async fn feeds_of_a_disabled_user_are_not_refreshed() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    create_feed(&db, id, "https://example.com/f", None).await;
    db.with(|mut conn| async move { user_dao::set_enabled(&mut conn, id, false).await })
        .await
        .expect("disable");

    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, 1_800_000_000, 10).await })
        .await
        .expect("due");
    assert!(due.is_empty());
}

#[tokio::test]
async fn the_due_list_honours_its_limit() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    for i in 0..5 {
        create_feed(&db, id, &format!("https://example.com/f{i}"), None).await;
    }
    let due = db
        .with(|mut conn| async move { feed_dao::list_due(&mut conn, 1_800_000_000, 2).await })
        .await
        .expect("due");
    assert_eq!(due.len(), 2);
}

#[tokio::test]
async fn problem_feeds_are_listed_for_the_reader_dialog() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let failing = create_feed(&db, id, "https://failing.example/f", None).await;
    let muted = create_feed(&db, id, "https://muted.example/f", None).await;
    create_feed(&db, id, "https://healthy.example/f", None).await;

    db.with(|mut conn| async move {
        feed_dao::update_last_error(&mut conn, failing, 1_700_000_000).await?;
        feed_dao::update_ttl(&mut conn, muted, id, Ttl::new(3600, true)).await?;
        Ok(())
    })
    .await
    .expect("seed");

    let problems = db
        .with(|mut conn| async move { feed_dao::list_with_problems(&mut conn, id).await })
        .await
        .expect("problems");
    assert_eq!(problems.len(), 2);
    assert!(problems.iter().any(|f| f.id == failing && f.has_error()));
    assert!(problems.iter().any(|f| f.id == muted && f.is_muted()));
}

// --- labels ----------------------------------------------------------------

#[tokio::test]
async fn labels_attach_detach_and_list_without_n_plus_one_queries() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    for i in 0..3 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i).await;
    }
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let ids = all_ids(&db, id).await;
    db.with(|mut conn| async move {
        let todo = tag_dao::find_or_create(&mut conn, id, "todo").await?;
        let read_later = tag_dao::find_or_create(&mut conn, id, "read-later").await?;
        tag_dao::tag_entries(&mut conn, todo, &[ids[0].micros(), ids[1].micros()]).await?;
        tag_dao::tag_entry(&mut conn, read_later, ids[1].micros(), true).await?;

        let on_first = tag_dao::labels_for_entry(&mut conn, ids[0].micros()).await?;
        assert_eq!(on_first.len(), 1);
        assert_eq!(on_first[0].name, "todo");

        // One query for the whole page.
        let all = tag_dao::labels_for_entries(
            &mut conn,
            &ids.iter().map(|i| i.micros()).collect::<Vec<_>>(),
        )
        .await?;
        assert_eq!(all[&ids[0].micros()].len(), 1);
        assert_eq!(all[&ids[1].micros()].len(), 2);
        assert!(
            !all.contains_key(&ids[2].micros()),
            "unlabelled entries are absent"
        );

        // Attaching twice is a no-op, not an error.
        tag_dao::tag_entry(&mut conn, todo, ids[0].micros(), true).await?;
        assert_eq!(
            tag_dao::labels_for_entry(&mut conn, ids[0].micros())
                .await?
                .len(),
            1
        );

        assert!(tag_dao::untag_entry(&mut conn, todo, ids[0].micros()).await?);
        assert!(tag_dao::labels_for_entry(&mut conn, ids[0].micros())
            .await?
            .is_empty());
        Ok(())
    })
    .await
    .expect("label ops");
}

#[tokio::test]
async fn label_counts_cover_total_and_unread() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100).await;
    stage_entry(&db, id, feed, "g2", "T", 1_700_000_200).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let ids = all_ids(&db, id).await;
    let counts = db
        .with(|mut conn| async move {
            let todo = tag_dao::find_or_create(&mut conn, id, "todo").await?;
            tag_dao::tag_entries(&mut conn, todo, &[ids[0].micros(), ids[1].micros()]).await?;
            entry_dao::mark_read(&mut conn, id, &[ids[0]], true).await?;
            let listed = tag_dao::list_with_counts(&mut conn, id).await?;
            Ok(listed
                .into_iter()
                .map(|(tag, total, unread)| (tag.name, total, unread))
                .collect::<Vec<_>>())
        })
        .await
        .expect("counts");

    assert_eq!(counts, vec![("todo".to_string(), 2, 1)]);
}

#[tokio::test]
async fn deleting_a_label_unlinks_articles_but_does_not_delete_them() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let feed = create_feed(&db, id, "https://example.com/f", None).await;
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100).await;
    db.with_writer(|mut conn| async move { entry_dao::commit_staged(&mut conn, id).await })
        .await
        .expect("commit");

    let ids = all_ids(&db, id).await;
    let tag_id = db
        .with(|mut conn| async move {
            let tag = tag_dao::find_or_create(&mut conn, id, "todo").await?;
            tag_dao::tag_entry(&mut conn, tag, ids[0].micros(), true).await?;
            Ok(tag)
        })
        .await
        .expect("label");

    db.with(|mut conn| async move { tag_dao::delete(&mut conn, tag_id, id).await })
        .await
        .expect("delete");

    assert_eq!(entry_count(&db, id).await, 1, "the article survives");
    let links: i64 = db
        .with(|mut conn| async move {
            let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entrytag")
                .fetch_one(&mut *conn)
                .await?;
            Ok(links)
        })
        .await
        .expect("count");
    assert_eq!(links, 0, "the link is gone");
}

#[tokio::test]
async fn renaming_a_label_keeps_its_links() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let tag_id = db
        .with(|mut conn| async move { tag_dao::find_or_create(&mut conn, id, "todo").await })
        .await
        .expect("create");
    let renamed = db
        .with(|mut conn| async move { tag_dao::rename(&mut conn, tag_id, id, "later").await })
        .await
        .expect("rename");
    assert!(renamed);

    let tag = db
        .with(|mut conn| async move { tag_dao::find_by_id(&mut conn, tag_id, id).await })
        .await
        .expect("read")
        .expect("tag");
    assert_eq!(tag.name, "later");
    assert_eq!(tag.id, tag_id, "the id is stable, so entrytag links hold");
}

#[test]
fn tag_normalize_rejects_the_names_the_schema_cannot_store() {
    assert!(Tag::normalize_name("a,b").is_err());
    assert!(Tag::normalize_name("#solo").unwrap() == "solo");
}

// --- folders ---------------------------------------------------------------

#[tokio::test]
async fn deleting_a_folder_orphans_its_feeds_rather_than_deleting_them() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let folder = db
        .with(|mut conn| async move {
            cat_dao::insert(
                &mut conn,
                &cat_dao::NewCategory {
                    id_user: id,
                    name: "News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )
            .await
        })
        .await
        .expect("folder");
    let feed = create_feed(&db, id, "https://example.com/f", Some(folder)).await;

    db.with(|mut conn| async move { cat_dao::delete(&mut conn, folder, id).await })
        .await
        .expect("delete folder");

    let f = db
        .with(|mut conn| async move { feed_dao::find_by_id(&mut conn, feed, id).await })
        .await
        .expect("read")
        .expect("feed survives");
    assert_eq!(f.category, None, "the feed becomes an orphan");
}

#[tokio::test]
async fn folders_reorder_and_read_back_in_sidebar_order() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let seeded = db
        .with(|mut conn| async move { user_dao::default_category_id(&mut conn, id).await })
        .await
        .expect("read")
        .expect("the seeded Uncategorized folder");

    let ids: Vec<i64> = db
        .with(|mut conn| async move {
            let mut out = Vec::new();
            for name in ["Alpha", "Beta", "Gamma"] {
                let position = cat_dao::next_position(&mut conn, id).await?;
                out.push(
                    cat_dao::insert(
                        &mut conn,
                        &cat_dao::NewCategory {
                            id_user: id,
                            name,
                            kind: Default::default(),
                            position,
                            attributes: Attributes::new(),
                        },
                    )
                    .await?,
                );
            }
            Ok(out)
        })
        .await
        .expect("create");

    // The sidebar always submits the complete ordering, so the seeded folder is included.
    let reordered = [ids[2], ids[0], seeded, ids[1]];
    let updated = db
        .with(|mut conn| async move { cat_dao::reorder(&mut conn, id, &reordered).await })
        .await
        .expect("reorder");
    assert_eq!(updated, 4, "every folder in the list is repositioned");

    let names: Vec<String> = db
        .with(|mut conn| async move {
            Ok(cat_dao::list(&mut conn, id)
                .await?
                .into_iter()
                .map(|c| c.name)
                .collect())
        })
        .await
        .expect("list");
    assert_eq!(names, vec!["Gamma", "Alpha", DEFAULT_CATEGORY_NAME, "Beta"]);
}

#[tokio::test]
async fn positions_do_not_collide_when_folders_are_appended() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let positions: Vec<i64> = db
        .with(|mut conn| async move {
            let mut out = Vec::new();
            for name in ["Alpha", "Beta"] {
                let position = cat_dao::next_position(&mut conn, id).await?;
                let cid = cat_dao::insert(
                    &mut conn,
                    &cat_dao::NewCategory {
                        id_user: id,
                        name,
                        kind: Default::default(),
                        position,
                        attributes: Attributes::new(),
                    },
                )
                .await?;
                out.push(
                    cat_dao::find_by_id(&mut conn, cid, id)
                        .await?
                        .expect("folder")
                        .position,
                );
            }
            Ok(out)
        })
        .await
        .expect("create");

    let mut sorted = positions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        positions.len(),
        "next_position must not repeat"
    );
}

#[tokio::test]
async fn reordering_ignores_ids_the_user_does_not_own() {
    let db = db().await;
    let alice = create_user(&db, "alice").await;
    let bob = create_user(&db, "bob").await;
    let bob_folder = db
        .with(|mut conn| async move {
            cat_dao::insert(
                &mut conn,
                &cat_dao::NewCategory {
                    id_user: bob,
                    name: "Bob",
                    kind: Default::default(),
                    position: 5,
                    attributes: Attributes::new(),
                },
            )
            .await
        })
        .await
        .expect("bob folder");

    let updated = db
        .with(|mut conn| async move { cat_dao::reorder(&mut conn, alice, &[bob_folder]).await })
        .await
        .expect("reorder");
    assert_eq!(updated, 0, "alice cannot reorder bob's folder");

    let unchanged = db
        .with(|mut conn| async move { cat_dao::find_by_id(&mut conn, bob_folder, bob).await })
        .await
        .expect("read")
        .expect("folder");
    assert_eq!(unchanged.position, 5);
}

// --- sessions --------------------------------------------------------------

#[tokio::test]
async fn sessions_expire_and_can_be_revoked() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let s = db
        .with(|mut conn| async move {
            session::create(&mut conn, id, 3600, Some("1.2.3.4"), Some("ua")).await
        })
        .await
        .expect("create");
    assert!(!s.is_expired());
    assert!(
        s.id.len() >= 32,
        "the id is a bearer token and must be long"
    );

    let sid = s.id.clone();
    let valid = db
        .with(move |mut conn| async move { session::find_valid(&mut conn, &sid).await })
        .await
        .expect("lookup")
        .expect("valid");
    assert_eq!(valid.id_user, id);
    assert_eq!(valid.ip.as_deref(), Some("1.2.3.4"));

    let sid = s.id.clone();
    assert!(db
        .with(move |mut conn| async move { session::revoke(&mut conn, &sid).await })
        .await
        .expect("revoke"));
    let sid = s.id.clone();
    assert!(db
        .with(move |mut conn| async move { session::find_valid(&mut conn, &sid).await })
        .await
        .expect("lookup")
        .is_none());
}

#[tokio::test]
async fn a_session_for_a_disabled_account_stops_working() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let s = db
        .with(|mut conn| async move { session::create(&mut conn, id, 3600, None, None).await })
        .await
        .expect("create");
    db.with(|mut conn| async move { user_dao::set_enabled(&mut conn, id, false).await })
        .await
        .expect("disable");
    assert!(
        db.with(|mut conn| async move { session::find_valid(&mut conn, &s.id).await })
            .await
            .expect("lookup")
            .is_none(),
        "disabling an account must invalidate its sessions"
    );
}

#[tokio::test]
async fn revoking_other_sessions_keeps_the_current_one() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let (keep, other) = db
        .with(|mut conn| async move {
            Ok((
                session::create(&mut conn, id, 3600, None, None).await?,
                session::create(&mut conn, id, 3600, None, None).await?,
            ))
        })
        .await
        .expect("create two");
    assert_ne!(keep.id, other.id, "ids must not collide");

    let keep_id = keep.id.clone();
    let revoked = db
        .with(move |mut conn| async move { session::revoke_others(&mut conn, id, &keep_id).await })
        .await
        .expect("revoke others");
    assert_eq!(revoked, 1);

    let keep_id = keep.id.clone();
    let other_id = other.id.clone();
    db.with(move |mut conn| async move {
        assert!(session::find_valid(&mut conn, &keep_id).await?.is_some());
        assert!(session::find_valid(&mut conn, &other_id).await?.is_none());
        assert_eq!(session::list_for_user(&mut conn, id).await?.len(), 1);
        Ok(())
    })
    .await
    .expect("assert");
}

#[tokio::test]
async fn expired_sessions_are_swept() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let live = db
        .with(|mut conn| async move { session::create(&mut conn, id, 3600, None, None).await })
        .await
        .expect("live");
    db.with(|mut conn| async move {
        // Backdate a second session so it is already expired.
        sqlx::query(
            "INSERT INTO session (id, id_user, created_at, expires_at) VALUES ('old', ?1, 0, 1)",
        )
        .bind(id)
        .execute(&mut *conn)
        .await?;
        Ok(())
    })
    .await
    .expect("backdate");

    let swept = db
        .with(|mut conn| async move { session::sweep_expired(&mut conn).await })
        .await
        .expect("sweep");
    assert_eq!(swept, 1);
    assert!(db
        .with(|mut conn| async move { session::find_valid(&mut conn, &live.id).await })
        .await
        .expect("lookup")
        .is_some());
}

// --- user config -----------------------------------------------------------

#[tokio::test]
async fn user_config_round_trips_through_the_database() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    let updated = db
        .with(|mut conn| async move {
            let cfg = UserConfig {
                ttl_default: 900,
                sort: "date".into(),
                sort_order: SortOrder::Asc,
                posts_per_page: 25,
                ..UserConfig::default()
            };
            user_dao::update_config(&mut conn, id, &cfg).await?;
            Ok(user_dao::find_by_id(&mut conn, id)
                .await?
                .expect("user")
                .config)
        })
        .await
        .expect("update");

    assert_eq!(updated.ttl_default, 900);
    assert_eq!(updated.sort, "date");
    assert_eq!(updated.sort_order, SortOrder::Asc);
    assert_eq!(updated.posts_per_page, 25);
    assert_eq!(
        updated.archiving.keep_max,
        Some(200),
        "unset fields keep defaults"
    );
}

#[tokio::test]
async fn a_corrupt_config_falls_back_to_defaults_instead_of_locking_the_user_out() {
    let db = db().await;
    let id = create_user(&db, "alice").await;
    db.with(|mut conn| async move {
        sqlx::query("UPDATE user SET config = '{not json' WHERE id = ?1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        Ok(())
    })
    .await
    .expect("corrupt");

    let user = db
        .with(|mut conn| async move { user_dao::find_by_id(&mut conn, id).await })
        .await
        .expect("read")
        .expect("user still loads");
    assert_eq!(user.config.ttl_default, 3600);
}
