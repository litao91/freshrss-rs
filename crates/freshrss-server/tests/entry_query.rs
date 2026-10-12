//! The listing builder against a real database.
//!
//! `entry_query`'s own unit tests assert on the SQL it emits, which is the right level for its shape but
//! cannot tell whether the shape *works*. These tests run it. The one that matters most is
//! [`paging_neither_repeats_nor_skips`]: keyset pagination is easy to get subtly wrong in a way that
//! looks fine on a small table and loses articles once a reader scrolls past a few pages.
//!
//! The boundary values a cursor carries are read back with hand-written SQL rather than with the
//! builder's own expressions, so a mistake in one is not reproduced in the other.

mod common;

use freshrss_search::parser::parse;
use freshrss_server::db::dao::entry_query::{count_unread, list, Cursor, CursorValue, EntryQuery};
use freshrss_server::db::dao::{
    category as cat_dao, entry as entry_dao, feed as feed_dao, tag as tag_dao, user as user_dao,
};
use freshrss_server::db::sql::sql;
use freshrss_server::db::Db;
use freshrss_server::domain::attributes::Attributes;
use freshrss_server::domain::category::CategoryKind;
use freshrss_server::domain::entry::Entry;
use freshrss_server::domain::feed::Feed;
use freshrss_server::domain::ids::EntryId;
use freshrss_server::domain::list_type::ListType;
use freshrss_server::domain::priority::Priority;
use freshrss_server::domain::sort::{Order, SecondarySort, Sort};
use freshrss_server::domain::state::EntryState;
use freshrss_server::{Error, Result};
use sqlx::{Row as _, TypeInfo as _, ValueRef as _};

use common::{insert_entry, SeededEntry};

const NOW: i64 = 1_700_000_000;
const PAGE: usize = 7;
const ARTICLES: i64 = 240;

struct Corpus {
    db: Db,
    id_user: i64,
    other_user: i64,
    feeds: Vec<i64>,
    categories: Vec<i64>,
    labels: Vec<i64>,
}

/// Seeds one user's articles across four feeds in three folders, plus a second user whose articles use
/// the same vocabulary and an overlapping id range.
async fn corpus() -> Corpus {
    let db = common::db().await;

    let (id_user, other_user, feeds, categories, labels) = db
        .with(|mut conn| async move {
            let id_user = user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: "alice",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await?;
            let other_user = user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: "bob",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await?;

            let mut categories = vec![user_dao::default_category_id(&mut conn, id_user)
                .await?
                .expect("seeded")];
            for (i, name) in ["News", "Technology"].into_iter().enumerate() {
                categories.push(
                    cat_dao::insert(
                        &mut conn,
                        &cat_dao::NewCategory {
                            id_user,
                            name,
                            kind: CategoryKind::default(),
                            position: i as i64 + 2,
                            attributes: Attributes::new(),
                        },
                    )
                    .await?,
                );
            }

            // Four feeds at three different priorities, so the scope floors have something to filter.
            let mut feeds = Vec::new();
            for (i, (category, priority)) in [
                (categories[0], Priority::MAIN_STREAM),
                (categories[1], Priority::MAIN_STREAM),
                (categories[2], Priority::IMPORTANT),
                (categories[1], Priority::HIDDEN),
            ]
            .into_iter()
            .enumerate()
            {
                // Two feeds share a name on purpose: the category sort's internal secondary on the
                // feed name is what keeps their articles from interleaving unpredictably.
                let name = if i % 2 == 0 {
                    "Shared name".to_owned()
                } else {
                    format!("Feed {i}")
                };
                let mut f = Feed::new(id_user, format!("https://example.com/feed{i}.xml"), name);
                f.id = feed_dao::insert(&mut conn, &f).await?;
                feed_dao::update_category(&mut conn, f.id, id_user, Some(category)).await?;
                feed_dao::update_priority(&mut conn, f.id, id_user, priority).await?;
                feeds.push(f.id);
            }

            let mut labels = Vec::new();
            for name in ["todo", "reading"] {
                labels.push(tag_dao::insert(&mut conn, id_user, name).await?);
            }

            for i in 0..ARTICLES {
                let feed = feeds[(i % 4) as usize];
                // Ids and publication dates advance independently, so a cursor built from one is not
                // accidentally a cursor for the other.
                let id = EntryId::from_unix(NOW - 86_400 * 30 + i * 900);
                let date = NOW - 86_400 * 60 + i * 3_600;
                // Titles repeat in a short cycle so every sort has real ties to break.
                let title = format!("Article {:03}", i % 17);
                let modified = (i % 5 != 0).then_some(date + 3_600);

                // The folded columns are what the search SQL reads, so the seed has to write them the
                // way the DAOs would; `insert_entry` derives them from the plain text below.
                insert_entry(
                    &mut conn,
                    &SeededEntry {
                        id: id.micros(),
                        id_user,
                        guid: format!("guid-{i}"),
                        author: if i % 2 == 0 { "Ada Lovelace" } else { "bot" }.to_owned(),
                        content: format!(
                            "<p>body {i} with {} words</p>",
                            "filler ".repeat((i % 23) as usize)
                        ),
                        link: format!("https://example.com/post/{i}"),
                        title,
                        date,
                        last_modified: modified,
                        last_user_modified: modified.map(|m| m + 60),
                        hash: vec![7u8; 16],
                        is_read: i % 3 == 0,
                        is_favorite: i % 11 == 0,
                        id_feed: feed,
                        tags: if i % 4 == 0 { "#rust #rss" } else { "#rust" }.to_owned(),
                    },
                )
                .await?;

                if i % 6 == 0 {
                    tag_dao::tag_entry(&mut conn, labels[(i % 2) as usize], id.micros(), true)
                        .await?;
                }
            }

            // Bob's articles sit in Alice's id range and use the same titles, so a missing `id_user`
            // fence shows up as extra rows rather than as an obviously foreign record.
            let mut bob_feed = Feed::new(other_user, "https://other.test/feed.xml", "Shared name");
            bob_feed.id = feed_dao::insert(&mut conn, &bob_feed).await?;
            for i in 0..40 {
                let id = EntryId::from_unix(NOW - 86_400 * 30 + i * 900 + 1);
                let title = format!("Article {:03}", i % 17);
                sqlx::query(
                    "INSERT INTO entry
                       (id, id_user, guid, title, author, content, link, date, lastSeen,
                        hash, is_read, is_favorite, id_feed, tags,
                        title_folded, content_folded, author_folded, link_folded, tags_folded)
                     VALUES (?1,?2,?3,?4,'Ada Lovelace','<p>body</p>',?5,?6,?6,?7,0,1,?8,'#rust',
                             ?9, '<p>body</p>', 'ada lovelace', ?10, '#rust')",
                )
                .bind(id.micros())
                .bind(other_user)
                .bind(format!("bob-{i}"))
                .bind(&title)
                .bind(format!("https://other.test/{i}"))
                .bind(NOW - 86_400 * 60 + i * 3_600)
                .bind(vec![8u8; 16])
                .bind(bob_feed.id)
                .bind(freshrss_search::fold(&title))
                .bind(format!("https://other.test/{i}"))
                .execute(&mut *conn)
                .await?;
            }

            Ok::<_, freshrss_server::Error>((id_user, other_user, feeds, categories, labels))
        })
        .await
        .expect("seed corpus");

    Corpus {
        db,
        id_user,
        other_user,
        feeds,
        categories,
        labels,
    }
}

fn base(c: &Corpus) -> EntryQuery {
    EntryQuery {
        id_user: c.id_user,
        id_max: Some(EntryId::from_unix(NOW)),
        limit: PAGE,
        ..EntryQuery::default()
    }
}

/// Reads the boundary values a cursor needs for the last row of a page, with hand-written SQL.
async fn boundary(c: &Corpus, q: &EntryQuery, id: EntryId) -> Cursor {
    // The id sort *is* the keyset, so its cursor carries no boundary values at all.
    if q.sort == Sort::Id {
        return Cursor {
            id,
            values: Vec::new(),
        };
    }
    let primary = match q.sort {
        Sort::Id => unreachable!("handled above"),
        Sort::Rand => unreachable!("a random sort cannot be resumed"),
        Sort::Date => "e.date",
        Sort::Title => "e.title",
        Sort::Link => "e.link",
        Sort::Length => "LENGTH(e.content)",
        Sort::LastModified => "e.lastModified",
        Sort::LastUserModified => "e.lastUserModified",
        Sort::FeedName => "f.name",
        Sort::CategoryName => "c.name",
    };
    let secondary = match q.secondary_sort {
        SecondarySort::Id => None,
        SecondarySort::Date => Some("e.date"),
        SecondarySort::Link => Some("e.link"),
        SecondarySort::Title => Some("e.title"),
    };
    // The category sort has an internal secondary on the feed name ahead of the reader's own.
    let middle = (q.sort == Sort::CategoryName).then_some("f.name");

    let mut selects = vec![primary.to_owned()];
    selects.extend(middle.map(str::to_owned));
    selects.extend(secondary.map(str::to_owned));

    let statement = format!(
        "SELECT {} FROM entry e
           INNER JOIN feed f ON f.id = e.id_feed
           LEFT JOIN category c ON c.id = f.category
          WHERE e.id = ?1",
        selects.join(", ")
    );

    let values =
        c.db.with(|mut conn| async move {
            let row = sqlx::query(sql(statement))
                .bind(id.micros())
                .fetch_one(&mut *conn)
                .await?;
            (0..selects.len())
                .map(|i| {
                    // Numeric expressions come back as integers and everything else as text, which
                    // is the distinction `CursorValue` exists to preserve. The type is read from the
                    // value rather than assumed, so a column that changed affinity is a loud failure.
                    let raw = row.try_get_raw(i)?;
                    let info = raw.type_info();
                    Ok(match info.name() {
                        "INTEGER" => CursorValue::Int(row.try_get(i)?),
                        "TEXT" => CursorValue::Text(row.try_get(i)?),
                        "NULL" => CursorValue::Text(String::new()),
                        found => unreachable!(
                            "no sort column in this corpus is a real or a blob, got {found}"
                        ),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .expect("boundary");

    Cursor { id, values }
}

/// Pages all the way through and returns every id in the order the pages arrived.
async fn page_through(c: &Corpus, mut q: EntryQuery) -> Vec<EntryId> {
    let mut seen = Vec::new();
    q.cursor = None;
    // One more iteration than could possibly be needed, so a cursor that stops making progress fails
    // the test instead of hanging.
    for _ in 0..(ARTICLES as usize / PAGE + 8) {
        let query = &q;
        let page =
            c.db.with(|mut conn| async move { list(&mut conn, query).await })
                .await
                .expect("lists");
        if page.is_empty() {
            return seen;
        }
        assert!(
            page.len() <= q.limit,
            "asked for {} rows and got {}",
            q.limit,
            page.len()
        );
        let last = page.last().expect("non-empty").id;
        seen.extend(page.iter().map(|e| e.id));
        if page.len() < q.limit {
            return seen;
        }
        q.cursor = Some(boundary(c, &q, last).await);
    }
    panic!("paging did not terminate after {} ids", seen.len());
}

/// Every article in the scope, in one query, for comparison.
async fn whole_scope(c: &Corpus, q: &EntryQuery) -> Vec<EntryId> {
    let mut all = q.clone();
    all.limit = ARTICLES as usize * 2;
    all.cursor = None;
    c.db.with(|mut conn| async move { list(&mut conn, &all).await })
        .await
        .expect("lists")
        .into_iter()
        .map(|e| e.id)
        .collect()
}

/// One listing, unwrapped.
async fn list_page(c: &Corpus, q: &EntryQuery) -> Vec<Entry> {
    c.db.with(|mut conn| async move { list(&mut conn, q).await })
        .await
        .expect("lists")
}

/// One listing that the builder is expected to reject before it ever reaches the database.
async fn list_err(c: &Corpus, q: &EntryQuery, expected: &str) -> Error {
    match c
        .db
        .with(|mut conn| async move { Ok(list(&mut conn, q).await) })
        .await
        .expect("the connection is borrowed whether or not the query is rejected")
    {
        Ok(rows) => panic!("{expected}: not rejected, it returned {} rows", rows.len()),
        Err(err) => err,
    }
}

/// One unread count, unwrapped.
async fn unread_count(c: &Corpus, q: &EntryQuery) -> i64 {
    c.db.with(|mut conn| async move { count_unread(&mut conn, q).await })
        .await
        .expect("counts")
}

/// One `COUNT(*)` from hand-written SQL, so a listing is never checked against its own query.
async fn count(c: &Corpus, statement: &str, params: Vec<i64>) -> i64 {
    let owned = statement.to_owned();
    c.db.with(|mut conn| async move {
        let mut query = sqlx::query_scalar(sql(owned));
        for value in params {
            query = query.bind(value);
        }
        let total: i64 = query.fetch_one(&mut *conn).await?;
        Ok(total)
    })
    .await
    .expect("count")
}

/// How many articles one feed holds.
async fn articles_in_feed(c: &Corpus, feed: i64) -> i64 {
    count(
        c,
        "SELECT COUNT(*) FROM entry WHERE id_feed = ?1",
        vec![feed],
    )
    .await
}

/// The sort/order combinations worth paging through: every shape of ladder the builder can emit.
fn paged_sorts() -> Vec<(Sort, Order, SecondarySort, Order)> {
    vec![
        (Sort::Id, Order::Desc, SecondarySort::Id, Order::Desc),
        (Sort::Id, Order::Asc, SecondarySort::Id, Order::Desc),
        (Sort::Date, Order::Desc, SecondarySort::Id, Order::Desc),
        (Sort::Date, Order::Asc, SecondarySort::Id, Order::Desc),
        (Sort::Title, Order::Desc, SecondarySort::Id, Order::Desc),
        (Sort::Title, Order::Asc, SecondarySort::Id, Order::Desc),
        (Sort::Link, Order::Desc, SecondarySort::Id, Order::Desc),
        (Sort::Length, Order::Desc, SecondarySort::Id, Order::Desc),
        (
            Sort::LastModified,
            Order::Desc,
            SecondarySort::Id,
            Order::Desc,
        ),
        (
            Sort::LastModified,
            Order::Asc,
            SecondarySort::Id,
            Order::Desc,
        ),
        // The name sorts carry a secondary ladder, so they get both an id secondary (which the builder
        // folds into the final rung) and a real one.
        (Sort::FeedName, Order::Desc, SecondarySort::Id, Order::Desc),
        (
            Sort::FeedName,
            Order::Asc,
            SecondarySort::Title,
            Order::Desc,
        ),
        (Sort::FeedName, Order::Desc, SecondarySort::Date, Order::Asc),
        (
            Sort::CategoryName,
            Order::Desc,
            SecondarySort::Id,
            Order::Desc,
        ),
        (
            Sort::CategoryName,
            Order::Asc,
            SecondarySort::Title,
            Order::Asc,
        ),
        (
            Sort::CategoryName,
            Order::Desc,
            SecondarySort::Link,
            Order::Desc,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paging_neither_repeats_nor_skips() {
    let c = corpus().await;
    for (sort, order, secondary, secondary_order) in paged_sorts() {
        let q = EntryQuery {
            sort,
            order,
            secondary_sort: secondary,
            secondary_order,
            ..base(&c)
        };
        let label = format!("{sort:?}/{order:?}+{secondary:?}/{secondary_order:?}");

        let paged = page_through(&c, q.clone()).await;
        let expected = whole_scope(&c, &q).await;

        assert_eq!(
            paged.len(),
            expected.len(),
            "{label}: paged {} rows, one query returned {}",
            paged.len(),
            expected.len()
        );

        let mut sorted = paged.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            paged.len(),
            "{label}: {} ids appeared on more than one page",
            paged.len() - sorted.len()
        );

        assert_eq!(
            paged, expected,
            "{label}: the pages arrived in a different order than one query produces"
        );
    }
}

#[tokio::test]
async fn paging_respects_the_state_filter_on_every_page() {
    let c = corpus().await;
    let q = EntryQuery {
        state: EntryState::unread(),
        sort: Sort::Date,
        ..base(&c)
    };
    let paged = page_through(&c, q.clone()).await;
    assert_eq!(paged, whole_scope(&c, &q).await);

    // The default scope is the main stream, which excludes the hidden feed, so the comparison has to
    // exclude it too.
    let unread = count(
        &c,
        "SELECT COUNT(*) FROM entry e
           INNER JOIN feed f ON f.id = e.id_feed
          WHERE e.id_user = ?1 AND e.is_read = 0 AND e.id <= ?2 AND f.priority >= 10",
        vec![c.id_user, EntryId::from_unix(NOW).micros()],
    )
    .await;
    assert_eq!(paged.len() as i64, unread, "every unread article, once");
    assert!(
        unread > 0 && unread < ARTICLES,
        "the corpus splits on read state"
    );
}

#[tokio::test]
async fn an_article_arriving_mid_scroll_does_not_shift_the_pages() {
    let c = corpus().await;
    let mut q = EntryQuery {
        sort: Sort::Date,
        ..base(&c)
    };

    let first = list_page(&c, &q).await;
    assert_eq!(first.len(), PAGE);
    q.cursor = Some(boundary(&c, &q, first.last().expect("non-empty").id).await);

    // A refresh lands between two pages. Without the `id_max` snapshot fence the new article would be
    // the newest in the scope and every following page would shift by one, repeating a row.
    let newcomer = EntryId::from_unix(NOW + 60);
    let (id_user, feed, newcomer_micros) = (c.id_user, c.feeds[0], newcomer.micros());
    c.db
        .with(|mut conn| async move {
            sqlx::query(
                "INSERT INTO entry (id, id_user, guid, title, author, content, link, date, lastSeen,
                                     hash, is_read, is_favorite, id_feed, tags)
                 VALUES (?1,?2,'late','Late arrival','','<p>x</p>','https://example.com/late',?3,?3,?4,0,0,?5,'')",
            )
            .bind(newcomer_micros)
            .bind(id_user)
            .bind(NOW + 60)
            .bind(vec![9u8; 16])
            .bind(feed)
            .execute(&mut *conn)
            .await?;
            Ok(())
        })
        .await
        .expect("insert");

    let second = list_page(&c, &q).await;
    assert!(
        !second.iter().any(|e| e.id == newcomer),
        "the snapshot fence should have excluded the article that arrived mid-scroll"
    );
    assert!(
        !first.iter().any(|f| second.iter().any(|s| s.id == f.id)),
        "no article appeared on both pages"
    );

    // Raising the fence is how the reader chooses to pick up new articles.
    q.id_max = Some(newer_id(newcomer));
    q.cursor = None;
    let with_new = list_page(&c, &q).await;
    assert_eq!(with_new.first().expect("non-empty").id, newcomer);
}

fn newer_id(id: EntryId) -> EntryId {
    EntryId::from_micros(id.micros() + 1)
}

#[tokio::test]
async fn a_random_sort_can_be_listed_but_not_resumed() {
    let c = corpus().await;
    let q = EntryQuery {
        sort: Sort::Rand,
        ..base(&c)
    };
    let page = list_page(&c, &q).await;
    assert_eq!(page.len(), PAGE);

    let resumed = EntryQuery {
        cursor: Some(Cursor {
            id: page[0].id,
            values: Vec::new(),
        }),
        ..q
    };
    let e = list_err(&c, &resumed, "a random sort has nothing to resume from").await;
    assert!(e.to_string().contains("cannot be resumed"), "{e}");
}

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_scope_admits_exactly_its_articles() {
    let c = corpus().await;
    let hidden_feed = c.feeds[3];
    let in_hidden = articles_in_feed(&c, hidden_feed).await;
    let in_important = articles_in_feed(&c, c.feeds[2]).await;
    let in_second_feed = articles_in_feed(&c, c.feeds[1]).await;

    // The main stream excludes the hidden feed.
    let main = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::MainStream,
            ..base(&c)
        },
    )
    .await;
    assert_eq!(main.len() as i64, ARTICLES - in_hidden);

    // Everything-but-hidden does too, but a search naming the hidden feed lifts it back in.
    let expr = parse(&format!("f:{hidden_feed}"), NOW).unwrap();
    let lifted = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::AllButHidden,
            search: Some(expr.clone()),
            ..base(&c)
        },
    )
    .await;
    assert_eq!(lifted.len() as i64, in_hidden);

    let everything = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::IncludingHidden,
            ..base(&c)
        },
    )
    .await;
    assert_eq!(everything.len() as i64, ARTICLES);

    let important = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::Important,
            ..base(&c)
        },
    )
    .await;
    assert_eq!(important.len() as i64, in_important);

    let feed = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::Feed,
            scope_id: hidden_feed,
            ..base(&c)
        },
    )
    .await;
    assert_eq!(
        feed.len() as i64,
        in_hidden,
        "a feed scope reaches its own feed whatever its priority"
    );

    let category = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::Category,
            scope_id: c.categories[1],
            ..base(&c)
        },
    )
    .await;
    assert_eq!(
        category.len() as i64,
        in_second_feed,
        "the hidden feed in this folder is below the folder's floor"
    );

    let starred = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::Starred,
            ..base(&c)
        },
    )
    .await;
    assert!(!starred.is_empty());
    let favourites = count(
        &c,
        "SELECT COUNT(*) FROM entry WHERE id_user = ?1 AND is_favorite = 1",
        vec![c.id_user],
    )
    .await;
    assert_eq!(starred.len() as i64, favourites, "starred ignores priority");
}

#[tokio::test]
async fn the_label_scopes_count_an_article_once() {
    let c = corpus().await;

    let tagged = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::AnyTag,
            ..base(&c)
        },
    )
    .await
    .len();
    let expected = count(
        &c,
        "SELECT COUNT(*) FROM entry e WHERE e.id_user = ?1
           AND EXISTS (SELECT 1 FROM entrytag et WHERE et.id_entry = e.id)",
        vec![c.id_user],
    )
    .await;
    assert_eq!(tagged, expected as usize);
    assert!(tagged > 0, "the corpus needs labelled articles");

    let one_label = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::Tag,
            scope_id: c.labels[0],
            ..base(&c)
        },
    )
    .await
    .len();
    assert!(one_label <= tagged, "{one_label} vs {tagged}");
    assert!(one_label > 0);

    let starred_or_tagged = whole_scope(
        &c,
        &EntryQuery {
            scope: ListType::StarredOrTagged,
            ..base(&c)
        },
    )
    .await
    .len();
    assert!(
        starred_or_tagged >= tagged,
        "starred-or-tagged is a superset of tagged: {starred_or_tagged} vs {tagged}"
    );
}

#[tokio::test]
async fn no_scope_reaches_another_users_articles() {
    let c = corpus().await;
    let scopes = [
        (ListType::IncludingHidden, 0),
        (ListType::MainStream, 0),
        (ListType::Starred, 0),
        (ListType::AnyTag, 0),
        (ListType::Feed, c.feeds[0]),
        (ListType::Category, c.categories[1]),
        (ListType::Tag, c.labels[0]),
    ];
    for (scope, id) in scopes {
        let q = EntryQuery {
            scope,
            scope_id: id,
            limit: ARTICLES as usize * 4,
            ..base(&c)
        };
        let rows = list_page(&c, &q).await;
        assert!(
            rows.iter().all(|e| e.id_user == c.id_user),
            "{scope:?} leaked another user's rows"
        );
        // Bob has 40 articles with Alice's titles; if the fence were missing the count would exceed
        // Alice's own corpus.
        assert!(
            rows.len() <= ARTICLES as usize,
            "{scope:?} returned {} rows, more than the {} this user has",
            rows.len(),
            ARTICLES
        );
        assert_eq!(rows.iter().filter(|e| e.id_user == c.other_user).count(), 0);
    }
}

// ---------------------------------------------------------------------------
// Search and counts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_search_narrows_the_listing() {
    let c = corpus().await;
    let expr = parse("intitle:003 author:bot", NOW).unwrap();
    let q = EntryQuery {
        search: Some(expr.clone()),
        limit: ARTICLES as usize * 2,
        ..base(&c)
    };
    let rows = list_page(&c, &q).await;
    assert!(!rows.is_empty(), "the corpus should contain a match");
    assert!(rows.iter().all(|e| e.title.contains("003")), "{rows:?}");
    assert!(
        rows.iter().all(|e| e.authors.iter().any(|a| a == "bot")),
        "{rows:?}"
    );
}

#[tokio::test]
async fn the_unread_count_agrees_with_the_listing_it_describes() {
    let c = corpus().await;
    let expr = parse("#rust", NOW).unwrap();
    let queries = vec![
        base(&c),
        EntryQuery {
            scope: ListType::Starred,
            ..base(&c)
        },
        EntryQuery {
            scope: ListType::Feed,
            scope_id: c.feeds[1],
            ..base(&c)
        },
        EntryQuery {
            scope: ListType::AnyTag,
            ..base(&c)
        },
        EntryQuery {
            search: Some(expr.clone()),
            ..base(&c)
        },
        // A nullable sort excludes the rows it cannot order, so the count has to as well or the header
        // badge would disagree with the list.
        EntryQuery {
            sort: Sort::LastModified,
            ..base(&c)
        },
    ];

    for q in &queries {
        let counted = unread_count(&c, q).await;
        let mut all = q.clone();
        all.state = EntryState::unread();
        all.limit = ARTICLES as usize * 2;
        all.cursor = None;
        let listed = list_page(&c, &all).await.len() as i64;
        assert_eq!(counted, listed, "the badge and the list disagree for {q:?}");
        assert!(counted >= 0);
    }
}

#[tokio::test]
async fn the_unread_count_ignores_the_cursor_but_keeps_the_fence() {
    let c = corpus().await;
    let first = base(&c);
    let whole = unread_count(&c, &first).await;

    let after_a_page = EntryQuery {
        cursor: Some(Cursor {
            id: EntryId::from_unix(NOW - 86_400 * 20),
            values: Vec::new(),
        }),
        ..base(&c)
    };
    let counted = unread_count(&c, &after_a_page).await;
    assert_eq!(
        counted, whole,
        "the count describes the scope, not the page after a cursor"
    );

    // Halfway through the corpus, which spans 240 articles at 900s each.
    let fenced = EntryQuery {
        id_max: Some(EntryId::from_unix(NOW - 86_400 * 30 + 900 * 120)),
        ..base(&c)
    };
    let counted = unread_count(&c, &fenced).await;
    assert!(
        counted < whole,
        "a narrower snapshot must count fewer: {counted} vs {whole}"
    );
}

#[tokio::test]
async fn a_nullable_sort_skips_the_rows_it_cannot_order() {
    let c = corpus().await;
    let q = EntryQuery {
        sort: Sort::LastModified,
        limit: ARTICLES as usize * 2,
        ..base(&c)
    };
    let rows = list_page(&c, &q).await;
    assert!(rows.iter().all(|e| e.last_modified.is_some()));
    assert!(
        rows.len() < ARTICLES as usize,
        "a fifth of the corpus has no lastModified"
    );
    // And paging through them still neither repeats nor skips.
    let paged = page_through(
        &c,
        EntryQuery {
            sort: Sort::LastModified,
            ..base(&c)
        },
    )
    .await;
    assert_eq!(paged.len(), rows.len());
}

#[tokio::test]
async fn the_rows_come_back_fully_populated() {
    // The late-row-lookup fetches ids first and the wide columns second; a column missing from the
    // outer select would read back as a default rather than as an error.
    let c = corpus().await;
    let q = EntryQuery {
        sort: Sort::Title,
        limit: 1,
        ..base(&c)
    };
    let rows = list_page(&c, &q).await;
    let row = rows.first().expect("one row");
    assert!(row.content.starts_with("<p>body "), "{}", row.content);
    assert!(!row.title.is_empty());
    assert!(!row.link.is_empty());
    assert!(!row.guid.is_empty());
    assert!(row.hash.is_some());
    assert!(!row.tags.is_empty());
    assert_eq!(row.id_user, c.id_user);

    // And it is the same row the direct lookup returns.
    let (id, id_user) = (row.id, c.id_user);
    let direct =
        c.db.with(|mut conn| async move { entry_dao::find_by_id(&mut conn, id, id_user).await })
            .await
            .expect("finds")
            .expect("exists");
    assert_eq!(direct, *row);
}

#[tokio::test]
async fn an_empty_page_is_not_an_error() {
    let c = corpus().await;
    let expr = parse("intitle:nothing-in-the-corpus-matches-this", NOW).unwrap();
    let q = EntryQuery {
        search: Some(expr.clone()),
        ..base(&c)
    };
    assert!(list_page(&c, &q).await.is_empty());
    assert_eq!(unread_count(&c, &q).await, 0);
}

#[tokio::test]
async fn a_rejected_query_never_reaches_the_database() {
    let c = corpus().await;
    for q in [
        EntryQuery {
            scope: ListType::Feed,
            scope_id: 0,
            ..base(&c)
        },
        EntryQuery {
            scope: ListType::MainStream,
            scope_id: 3,
            ..base(&c)
        },
        EntryQuery {
            sort: Sort::Rand,
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: Vec::new(),
            }),
            ..base(&c)
        },
    ] {
        let e = list_err(&c, &q, "rejected").await;
        assert!(
            e.to_string().contains("scope") || e.to_string().contains("resumed"),
            "{e}"
        );
    }
}
