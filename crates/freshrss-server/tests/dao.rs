//! DAO integration tests against the real schema.
//!
//! The unit tests in each DAO module cover row mapping and SQL fragments; these cover the
//! invariants that only show up once the tables, foreign keys and `id_user` scoping are all live.
//! The multi-user isolation cases are the important ones: FreshRSS never had to enforce isolation
//! because each user got their own database file, so nothing upstream will warn us when a query
//! forgets its `id_user` predicate.

use freshrss_server::db::Db;
use freshrss_server::db::dao::{category as cat_dao, entry as entry_dao, feed as feed_dao, meta, session, tag as tag_dao, user as user_dao};
use freshrss_server::domain::attributes::Attributes;
use freshrss_server::domain::category::DEFAULT_CATEGORY_NAME;
use freshrss_server::domain::entry::Entry;
use freshrss_server::domain::feed::Feed;
use freshrss_server::domain::ids::EntryId;
use freshrss_server::domain::priority::Priority;
use freshrss_server::domain::tag::Tag;
use freshrss_server::domain::ttl::Ttl;
use freshrss_server::domain::user::{SortOrder, UserConfig};
use rusqlite::params;

fn db() -> Db {
    Db::in_memory().expect("in-memory db")
}

fn create_user(db: &Db, username: &str) -> i64 {
    db.blocking_with(|conn| {
        Ok(user_dao::insert(
            conn,
            &user_dao::NewUser {
                username,
                email: Some(&format!("{username}@example.com")),
                password_hash: "argon2id$hash",
                is_admin: false,
            },
        )?)
    })
    .expect("create user")
}

fn create_feed(db: &Db, id_user: i64, url: &str, category: Option<i64>) -> i64 {
    db.blocking_with(|conn| {
        let feed = Feed {
            category,
            ..Feed::new(id_user, url, "Example")
        };
        Ok(feed_dao::insert(conn, &feed)?)
    })
    .expect("create feed")
}

fn stage_entry(db: &Db, id_user: i64, id_feed: i64, guid: &str, title: &str, date: i64) {
    db.blocking_with(|conn| {
        let mut entry = Entry::new(id_user, id_feed, guid);
        entry.id = EntryId::now();
        entry.title = title.to_string();
        entry.link = format!("https://example.com/{guid}");
        entry.content = format!("<p>{title}</p>");
        entry.date = date;
        entry.last_seen = date;
        entry.hash = Some([7u8; 16]);
        entry_dao::stage(conn, &entry)?;
        Ok(())
    })
    .expect("stage");
}

// --- users -----------------------------------------------------------------

#[test]
fn registering_a_user_seeds_their_uncategorized_folder() {
    let db = db();
    let id = create_user(&db, "alice");
    let (config, folders): (Option<UserConfig>, Vec<String>) = db
        .blocking_with(|conn| {
            let user = user_dao::find_by_id(conn, id)?.expect("user exists");
            let folders = cat_dao::list(conn, id)?
                .into_iter()
                .map(|c| c.name)
                .collect();
            Ok((Some(user.config), folders))
        })
        .expect("read");

    assert_eq!(folders, vec![DEFAULT_CATEGORY_NAME.to_string()]);
    assert_eq!(config.unwrap().ttl_default, 3600);
}

#[test]
fn each_user_gets_their_own_uncategorized_folder() {
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");

    let ids: (i64, i64) = db
        .blocking_with(|conn| {
            Ok((
                user_dao::default_category_id(conn, alice)?.expect("alice's folder"),
                user_dao::default_category_id(conn, bob)?.expect("bob's folder"),
            ))
        })
        .expect("read");
    assert_ne!(ids.0, ids.1, "the folder must not be shared");
}

#[test]
fn username_lookup_is_case_insensitive_but_preserves_the_stored_form() {
    let db = db();
    create_user(&db, "Alice");
    let found = db
        .blocking_with(|conn| Ok(user_dao::find_by_username(conn, "aLiCe")?))
        .expect("read")
        .expect("found");
    assert_eq!(found.username, "Alice", "the original casing is kept");
}

#[test]
fn deleting_a_user_cascades_to_everything_they_own() {
    let db = db();
    let id = create_user(&db, "alice");
    let folder = db
        .blocking_with(|conn| {
            Ok(cat_dao::insert(
                conn,
                &cat_dao::NewCategory {
                    id_user: id,
                    name: "News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )?)
        })
        .expect("folder");
    let feed = create_feed(&db, id, "https://example.com/f", Some(folder));
    stage_entry(&db, id, feed, "g1", "Title", 1_700_000_000);
    db.blocking_with(|conn| {
        entry_dao::commit_staged(conn, id)?;
        let tag = tag_dao::find_or_create(conn, id, "todo")?;
        let entry = entry_dao::find_by_guid(conn, feed, "g1")?.expect("entry");
        tag_dao::tag_entry(conn, tag, entry.id.micros(), true)?;
        session::create(conn, id, 3600, Some("127.0.0.1"), Some("test"))?;
        Ok(())
    })
    .expect("seed");

    db.blocking_with(|conn| Ok(user_dao::delete(conn, id)?))
        .expect("delete");

    let leftovers = db
        .blocking_with(|conn| {
            Ok((
                conn.query_row("SELECT COUNT(*) FROM category", [], |r| r.get::<_, i64>(0))?,
                conn.query_row("SELECT COUNT(*) FROM feed", [], |r| r.get::<_, i64>(0))?,
                conn.query_row("SELECT COUNT(*) FROM entry", [], |r| r.get::<_, i64>(0))?,
                conn.query_row("SELECT COUNT(*) FROM tag", [], |r| r.get::<_, i64>(0))?,
                conn.query_row("SELECT COUNT(*) FROM entrytag", [], |r| r.get::<_, i64>(0))?,
                conn.query_row("SELECT COUNT(*) FROM session", [], |r| r.get::<_, i64>(0))?,
            ))
        })
        .expect("count");
    assert_eq!(leftovers, (0, 0, 0, 0, 0, 0), "nothing may survive the user");
}

#[test]
fn a_disabled_user_is_inert_but_their_rows_survive() {
    let db = db();
    let id = create_user(&db, "alice");
    db.blocking_with(|conn| Ok(user_dao::set_enabled(conn, id, false)?))
        .expect("disable");
    let user = db
        .blocking_with(|conn| Ok(user_dao::find_by_id(conn, id)?))
        .expect("read")
        .expect("still present");
    assert!(!user.enabled);
}

// --- multi-user isolation --------------------------------------------------

/// The invariant FreshRSS never needed: one user must not be able to read or write another's rows.
#[test]
fn no_dao_leaks_rows_across_users() {
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");

    // Alice owns a folder, a feed, an article and a label.
    let (alice_folder, alice_feed) = db
        .blocking_with(|conn| {
            let folder = cat_dao::insert(
                conn,
                &cat_dao::NewCategory {
                    id_user: alice,
                    name: "Alice News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )?;
            let feed = feed_dao::insert(conn, &Feed::new(alice, "https://alice.example/f", "Alice"))?;
            Ok((folder, feed))
        })
        .expect("seed alice");
    stage_entry(&db, alice, alice_feed, "g1", "Alice article", 1_700_000_000);
    db.blocking_with(|conn| {
        entry_dao::commit_staged(conn, alice)?;
        tag_dao::find_or_create(conn, alice, "alice-label")?;
        Ok(())
    })
    .expect("commit alice");

    let bob_folder = db
        .blocking_with(|conn| {
            Ok(cat_dao::insert(
                conn,
                &cat_dao::NewCategory {
                    id_user: bob,
                    name: "Bob News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )?)
        })
        .expect("seed bob");

    db.blocking_with(|conn| {
        // Reads by id must not resolve across users.
        assert!(
            cat_dao::find_by_id(conn, alice_folder, bob)?.is_none(),
            "bob must not see alice's folder"
        );
        assert!(
            feed_dao::find_by_id(conn, alice_feed, bob)?.is_none(),
            "bob must not see alice's feed"
        );
        assert!(
            tag_dao::find_by_name(conn, bob, "alice-label")?.is_none(),
            "bob must not see alice's label"
        );

        // Listings must be scoped.
        assert_eq!(cat_dao::list(conn, bob)?.len(), 2, "bob sees only his own two folders");
        assert_eq!(feed_dao::list(conn, bob)?.len(), 0);
        assert_eq!(tag_dao::list(conn, bob)?.len(), 0);
        assert_eq!(
            entry_dao::count_states(conn, bob)?.total,
            0,
            "bob has no articles"
        );
        assert_eq!(entry_dao::count_states(conn, alice)?.total, 1);

        // Writes scoped by user must not touch another user's rows.
        let alice_entry = entry_dao::find_by_guid(conn, alice_feed, "g1")?.expect("alice's entry");
        let marked = entry_dao::mark_read(conn, bob, &[alice_entry.id], true)?;
        assert_eq!(marked, 0, "bob cannot mark alice's article read");

        assert!(!feed_dao::update_name(conn, alice_feed, bob, "hijacked")?);
        assert!(!feed_dao::update_priority(conn, alice_feed, bob, Priority::HIDDEN)?);
        assert!(!feed_dao::delete(conn, alice_feed, bob)?);
        assert!(!cat_dao::delete(conn, alice_folder, bob)?);
        assert!(!cat_dao::update_name(conn, alice_folder, bob, "hijacked")?);

        // Everything alice owns is still intact.
        assert_eq!(feed_dao::list(conn, alice)?.len(), 1);
        let feed = feed_dao::find_by_id(conn, alice_feed, alice)?.expect("feed survives");
        assert_eq!(feed.name, "Alice", "the rename must not have applied");
        assert_eq!(feed.priority, Priority::MAIN_STREAM);
        let entry = entry_dao::find_by_id(conn, alice_entry.id, alice)?.expect("entry survives");
        assert!(!entry.is_read);

        assert!(bob_folder > 0);
        Ok(())
    })
    .expect("isolation checks");
}

#[test]
fn the_same_feed_url_may_be_subscribed_by_two_users() {
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");
    let url = "https://shared.example/feed.xml";

    let (a, b) = db
        .blocking_with(|conn| {
            Ok((
                feed_dao::insert(conn, &Feed::new(alice, url, "Shared"))?,
                feed_dao::insert(conn, &Feed::new(bob, url, "Shared"))?,
            ))
        })
        .expect("insert both");
    assert_ne!(a, b);

    db.blocking_with(|conn| {
        assert!(feed_dao::find_by_url(conn, alice, url)?.is_some());
        assert!(feed_dao::find_by_url(conn, bob, url)?.is_some());
        let carol = 999;
        assert!(
            feed_dao::find_by_url(conn, carol, url)?.is_none(),
            "a user who has not subscribed sees nothing"
        );
        Ok(())
    })
    .expect("lookups");
}

#[test]
fn the_same_label_name_is_independent_per_user() {
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");
    let (a, b) = db
        .blocking_with(|conn| {
            Ok((
                tag_dao::find_or_create(conn, alice, "todo")?,
                tag_dao::find_or_create(conn, bob, "todo")?,
            ))
        })
        .expect("create");
    assert_ne!(a, b, "each user gets their own row");

    // Calling twice for the same user returns the same row rather than inserting.
    let again = db
        .blocking_with(|conn| Ok(tag_dao::find_or_create(conn, alice, "todo")?))
        .expect("again");
    assert_eq!(again, a);
}

// --- entry ids and staging -------------------------------------------------

#[test]
fn committing_staged_entries_allocates_dense_chronological_ids() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);

    // Staged out of publication order on purpose.
    stage_entry(&db, id, feed, "newest", "Newest", 1_700_000_300);
    stage_entry(&db, id, feed, "oldest", "Oldest", 1_700_000_100);
    stage_entry(&db, id, feed, "middle", "Middle", 1_700_000_200);

    let inserted = db
        .blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");
    assert_eq!(inserted, 3);

    let ordered: Vec<(String, i64)> = db
        .blocking_with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT guid, id FROM entry WHERE id_user = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
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

#[test]
fn a_second_commit_never_reuses_or_precedes_an_earlier_id() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);

    stage_entry(&db, id, feed, "first", "First", 1_700_000_100);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit 1");
    let high_water_1 = db
        .blocking_with(|conn| Ok(meta::entry_id_high_water(conn)?))
        .expect("hwm");

    stage_entry(&db, id, feed, "second", "Second", 1_700_000_200);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit 2");

    let ids: Vec<i64> = db
        .blocking_with(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM entry WHERE id_user = ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, i64>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .expect("read");

    assert_eq!(ids.len(), 2);
    assert!(ids[1] > ids[0], "ids must be monotonic across commits");
    assert!(ids[0] <= high_water_1);
    assert!(ids[1] > high_water_1, "the second batch starts above the first");
}

#[test]
fn concurrent_commits_from_two_users_do_not_collide_or_drop_articles() {
    // This is the regression the global allocator exists to prevent. FreshRSS's
    // `rowid + (MAX(id) - COUNT(*))` renumbering is only safe one-user-per-database; with a shared
    // database two simultaneous refreshes would target overlapping id ranges, and because the
    // commit is INSERT OR IGNORE the loser's articles vanish without an error.
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");
    let alice_feed = create_feed(&db, alice, "https://alice.example/f", None);
    let bob_feed = create_feed(&db, bob, "https://bob.example/f", None);

    let per_user = 50;
    for i in 0..per_user {
        stage_entry(&db, alice, alice_feed, &format!("a{i}"), "A", 1_700_000_000 + i);
        stage_entry(&db, bob, bob_feed, &format!("b{i}"), "B", 1_700_000_000 + i);
    }

    // Both users commit; the writer mutex serializes them, which is the point.
    let (alice_committed, bob_committed) = db
        .blocking_with_writer(|conn| {
            Ok((
                entry_dao::commit_staged(conn, alice)?,
                entry_dao::commit_staged(conn, bob)?,
            ))
        })
        .expect("commit both");

    assert_eq!(alice_committed, per_user as usize);
    assert_eq!(bob_committed, per_user as usize);

    let (alice_count, bob_count, distinct_ids) = db
        .blocking_with(|conn| {
            Ok((
                entry_dao::count_states(conn, alice)?.total,
                entry_dao::count_states(conn, bob)?.total,
                conn.query_row("SELECT COUNT(DISTINCT id) FROM entry", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            ))
        })
        .expect("counts");

    assert_eq!(alice_count, i64::from(per_user), "alice lost no articles");
    assert_eq!(bob_count, i64::from(per_user), "bob lost no articles");
    assert_eq!(
        distinct_ids,
        i64::from(per_user) * 2,
        "every id must be unique across users"
    );
}

#[test]
fn staging_the_same_guid_twice_inserts_one_article() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);

    stage_entry(&db, id, feed, "dup", "First", 1_700_000_100);
    stage_entry(&db, id, feed, "dup", "Second", 1_700_000_200);

    let inserted = db
        .blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");
    assert_eq!(inserted, 1, "the duplicate is ignored, not doubled");
    assert_eq!(
        entry_count(&db, id),
        1,
        "exactly one article exists for that guid"
    );
}

#[test]
fn committing_nothing_is_a_noop_that_does_not_burn_ids() {
    let db = db();
    let id = create_user(&db, "alice");
    let before = db
        .blocking_with(|conn| Ok(meta::entry_id_high_water(conn)?))
        .expect("hwm");
    let inserted = db
        .blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");
    assert_eq!(inserted, 0);
    let after = db
        .blocking_with(|conn| Ok(meta::entry_id_high_water(conn)?))
        .expect("hwm");
    assert_eq!(before, after, "an empty refresh must not advance the sequence");
}

#[test]
fn discarding_staged_rows_removes_them() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_000);

    let discarded = db
        .blocking_with(|conn| Ok(entry_dao::discard_staged(conn, id)?))
        .expect("discard");
    assert_eq!(discarded, 1);

    let inserted = db
        .blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");
    assert_eq!(inserted, 0);
    assert_eq!(entry_count(&db, id), 0);
}

fn entry_count(db: &Db, id_user: i64) -> i64 {
    db.blocking_with(|conn| Ok(entry_dao::count_states(conn, id_user)?.total))
        .expect("count")
}

// --- reconciliation --------------------------------------------------------

#[test]
fn updating_an_article_keeps_its_id_and_the_users_state() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "Original", 1_700_000_100);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    // The reader marks it read and stars it.
    let original = db
        .blocking_with(|conn| Ok(entry_dao::find_by_guid(conn, feed, "g1")?))
        .expect("read")
        .expect("entry");
    db.blocking_with(|conn| {
        entry_dao::mark_read(conn, id, &[original.id], true)?;
        entry_dao::mark_favorite(conn, id, &[original.id], true)?;
        Ok(())
    })
    .expect("mark");

    // An upstream edit arrives.
    db.blocking_with_writer(|conn| {
        let mut updated = Entry::new(id, feed, "g1");
        updated.title = "Corrected".into();
        updated.content = "<p>new body</p>".into();
        updated.link = original.link.clone();
        updated.date = 1_700_000_999;
        updated.last_seen = 1_700_001_000;
        updated.last_modified = Some(1_700_001_000);
        updated.hash = Some([9u8; 16]);
        // is_read / is_favorite left at their defaults: `None`-style COALESCE keeps the DB value.
        assert!(entry_dao::update_existing(conn, &updated)?);
        Ok(())
    })
    .expect("update");

    let after = db
        .blocking_with(|conn| Ok(entry_dao::find_by_guid(conn, feed, "g1")?))
        .expect("read")
        .expect("entry");

    assert_eq!(after.id, original.id, "the id must not change on update");
    assert_eq!(after.title, "Corrected");
    assert_eq!(after.content, "<p>new body</p>");
    assert_eq!(after.hash, Some([9u8; 16]));
    assert_eq!(after.last_modified, Some(1_700_001_000));
    assert!(after.is_read, "an upstream edit must not clear the read flag");
    assert!(after.is_favorite, "an upstream edit must not un-star");
    assert_eq!(
        entry_count(&db, id),
        1,
        "an update must not create a second article"
    );
}

#[test]
fn list_hashes_for_guids_answers_seen_and_changed_in_one_query() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "One", 1_700_000_100);
    stage_entry(&db, id, feed, "g2", "Two", 1_700_000_200);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let hashes = db
        .blocking_with(|conn| {
            Ok(entry_dao::list_hashes_for_guids(
                conn,
                feed,
                &["g1".to_string(), "g2".to_string(), "g3".to_string()],
            )?)
        })
        .expect("lookup");

    assert_eq!(hashes.len(), 2, "g3 was never seen");
    assert_eq!(hashes["g1"], [7u8; 16]);
    assert!(!hashes.contains_key("g3"));
}

#[test]
fn last_seen_advances_for_guids_still_published_and_stops_for_gone_ones() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "kept", "Kept", 1_700_000_100);
    stage_entry(&db, id, feed, "gone", "Gone", 1_700_000_100);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let later = 1_700_009_999;
    let touched = db
        .blocking_with(|conn| {
            Ok(entry_dao::update_last_seen(
                conn,
                feed,
                &["kept".to_string()],
                later,
            )?)
        })
        .expect("update last seen");
    assert_eq!(touched, 1);

    db.blocking_with(|conn| {
        let kept = entry_dao::find_by_guid(conn, feed, "kept")?.expect("kept");
        let gone = entry_dao::find_by_guid(conn, feed, "gone")?.expect("gone");
        assert_eq!(kept.last_seen, later, "still published, so it slides forward");
        assert!(
            gone.last_seen < later,
            "an article that vanished keeps its old lastSeen; retention acts on that later"
        );
        Ok(())
    })
    .expect("assert");
}

// --- point mutations -------------------------------------------------------

#[test]
fn marking_read_only_touches_rows_that_actually_change() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    for i in 0..3 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i);
    }
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let ids = all_ids(&db, id);
    let first = db
        .blocking_with(|conn| Ok(entry_dao::mark_read(conn, id, &ids[..2], true)?))
        .expect("mark");
    assert_eq!(first, 2);

    let again = db
        .blocking_with(|conn| Ok(entry_dao::mark_read(conn, id, &ids[..2], true)?))
        .expect("mark again");
    assert_eq!(again, 0, "already-read articles are not rewritten");

    let unread = db
        .blocking_with(|conn| Ok(entry_dao::count_states(conn, id)?))
        .expect("counts");
    assert_eq!(unread.unread, 1);
    assert_eq!(unread.read(), 2);
}

#[test]
fn marking_read_a_second_time_preserves_the_original_user_timestamp() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let ids = all_ids(&db, id);
    db.blocking_with(|conn| Ok(entry_dao::mark_read(conn, id, &ids, true)?))
        .expect("mark");
    let first = last_user_modified(&db, ids[0]);
    assert!(first.is_some(), "a user action stamps the column");

    // The `is_read <> ?1` guard means a repeat call cannot churn the timestamp.
    db.blocking_with(|conn| Ok(entry_dao::mark_read(conn, id, &ids, true)?))
        .expect("mark again");
    assert_eq!(last_user_modified(&db, ids[0]), first);
}

fn last_user_modified(db: &Db, id: EntryId) -> Option<i64> {
    db.blocking_with(|conn| {
        Ok(conn
            .query_row(
                "SELECT lastUserModified FROM entry WHERE id = ?1",
                params![id.micros()],
                |r| r.get::<_, Option<i64>>(0),
            )
            .expect("row"))
    })
    .expect("read")
}

fn all_ids(db: &Db, id_user: i64) -> Vec<EntryId> {
    db.blocking_with(|conn| {
        let mut stmt = conn.prepare("SELECT id FROM entry WHERE id_user = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![id_user], |r| r.get::<_, i64>(0))?;
        let ids: Vec<i64> = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(ids.into_iter().map(EntryId::from_micros).collect())
    })
    .expect("ids")
}

#[test]
fn mark_all_read_respects_the_id_max_snapshot_fence() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    for i in 0..4 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i);
    }
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let ids = all_ids(&db, id);
    // "Mark all as read" means everything the reader had loaded, not articles that arrived since.
    let fence = ids[1];
    let marked = db
        .blocking_with(|conn| {
            Ok(entry_dao::mark_read_all(conn, id, fence, 1_700_099_999)?)
        })
        .expect("mark all");
    assert_eq!(marked, 2, "only the two at or below the fence");

    let states = db
        .blocking_with(|conn| Ok(entry_dao::count_states(conn, id)?))
        .expect("counts");
    assert_eq!(states.unread, 2);
}

// --- cached counters -------------------------------------------------------

#[test]
fn cached_counters_track_entries_and_unreads() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    for i in 0..3 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i);
    }
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    db.blocking_with(|conn| Ok(feed_dao::update_cached_values(conn, &[feed])?))
        .expect("refresh counters");
    let f = db
        .blocking_with(|conn| Ok(feed_dao::find_by_id(conn, feed, id)?))
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_entries, 3);
    assert_eq!(f.cache_nb_unreads, 3);

    let ids = all_ids(&db, id);
    db.blocking_with(|conn| Ok(entry_dao::mark_read(conn, id, &ids[..1], true)?))
        .expect("mark");
    db.blocking_with(|conn| Ok(feed_dao::bump_cache_unreads(conn, feed, -1)?))
        .expect("bump");

    let f = db
        .blocking_with(|conn| Ok(feed_dao::find_by_id(conn, feed, id)?))
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_unreads, 2);
    assert_eq!(f.cache_nb_entries, 3, "total is unaffected by reading");
}

#[test]
fn the_unread_counter_never_goes_negative() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    db.blocking_with(|conn| Ok(feed_dao::bump_cache_unreads(conn, feed, -5)?))
        .expect("bump");
    let f = db
        .blocking_with(|conn| Ok(feed_dao::find_by_id(conn, feed, id)?))
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_unreads, 0);
}

#[test]
fn truncating_a_feed_empties_it_and_forces_a_full_refetch() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");
    db.blocking_with(|conn| {
        feed_dao::update_last_update(conn, feed, 1_700_000_200)?;
        feed_dao::update_cached_values(conn, &[feed])?;
        Ok(())
    })
    .expect("counters");

    let deleted = db
        .blocking_with(|conn| Ok(feed_dao::truncate_entries(conn, feed, id)?))
        .expect("truncate");
    assert_eq!(deleted, 1);

    let f = db
        .blocking_with(|conn| Ok(feed_dao::find_by_id(conn, feed, id)?))
        .expect("read")
        .expect("feed");
    assert_eq!(f.cache_nb_entries, 0);
    assert_eq!(f.cache_nb_unreads, 0);
    assert_eq!(f.last_update, 0, "the next refresh treats the feed as new");
    assert_eq!(entry_count(&db, id), 0);
}

// --- refresh scheduling ----------------------------------------------------

#[test]
fn a_never_fetched_feed_is_due_immediately() {
    let db = db();
    let id = create_user(&db, "alice");
    create_feed(&db, id, "https://example.com/f", None);
    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_700_000_000, 10)?))
        .expect("due");
    assert_eq!(due.len(), 1);
}

#[test]
fn a_recently_refreshed_feed_is_not_due() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    db.blocking_with(|conn| Ok(feed_dao::update_last_update(conn, feed, 1_700_000_000)?))
        .expect("refresh");

    // ttl is 0, so the owner's ttl_default (3600) applies.
    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_700_000_100, 10)?))
        .expect("due");
    assert!(due.is_empty(), "100s into a 3600s ttl is not due");

    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_700_000_000 + 3600, 10)?))
        .expect("due");
    assert_eq!(due.len(), 1, "due once the ttl has elapsed");
}

#[test]
fn ttl_zero_uses_the_owners_default_not_a_global_one() {
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");

    // Bob's account is configured to poll every 60 seconds.
    db.blocking_with(|conn| {
        let mut cfg = UserConfig::default();
        cfg.ttl_default = 60;
        Ok(user_dao::update_config(conn, bob, &cfg)?)
    })
    .expect("configure bob");

    let alice_feed = create_feed(&db, alice, "https://alice.example/f", None);
    let bob_feed = create_feed(&db, bob, "https://bob.example/f", None);
    let now = 1_700_000_000;
    db.blocking_with(|conn| {
        feed_dao::update_last_update(conn, alice_feed, now)?;
        feed_dao::update_last_update(conn, bob_feed, now)?;
        Ok(())
    })
    .expect("refresh both");

    // 120 seconds later: past bob's 60s default, well inside alice's 3600s.
    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, now + 120, 10)?))
        .expect("due");
    let ids: Vec<i64> = due.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![bob_feed], "only bob's feed is due");
    assert!(!ids.contains(&alice_feed));
}

#[test]
fn muted_feeds_are_excluded_from_batch_refresh() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    db.blocking_with(|conn| Ok(feed_dao::update_ttl(conn, feed, id, Ttl::new(3600, true))?))
        .expect("mute");

    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_800_000_000, 10)?))
        .expect("due");
    assert!(due.is_empty(), "a negative ttl means muted, not 'very often'");

    // Still reachable when named explicitly, so the user can force a refresh.
    let direct = db
        .blocking_with(|conn| Ok(feed_dao::find_by_id(conn, feed, id)?))
        .expect("read")
        .expect("feed");
    assert!(direct.is_muted());
}

#[test]
fn a_failed_feed_retries_on_its_error_timestamp() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    let now = 1_700_000_000;
    db.blocking_with(|conn| {
        // A success long ago, then a recent failure: the failure must dominate.
        feed_dao::update_last_update(conn, feed, now)?;
        feed_dao::update_last_error(conn, feed, now + 3000)?;
        Ok(())
    })
    .expect("fail");

    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, now + 3100, 10)?))
        .expect("due");
    assert!(due.is_empty(), "only 100s past the error, inside the 3600s ttl");

    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, now + 3000 + 3600, 10)?))
        .expect("due");
    assert_eq!(due.len(), 1);
}

#[test]
fn due_feeds_are_ordered_oldest_first() {
    let db = db();
    let id = create_user(&db, "alice");
    let a = create_feed(&db, id, "https://a.example/f", None);
    let b = create_feed(&db, id, "https://b.example/f", None);
    db.blocking_with(|conn| {
        feed_dao::update_last_update(conn, b, 1_600_000_000)?;
        feed_dao::update_last_update(conn, a, 1_690_000_000)?;
        Ok(())
    })
    .expect("refresh");

    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_800_000_000, 10)?))
        .expect("due");
    let ids: Vec<i64> = due.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![b, a], "the stalest feed is refreshed first");
}

#[test]
fn feeds_of_a_disabled_user_are_not_refreshed() {
    let db = db();
    let id = create_user(&db, "alice");
    create_feed(&db, id, "https://example.com/f", None);
    db.blocking_with(|conn| Ok(user_dao::set_enabled(conn, id, false)?))
        .expect("disable");

    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_800_000_000, 10)?))
        .expect("due");
    assert!(due.is_empty());
}

#[test]
fn the_due_list_honours_its_limit() {
    let db = db();
    let id = create_user(&db, "alice");
    for i in 0..5 {
        create_feed(&db, id, &format!("https://example.com/f{i}"), None);
    }
    let due = db
        .blocking_with(|conn| Ok(feed_dao::list_due(conn, 1_800_000_000, 2)?))
        .expect("due");
    assert_eq!(due.len(), 2);
}

#[test]
fn problem_feeds_are_listed_for_the_reader_dialog() {
    let db = db();
    let id = create_user(&db, "alice");
    let failing = create_feed(&db, id, "https://failing.example/f", None);
    let muted = create_feed(&db, id, "https://muted.example/f", None);
    create_feed(&db, id, "https://healthy.example/f", None);

    db.blocking_with(|conn| {
        feed_dao::update_last_error(conn, failing, 1_700_000_000)?;
        feed_dao::update_ttl(conn, muted, id, Ttl::new(3600, true))?;
        Ok(())
    })
    .expect("seed");

    let problems = db
        .blocking_with(|conn| Ok(feed_dao::list_with_problems(conn, id)?))
        .expect("problems");
    assert_eq!(problems.len(), 2);
    assert!(problems.iter().any(|f| f.id == failing && f.has_error()));
    assert!(problems.iter().any(|f| f.id == muted && f.is_muted()));
}

// --- labels ----------------------------------------------------------------

#[test]
fn labels_attach_detach_and_list_without_n_plus_one_queries() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    for i in 0..3 {
        stage_entry(&db, id, feed, &format!("g{i}"), "T", 1_700_000_100 + i);
    }
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let ids = all_ids(&db, id);
    db.blocking_with(|conn| {
        let todo = tag_dao::find_or_create(conn, id, "todo")?;
        let read_later = tag_dao::find_or_create(conn, id, "read-later")?;
        tag_dao::tag_entries(conn, todo, &[ids[0].micros(), ids[1].micros()])?;
        tag_dao::tag_entry(conn, read_later, ids[1].micros(), true)?;

        let on_first = tag_dao::labels_for_entry(conn, ids[0].micros())?;
        assert_eq!(on_first.len(), 1);
        assert_eq!(on_first[0].name, "todo");

        // One query for the whole page.
        let all = tag_dao::labels_for_entries(
            conn,
            &ids.iter().map(|i| i.micros()).collect::<Vec<_>>(),
        )?;
        assert_eq!(all[&ids[0].micros()].len(), 1);
        assert_eq!(all[&ids[1].micros()].len(), 2);
        assert!(!all.contains_key(&ids[2].micros()), "unlabelled entries are absent");

        // Attaching twice is a no-op, not an error.
        tag_dao::tag_entry(conn, todo, ids[0].micros(), true)?;
        assert_eq!(tag_dao::labels_for_entry(conn, ids[0].micros())?.len(), 1);

        assert!(tag_dao::untag_entry(conn, todo, ids[0].micros())?);
        assert!(tag_dao::labels_for_entry(conn, ids[0].micros())?.is_empty());
        Ok(())
    })
    .expect("label ops");
}

#[test]
fn label_counts_cover_total_and_unread() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100);
    stage_entry(&db, id, feed, "g2", "T", 1_700_000_200);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let ids = all_ids(&db, id);
    let counts = db
        .blocking_with(|conn| {
            let todo = tag_dao::find_or_create(conn, id, "todo")?;
            tag_dao::tag_entries(conn, todo, &[ids[0].micros(), ids[1].micros()])?;
            entry_dao::mark_read(conn, id, &[ids[0]], true)?;
            let listed = tag_dao::list_with_counts(conn, id)?;
            Ok(listed
                .into_iter()
                .map(|(tag, total, unread)| (tag.name, total, unread))
                .collect::<Vec<_>>())
        })
        .expect("counts");

    assert_eq!(counts, vec![("todo".to_string(), 2, 1)]);
}

#[test]
fn deleting_a_label_unlinks_articles_but_does_not_delete_them() {
    let db = db();
    let id = create_user(&db, "alice");
    let feed = create_feed(&db, id, "https://example.com/f", None);
    stage_entry(&db, id, feed, "g1", "T", 1_700_000_100);
    db.blocking_with_writer(|conn| Ok(entry_dao::commit_staged(conn, id)?))
        .expect("commit");

    let ids = all_ids(&db, id);
    let tag_id = db
        .blocking_with(|conn| {
            let tag = tag_dao::find_or_create(conn, id, "todo")?;
            tag_dao::tag_entry(conn, tag, ids[0].micros(), true)?;
            Ok(tag)
        })
        .expect("label");

    db.blocking_with(|conn| Ok(tag_dao::delete(conn, tag_id, id)?))
        .expect("delete");

    assert_eq!(entry_count(&db, id), 1, "the article survives");
    let links: i64 = db
        .blocking_with(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM entrytag", [], |r| r.get(0))?)
        })
        .expect("count");
    assert_eq!(links, 0, "the link is gone");
}

#[test]
fn renaming_a_label_keeps_its_links() {
    let db = db();
    let id = create_user(&db, "alice");
    let tag_id = db
        .blocking_with(|conn| Ok(tag_dao::find_or_create(conn, id, "todo")?))
        .expect("create");
    let renamed = db
        .blocking_with(|conn| Ok(tag_dao::rename(conn, tag_id, id, "later")?))
        .expect("rename");
    assert!(renamed);

    let tag = db
        .blocking_with(|conn| Ok(tag_dao::find_by_id(conn, tag_id, id)?))
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

#[test]
fn deleting_a_folder_orphans_its_feeds_rather_than_deleting_them() {
    let db = db();
    let id = create_user(&db, "alice");
    let folder = db
        .blocking_with(|conn| {
            Ok(cat_dao::insert(
                conn,
                &cat_dao::NewCategory {
                    id_user: id,
                    name: "News",
                    kind: Default::default(),
                    position: 1,
                    attributes: Attributes::new(),
                },
            )?)
        })
        .expect("folder");
    let feed = create_feed(&db, id, "https://example.com/f", Some(folder));

    db.blocking_with(|conn| Ok(cat_dao::delete(conn, folder, id)?))
        .expect("delete folder");

    let f = db
        .blocking_with(|conn| Ok(feed_dao::find_by_id(conn, feed, id)?))
        .expect("read")
        .expect("feed survives");
    assert_eq!(f.category, None, "the feed becomes an orphan");
}

#[test]
fn folders_reorder_and_read_back_in_sidebar_order() {
    let db = db();
    let id = create_user(&db, "alice");
    let seeded = db
        .blocking_with(|conn| Ok(user_dao::default_category_id(conn, id)?))
        .expect("read")
        .expect("the seeded Uncategorized folder");

    let ids: Vec<i64> = db
        .blocking_with(|conn| {
            let mut out = Vec::new();
            for name in ["Alpha", "Beta", "Gamma"] {
                out.push(cat_dao::insert(
                    conn,
                    &cat_dao::NewCategory {
                        id_user: id,
                        name,
                        kind: Default::default(),
                        position: cat_dao::next_position(conn, id)?,
                        attributes: Attributes::new(),
                    },
                )?);
            }
            Ok(out)
        })
        .expect("create");

    // The sidebar always submits the complete ordering, so the seeded folder is included.
    let reordered = [ids[2], ids[0], seeded, ids[1]];
    let updated = db
        .blocking_with(|conn| Ok(cat_dao::reorder(conn, id, &reordered)?))
        .expect("reorder");
    assert_eq!(updated, 4, "every folder in the list is repositioned");

    let names: Vec<String> = db
        .blocking_with(|conn| {
            Ok(cat_dao::list(conn, id)?
                .into_iter()
                .map(|c| c.name)
                .collect())
        })
        .expect("list");
    assert_eq!(names, vec!["Gamma", "Alpha", DEFAULT_CATEGORY_NAME, "Beta"]);
}

#[test]
fn positions_do_not_collide_when_folders_are_appended() {
    let db = db();
    let id = create_user(&db, "alice");
    let positions: Vec<i64> = db
        .blocking_with(|conn| {
            let mut out = Vec::new();
            for name in ["Alpha", "Beta"] {
                let position = cat_dao::next_position(conn, id)?;
                let cid = cat_dao::insert(
                    conn,
                    &cat_dao::NewCategory {
                        id_user: id,
                        name,
                        kind: Default::default(),
                        position,
                        attributes: Attributes::new(),
                    },
                )?;
                out.push(cat_dao::find_by_id(conn, cid, id)?.expect("folder").position);
            }
            Ok(out)
        })
        .expect("create");

    let mut sorted = positions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), positions.len(), "next_position must not repeat");
}

#[test]
fn reordering_ignores_ids_the_user_does_not_own() {
    let db = db();
    let alice = create_user(&db, "alice");
    let bob = create_user(&db, "bob");
    let bob_folder = db
        .blocking_with(|conn| {
            Ok(cat_dao::insert(
                conn,
                &cat_dao::NewCategory {
                    id_user: bob,
                    name: "Bob",
                    kind: Default::default(),
                    position: 5,
                    attributes: Attributes::new(),
                },
            )?)
        })
        .expect("bob folder");

    let updated = db
        .blocking_with(|conn| Ok(cat_dao::reorder(conn, alice, &[bob_folder])?))
        .expect("reorder");
    assert_eq!(updated, 0, "alice cannot reorder bob's folder");

    let unchanged = db
        .blocking_with(|conn| Ok(cat_dao::find_by_id(conn, bob_folder, bob)?))
        .expect("read")
        .expect("folder");
    assert_eq!(unchanged.position, 5);
}

// --- sessions --------------------------------------------------------------

#[test]
fn sessions_expire_and_can_be_revoked() {
    let db = db();
    let id = create_user(&db, "alice");
    let s = db
        .blocking_with(|conn| Ok(session::create(conn, id, 3600, Some("1.2.3.4"), Some("ua"))?))
        .expect("create");
    assert!(!s.is_expired());
    assert!(s.id.len() >= 32, "the id is a bearer token and must be long");

    let valid = db
        .blocking_with(|conn| Ok(session::find_valid(conn, &s.id)?))
        .expect("lookup")
        .expect("valid");
    assert_eq!(valid.id_user, id);
    assert_eq!(valid.ip.as_deref(), Some("1.2.3.4"));

    assert!(db
        .blocking_with(|conn| Ok(session::revoke(conn, &s.id)?))
        .expect("revoke"));
    assert!(db
        .blocking_with(|conn| Ok(session::find_valid(conn, &s.id)?))
        .expect("lookup")
        .is_none());
}

#[test]
fn a_session_for_a_disabled_account_stops_working() {
    let db = db();
    let id = create_user(&db, "alice");
    let s = db
        .blocking_with(|conn| Ok(session::create(conn, id, 3600, None, None)?))
        .expect("create");
    db.blocking_with(|conn| Ok(user_dao::set_enabled(conn, id, false)?))
        .expect("disable");
    assert!(db
        .blocking_with(|conn| Ok(session::find_valid(conn, &s.id)?))
        .expect("lookup")
        .is_none(), "disabling an account must invalidate its sessions");
}

#[test]
fn revoking_other_sessions_keeps_the_current_one() {
    let db = db();
    let id = create_user(&db, "alice");
    let (keep, other) = db
        .blocking_with(|conn| {
            Ok((
                session::create(conn, id, 3600, None, None)?,
                session::create(conn, id, 3600, None, None)?,
            ))
        })
        .expect("create two");
    assert_ne!(keep.id, other.id, "ids must not collide");

    let revoked = db
        .blocking_with(|conn| Ok(session::revoke_others(conn, id, &keep.id)?))
        .expect("revoke others");
    assert_eq!(revoked, 1);

    db.blocking_with(|conn| {
        assert!(session::find_valid(conn, &keep.id)?.is_some());
        assert!(session::find_valid(conn, &other.id)?.is_none());
        assert_eq!(session::list_for_user(conn, id)?.len(), 1);
        Ok(())
    })
    .expect("assert");
}

#[test]
fn expired_sessions_are_swept() {
    let db = db();
    let id = create_user(&db, "alice");
    let live = db
        .blocking_with(|conn| Ok(session::create(conn, id, 3600, None, None)?))
        .expect("live");
    db.blocking_with(|conn| {
        // Backdate a second session so it is already expired.
        conn.execute(
            "INSERT INTO session (id, id_user, created_at, expires_at) VALUES ('old', ?1, 0, 1)",
            params![id],
        )?;
        Ok(())
    })
    .expect("backdate");

    let swept = db
        .blocking_with(|conn| Ok(session::sweep_expired(conn)?))
        .expect("sweep");
    assert_eq!(swept, 1);
    assert!(db
        .blocking_with(|conn| Ok(session::find_valid(conn, &live.id)?))
        .expect("lookup")
        .is_some());
}

// --- user config -----------------------------------------------------------

#[test]
fn user_config_round_trips_through_the_database() {
    let db = db();
    let id = create_user(&db, "alice");
    let updated = db
        .blocking_with(|conn| {
            let cfg = UserConfig {
                ttl_default: 900,
                sort: "date".into(),
                sort_order: SortOrder::Asc,
                posts_per_page: 25,
                ..UserConfig::default()
            };
            user_dao::update_config(conn, id, &cfg)?;
            Ok(user_dao::find_by_id(conn, id)?.expect("user").config)
        })
        .expect("update");

    assert_eq!(updated.ttl_default, 900);
    assert_eq!(updated.sort, "date");
    assert_eq!(updated.sort_order, SortOrder::Asc);
    assert_eq!(updated.posts_per_page, 25);
    assert_eq!(updated.archiving.keep_max, Some(200), "unset fields keep defaults");
}

#[test]
fn a_corrupt_config_falls_back_to_defaults_instead_of_locking_the_user_out() {
    let db = db();
    let id = create_user(&db, "alice");
    db.blocking_with(|conn| {
        conn.execute(
            "UPDATE user SET config = '{not json' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .expect("corrupt");

    let user = db
        .blocking_with(|conn| Ok(user_dao::find_by_id(conn, id)?))
        .expect("read")
        .expect("user still loads");
    assert_eq!(user.config.ttl_default, 3600);
}
