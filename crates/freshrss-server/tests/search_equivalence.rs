//! The differential property test: for a random query, the SQL id-set equals the matcher id-set.
//!
//! This is the test that justifies having two backends at all. FreshRSS has the same split — SQL for
//! listings, an in-memory matcher for filter actions — and the two disagree: its SQL path uses
//! `LIKE`, which SQLite makes case-insensitive for ASCII only, while its matcher accent-folds through
//! `strilike`. So `intitle:ecole` matches "École" when deciding whether to mark an article read, and
//! does *not* match it when the user searches for the same thing. Nobody noticed because the two code
//! paths are never compared.
//!
//! Here they are compared, on every generated query. Both sides call `normalize::fold`, so agreement
//! is the expected outcome and a disagreement is a real bug rather than a known divergence.
//!
//! # Why the generator draws from a fixed vocabulary
//!
//! Random strings would miss almost every row and the assertion would pass vacuously. The needles
//! below all occur in the seeded corpus, so a generated query splits it both ways — which is what
//! makes a divergence show up as a non-empty symmetric difference. [`the_vocabulary_splits_the_corpus`]
//! asserts that property of the corpus directly, so the generator cannot silently rot into testing
//! nothing.

mod common;

use std::collections::BTreeSet;
use std::sync::LazyLock;

use freshrss_search::ast::{DateField, DateRange, Expr, IdSet, Pred, TextPat};
use freshrss_search::matcher::{matches, SearchTarget};
use freshrss_search::regex::Pattern;
use freshrss_search::sql::{compile, SqlCtx};
use freshrss_server::db::sql::{bind_all, sql, SqlValue};
use freshrss_server::db::Db;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::subsequence;
use sqlx::sqlite::SqliteRow;
use sqlx::Row as _;

use common::{insert_entry, SeededEntry};

/// The reference instant every generated date range is built around.
const NOW: i64 = 1_700_000_000;

const ROWS: usize = 120;

/// The user whose rows the test queries. A second user exists to prove a fragment cannot leak across
/// the `id_user` boundary.
const USER: &str = "alice";
const OTHER: &str = "bob";

/// Ingestion time (which *is* the entry id, in microseconds) and publication time deliberately cover
/// disjoint spans: `date:` reads one and `pubdate:` the other, so a field mix-up or a wrong
/// µs↔s scale moves the matched set instead of cancelling out.
const INGESTION_SPAN: i64 = 3_600;
const PUBLICATION_SPAN: i64 = 86_400;
const PUBLICATION_EPOCH: i64 = NOW - 86_400 * 60;

const TITLES: &[&str] = &[
    "A Rust article",
    "l'École des chiens",
    "École lowercase",
    "CAFÉ au lait",
    "rust and axum together",
    "only axum here",
    "",
    "trailing whitespace   ",
    "Ünïcödé title",
    "100% sure",
    "a_b underscore",
];

const CONTENTS: &[&str] = &[
    "<p>Body mentioning axum</p>",
    "<p>Nothing interesting</p>",
    "<p>Rust inside the body only</p>",
    "<p>École in the content</p>",
    "",
    "<p>100% and a_b</p>",
];

/// Authors, in the `"; "`-joined column form the domain writes.
const AUTHORS: &[&str] = &[
    "Ada Lovelace",
    "Ada Lovelace; Grace Hopper",
    "Grace Hopper",
    "",
    "Éditeur Français",
    "bot account",
];

const LINKS: &[&str] = &[
    "https://example.com/a-rust-article",
    "https://example.com/axum",
    "https://other.test/post/1",
    "",
    "https://example.com/école",
];

/// Tag columns, in the `"#a #b"` form.
const TAGS: &[&str] = &[
    "#rust #rss",
    "#axum",
    "#rust",
    "",
    "#étiquette",
    "#rust #axum #rss",
];

const LABELS: &[&str] = &["todo", "reading", "étiquette", "done"];

const TEXT_NEEDLES: &[&str] = &[
    "rust",
    "axum",
    "ecole",
    "école",
    "cafe",
    "café",
    "unicode",
    "ünïcödé",
    "100%",
    "a_b",
    "_",
    "body",
    "ada",
    "lovelace",
    "hopper",
    "bot",
    "example.com",
    "other.test",
    "the",
    "",
];

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// A row read back from the database, implementing [`SearchTarget`].
///
/// The matcher sees exactly the bytes SQLite stored rather than the structs used to seed them, so a
/// difference in how a value was written shows up as a divergence instead of being hidden by both
/// sides reading the same in-memory string.
#[derive(Debug, Clone)]
struct Row {
    id: i64,
    feed_id: i64,
    category_id: Option<i64>,
    title: String,
    content: String,
    author: String,
    link: String,
    tags: String,
    label_ids: Vec<i64>,
    label_names: Vec<String>,
    date: i64,
    last_modified: Option<i64>,
    last_user_modified: Option<i64>,
}

impl SearchTarget for Row {
    fn id(&self) -> Option<i64> {
        Some(self.id)
    }
    fn feed_id(&self) -> i64 {
        self.feed_id
    }
    fn category_id(&self) -> Option<i64> {
        self.category_id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn content(&self) -> &str {
        &self.content
    }
    fn author(&self) -> &str {
        &self.author
    }
    fn link(&self) -> &str {
        &self.link
    }
    fn tags(&self) -> &str {
        &self.tags
    }
    fn label_ids(&self) -> &[i64] {
        &self.label_ids
    }
    fn label_names(&self) -> &[String] {
        &self.label_names
    }
    fn date(&self) -> i64 {
        self.date
    }
    fn last_modified(&self) -> Option<i64> {
        self.last_modified
    }
    fn last_user_modified(&self) -> Option<i64> {
        self.last_user_modified
    }
}

struct Harness {
    db: Db,
    /// The runtime every database call in this file is driven on.
    ///
    /// The corpus is built once and shared by all 300 generated cases, and `proptest!` emits
    /// synchronous test bodies, so there is no ambient runtime to `.await` on. Declared after `db`
    /// so it outlives the pool that was opened on it.
    rt: tokio::runtime::Runtime,
    id_user: i64,
    rows: Vec<Row>,
    feed_ids: Vec<i64>,
    category_ids: Vec<i64>,
    label_ids: Vec<i64>,
}

/// Built once and shared: every case only reads, and rebuilding a 120-row database per case would
/// dominate the runtime without adding coverage.
static HARNESS: LazyLock<Harness> = LazyLock::new(build_harness);

impl Harness {
    fn entry_ids(&self) -> Vec<i64> {
        self.rows.iter().map(|r| r.id).collect()
    }

    /// The ids the SQL backend returns, restricted to one user the way every listing query is.
    fn sql_ids(&self, expr: &Expr) -> BTreeSet<i64> {
        let fragment = compile(expr, &SqlCtx::default()).expect("compiles");
        // The user fence is a bare `?` too, not `?1`: sqlx resolves an anonymous placeholder by
        // counting the anonymous ones it has seen, so numbering this one would hand it the
        // fragment's first parameter as well. The compiler emits bare `?` throughout.
        let statement = format!(
            "SELECT e.id FROM entry e WHERE e.id_user = ? AND ({}) ORDER BY e.id",
            fragment.sql
        );
        let mut params = vec![SqlValue::Int(self.id_user)];
        params.extend(fragment.params.iter().map(SqlValue::from));

        self.rt
            .block_on(self.db.with(|mut conn| async move {
                Ok(bind_all(sqlx::query(sql(statement)), params)
                    .try_map(|row: SqliteRow| row.try_get::<i64, _>(0))
                    .fetch_all(&mut *conn)
                    .await?)
            }))
            .expect("query runs")
            .into_iter()
            .collect()
    }

    fn matcher_ids(&self, expr: &Expr) -> BTreeSet<i64> {
        self.rows
            .iter()
            .filter(|row| matches(expr, *row).expect("evaluates"))
            .map(|row| row.id)
            .collect()
    }

    fn assert_agree(&self, expr: &Expr) {
        self.assert_agree_because(expr, "a generated query")
    }

    /// [`Self::assert_agree`] with a human name for the case, so a failure in the hand-written list
    /// says which operator broke rather than only dumping a tree.
    fn assert_agree_because(&self, expr: &Expr, label: &str) {
        let sql = self.sql_ids(expr);
        let matcher = self.matcher_ids(expr);
        if sql == matcher {
            return;
        }
        let describe = |ids: &BTreeSet<i64>, other: &BTreeSet<i64>| -> Vec<String> {
            ids.difference(other)
                .map(|id| {
                    self.rows
                        .iter()
                        .find(|r| &r.id == id)
                        .map(|r| {
                            format!(
                                "#{id} title={:?} author={:?} tags={:?} labels={:?} date={}",
                                r.title, r.author, r.tags, r.label_names, r.date
                            )
                        })
                        .unwrap_or_else(|| format!("#{id} (not in this user's corpus)"))
                })
                .collect()
        };
        panic!(
            "the backends disagree on {label}: {expr:?}\n\
             \n  compiled sql: {}\n\
             \n  sql matched {} rows, matcher matched {}\n\
             \n  only in sql:     {:#?}\n\
             \n  only in matcher: {:#?}",
            compile(expr, &SqlCtx::default())
                .map(|f| f.sql)
                .unwrap_or_default(),
            sql.len(),
            matcher.len(),
            describe(&sql, &matcher),
            describe(&matcher, &sql),
        );
    }
}

/// Seeds the corpus. Deterministic, so a failing case reproduces exactly.
///
/// Rows go straight into `entry` with ids this test chooses rather than through
/// `entry_dao::commit_staged`, because the commit path allocates ids from the global sequence — and
/// the id *is* the `date:` operand, so the ranges below would be unpredictable. The staging pipeline
/// has its own coverage in `tests/dao.rs`.
fn build_harness() -> Harness {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let (db, id_user, feed_ids, category_ids, label_ids, rows) =
        rt.block_on(seed()).expect("seed corpus");

    Harness {
        db,
        rt,
        id_user,
        rows,
        feed_ids,
        category_ids,
        label_ids,
    }
}

/// Builds the database and its 120-row corpus, on the harness's own runtime.
async fn seed() -> freshrss_server::Result<(Db, i64, Vec<i64>, Vec<i64>, Vec<i64>, Vec<Row>)> {
    use freshrss_server::db::dao::{category, feed, tag, user};
    use freshrss_server::domain::attributes::Attributes;
    use freshrss_server::domain::category::CategoryKind;
    use freshrss_server::domain::feed::Feed;

    let db = common::db().await;

    let (id_user, feed_ids, category_ids, label_ids, rows) = db
        .with(|mut conn| async move {
            let id_user = user::insert(
                &mut conn,
                &user::NewUser {
                    username: USER,
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await?;
            let other_user = user::insert(
                &mut conn,
                &user::NewUser {
                    username: OTHER,
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await?;

            let mut category_ids = vec![user::default_category_id(&mut conn, id_user)
                .await?
                .expect("seeded")];
            for (i, name) in ["Technology", "Etiquettes"].into_iter().enumerate() {
                category_ids.push(
                    category::insert(
                        &mut conn,
                        &category::NewCategory {
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

            let mut feed_ids = Vec::new();
            // An entry has no category column of its own: `c:` resolves through the feed. The matcher
            // is handed the category by its caller, so the seeded rows have to carry the *feed's*
            // category or the two backends would be answering different questions.
            let mut feed_categories = Vec::new();
            for i in 0..4 {
                let mut f = Feed::new(
                    id_user,
                    format!("https://example.com/feed{i}.xml"),
                    format!("Feed {i}"),
                );
                f.id = feed::insert(&mut conn, &f).await?;
                // The last feed is deliberately left without a folder, so the "no category" branch of
                // `c:` is exercised against a real NULL rather than only against a missing id.
                let category = (i < 3).then_some(category_ids[i % 3]);
                feed::update_category(&mut conn, f.id, id_user, category).await?;
                feed_ids.push(f.id);
                feed_categories.push(category);
            }

            // A second user's feed carrying the same vocabulary: any fragment that forgot its
            // `id_user` predicate would pick these rows up and the isolation test would fail.
            let mut foreign = Feed::new(other_user, "https://other.test/feed.xml", "Foreign");
            foreign.id = feed::insert(&mut conn, &foreign).await?;
            let foreign_category = user::default_category_id(&mut conn, other_user)
                .await?
                .expect("seeded");
            feed::update_category(&mut conn, foreign.id, other_user, Some(foreign_category))
                .await?;

            let mut label_ids = Vec::new();
            for name in LABELS {
                label_ids.push(tag::insert(&mut conn, id_user, name).await?);
            }

            let mut rows = Vec::new();
            for idx in 0..ROWS {
                let id = NOW * 1_000_000 + (idx as i64) * INGESTION_SPAN * 1_000_000;
                let date = PUBLICATION_EPOCH + (idx as i64) * PUBLICATION_SPAN;
                // Some rows were never edited upstream or by the user, so the NULL branches of
                // `mdate:` and `userdate:` are exercised.
                let last_modified = (idx % 3 != 0).then_some(date + 5 * 3_600);
                let last_user_modified = (idx % 4 != 0).then_some(date + 9 * 3_600);
                let title = TITLES[idx % TITLES.len()];
                let content = CONTENTS[(idx / 3) % CONTENTS.len()];
                let author = AUTHORS[(idx / 2) % AUTHORS.len()];
                let link = LINKS[(idx / 5) % LINKS.len()];
                let tags = TAGS[(idx / 7) % TAGS.len()];
                let feed_id = feed_ids[idx % feed_ids.len()];
                let category_id = feed_categories[idx % feed_ids.len()];

                // The folded columns are what the SQL backend compares against, so a harness that
                // skipped them would be comparing the matcher against empty strings;
                // `insert_entry` derives them from the plain text below.
                insert_entry(
                    &mut conn,
                    &SeededEntry {
                        id,
                        id_user,
                        guid: format!("guid-{idx}"),
                        title: title.to_owned(),
                        author: author.to_owned(),
                        content: content.to_owned(),
                        link: link.to_owned(),
                        date,
                        last_modified,
                        last_user_modified,
                        hash: vec![7u8; 16],
                        is_read: idx % 5 == 0,
                        is_favorite: idx % 11 == 0,
                        id_feed: feed_id,
                        tags: tags.to_owned(),
                    },
                )
                .await?;

                // Zero to four labels per row, with a fifth of the corpus carrying none at all so
                // that `l:*` and `NOT l:*` both split it.
                let mut attached_ids = Vec::new();
                let mut attached_names = Vec::new();
                for li in 0..(idx / 6) % 5 {
                    let label = (li + idx) % LABELS.len();
                    tag::tag_entry(&mut conn, label_ids[label], id, true).await?;
                    attached_ids.push(label_ids[label]);
                    attached_names.push(LABELS[label].to_owned());
                }

                rows.push(Row {
                    id,
                    feed_id,
                    category_id,
                    title: title.to_owned(),
                    content: content.to_owned(),
                    author: author.to_owned(),
                    link: link.to_owned(),
                    tags: tags.to_owned(),
                    label_ids: attached_ids,
                    label_names: attached_names,
                    date,
                    last_modified,
                    last_user_modified,
                });
            }

            // An entry has no category column: SQL resolves `c:` through `feed`, while the matcher is
            // handed the category by whoever implements `SearchTarget`. Seeding those two
            // independently would make the backends answer different questions and the property would
            // fail for a reason that has nothing to do with search.
            for row in &rows {
                let slot = feed_ids
                    .iter()
                    .position(|f| *f == row.feed_id)
                    .expect("feed");
                assert_eq!(
                    feed_categories[slot], row.category_id,
                    "entry {} disagrees with feed {} about its folder",
                    row.id, row.feed_id
                );
            }

            // The other user gets rows whose titles also appear in Alice's corpus, and ids that
            // interleave with hers.
            for (i, title) in TITLES.iter().take(6).enumerate() {
                sqlx::query(
                    "INSERT INTO entry
                       (id, id_user, guid, title, author, content, link, date, lastSeen,
                        hash, is_read, is_favorite, id_feed, tags,
                        title_folded, content_folded, author_folded, link_folded, tags_folded)
                     VALUES (?1,?2,?3,?4,'Ada Lovelace',' ',?5,?6,?6,?7,0,0,?8,'#rust #todo',
                             ?9, '', 'ada lovelace', ?10, '#rust #todo')",
                )
                .bind(NOW * 1_000_000 + (i as i64) * INGESTION_SPAN * 1_000_000 + 1)
                .bind(other_user)
                .bind(format!("foreign-{i}"))
                .bind(*title)
                .bind(format!("https://other.test/{i}"))
                .bind(PUBLICATION_EPOCH + (i as i64) * PUBLICATION_SPAN)
                .bind(vec![8u8; 16])
                .bind(foreign.id)
                .bind(freshrss_search::fold(title))
                .bind(format!("https://other.test/{i}"))
                .execute(&mut *conn)
                .await?;
            }

            Ok::<_, freshrss_server::Error>((id_user, feed_ids, category_ids, label_ids, rows))
        })
        .await?;

    Ok((db, id_user, feed_ids, category_ids, label_ids, rows))
}

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

/// A text pattern drawn from the corpus.
///
/// Containment only: the SQL backend refuses regexes outright (SQLite has no regex operator without a
/// user-defined function), so a generated regex would turn every case into an error assertion. The
/// regex half of the matcher is covered by `a_regex_is_refused_by_sql_but_answered_by_the_matcher`
/// below and by the matcher's own unit tests.
fn text_pat() -> BoxedStrategy<TextPat> {
    prop::sample::select(TEXT_NEEDLES.to_vec())
        .prop_map(TextPat::contains)
        .boxed()
}

fn date_field() -> BoxedStrategy<DateField> {
    prop_oneof![
        // Weighted toward `date:` because it is the one field whose operand is scaled, and therefore
        // the one where the two backends can silently disagree about units.
        4 => Just(DateField::Ingestion),
        3 => Just(DateField::Publication),
        2 => Just(DateField::Modified),
        1 => Just(DateField::UserModified),
    ]
    .boxed()
}

/// Bounds drawn from a window that covers both the ingestion and the publication span, so every
/// field's range sometimes matches rows and sometimes does not.
fn date_bound() -> impl Strategy<Value = i64> {
    (NOW - 86_400 * 400)..(NOW + 86_400 * 400)
}

fn date_range() -> BoxedStrategy<DateRange> {
    prop_oneof![
        4 => (date_bound(), date_bound()).prop_map(|(a, b)| DateRange {
            min: Some(a.min(b)),
            max: Some(a.max(b)),
        }),
        2 => date_bound().prop_map(|v| DateRange { min: Some(v), max: None }),
        2 => date_bound().prop_map(|v| DateRange { min: None, max: Some(v) }),
        // An unbounded range matches everything on both sides; including it keeps the trivial case
        // honest rather than special-cased.
        1 => Just(DateRange::default()),
        // The complemented form the parser produces for `-date:…`, which the SQL side complements
        // and the matcher side `xor`s.
        1 => (date_bound(), date_bound()).prop_map(|(a, b)| DateRange {
            min: Some(a.max(b) + 1),
            max: Some(a.min(b) - 1),
        }),
    ]
    .boxed()
}

/// A subset of `ids` of arbitrary length, the empty one included.
///
/// `prop::sample::subsequence` wants a fixed size, and size is precisely what has to vary: an empty
/// list means "match nothing" while [`IdSet::Any`] means "has at least one", and the two backends
/// implement that distinction independently.
fn subset(ids: Vec<i64>) -> BoxedStrategy<Vec<i64>> {
    let len = ids.len();
    (0..=len)
        .prop_flat_map(move |size| subsequence(ids.clone(), size))
        .boxed()
}

fn id_set(ids: Vec<i64>) -> BoxedStrategy<IdSet> {
    prop_oneof![
        1 => Just(IdSet::Any),
        4 => subset(ids).prop_map(IdSet::List),
    ]
    .boxed()
}

/// One leaf predicate, weighted toward the operators whose two implementations are most likely to
/// drift: text folding, whole-tag matching, label lookup by name versus by id, and the id-scaled
/// `date:` operand.
fn leaf_pred(h: &'static Harness) -> BoxedStrategy<Pred> {
    prop_oneof![
        6 => (text_pat(), any::<bool>()).prop_map(|(pat, negated)| Pred::FreeText { pat, negated }),
        4 => (text_pat(), any::<bool>()).prop_map(|(pat, negated)| Pred::Title { pat, negated }),
        3 => (text_pat(), any::<bool>()).prop_map(|(pat, negated)| Pred::Content { pat, negated }),
        3 => (text_pat(), any::<bool>()).prop_map(|(pat, negated)| Pred::Author { pat, negated }),
        2 => (text_pat(), any::<bool>()).prop_map(|(pat, negated)| Pred::Url { pat, negated }),
        // Tags: the whole-tag rule is implemented two completely different ways on each side —
        // SQL pads the column and matches `"%needle #%"`, the matcher splits and compares.
        4 => (text_pat(), any::<bool>()).prop_map(|(pat, negated)| Pred::Tag { pat, negated }),
        3 => (
            vec(text_pat(), 1..3),
            any::<bool>(),
        ).prop_map(|(names, negated)| Pred::LabelNames { names, negated }),
        3 => (id_set(h.label_ids.clone()), any::<bool>())
            .prop_map(|(ids, negated)| Pred::LabelIds { ids, negated }),
        3 => (subset(h.feed_ids.clone()), any::<bool>())
            .prop_map(|(ids, negated)| Pred::FeedIds { ids, negated }),
        2 => (subset(h.category_ids.clone()), any::<bool>())
            .prop_map(|(ids, negated)| Pred::CategoryIds { ids, negated }),
        2 => (subset(h.entry_ids()), any::<bool>())
            .prop_map(|(ids, negated)| Pred::EntryIds { ids, negated }),
        4 => (date_field(), date_range(), any::<bool>())
            .prop_map(|(field, range, negated)| Pred::Date { field, range, negated }),
    ]
    .boxed()
}

/// A whole expression: leaves combined with And/Or/Not down to a bounded depth.
///
/// The depth cap keeps generated SQL small enough that a failure message is readable, which matters
/// more here than coverage — a 200-term query that fails tells you nothing. Proptest's shrinking
/// then reduces a failing tree to the couple of nodes that actually diverge.
fn expr(h: &'static Harness, depth: usize) -> BoxedStrategy<Expr> {
    let leaf = leaf_pred(h).prop_map(|p| Expr::Leaf(vec![p]));
    // Two or three predicates in one leaf, which is the common real shape and exercises the
    // implicit-AND path separately from the `Node` path.
    let wide_leaf = vec(leaf_pred(h), 2..4).prop_map(Expr::Leaf);
    if depth == 0 {
        return prop_oneof![3 => leaf, 1 => wide_leaf].boxed();
    }
    let and = vec(expr(h, depth - 1), 2..4).prop_map(Expr::and);
    let or = vec(expr(h, depth - 1), 2..4).prop_map(Expr::or);
    let not = expr(h, depth - 1).prop_map(Expr::negate);
    prop_oneof![
        3 => leaf,
        2 => wide_leaf,
        2 => and,
        2 => or,
        1 => not,
    ]
    .boxed()
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// For a random query, the SQL backend and the in-memory matcher return the same set of ids.
    #[test]
    fn sql_and_matcher_agree(expr in expr(&HARNESS, 2)) {
        HARNESS.assert_agree(&expr);
    }
}

// ---------------------------------------------------------------------------
// Guards around the property
// ---------------------------------------------------------------------------

/// The corpus is discriminating: these queries each match *some* rows and not *all* of them.
///
/// Without this the property could pass 300 times while both backends matched nothing, which is the
/// failure mode a differential test has to defend itself against.
#[test]
fn the_vocabulary_splits_the_corpus() {
    let h = &HARNESS;
    let total = h.rows.len();
    assert!(total >= 100, "corpus should be large enough to split");

    for expr in [
        Expr::pred(Pred::Title {
            pat: TextPat::contains("rust"),
            negated: false,
        }),
        Expr::pred(Pred::Content {
            pat: TextPat::contains("ecole"),
            negated: false,
        }),
        Expr::pred(Pred::Author {
            pat: TextPat::contains("ada"),
            negated: false,
        }),
        Expr::pred(Pred::Tag {
            pat: TextPat::contains("rust"),
            negated: false,
        }),
        Expr::pred(Pred::LabelNames {
            names: vec![TextPat::contains("todo")],
            negated: false,
        }),
        Expr::pred(Pred::LabelIds {
            ids: IdSet::Any,
            negated: false,
        }),
        Expr::pred(Pred::Url {
            pat: TextPat::contains("example.com"),
            negated: false,
        }),
        Expr::pred(Pred::Date {
            field: DateField::Ingestion,
            range: DateRange {
                min: Some(NOW),
                max: Some(NOW + 3_600 * 10),
            },
            negated: false,
        }),
        Expr::pred(Pred::Date {
            field: DateField::Publication,
            range: DateRange {
                min: Some(PUBLICATION_EPOCH),
                max: Some(PUBLICATION_EPOCH + 86_400 * 10),
            },
            negated: false,
        }),
    ] {
        let ids = h.sql_ids(&expr);
        assert!(
            !ids.is_empty() && ids.len() < total,
            "{expr:?} matched {} of {total} rows — the corpus does not split on it",
            ids.len()
        );
        h.assert_agree(&expr);
    }
}

/// A regex is refused by the SQL backend and answered by the matcher.
///
/// This is the one place the two backends now disagree on purpose: SQLite has no regex operator
/// without a user-defined function and the driver cannot register one, so a listing says "no, and
/// here is why" while a filter action — which runs before an article has an id, over the parsed feed —
/// still evaluates the pattern. The divergence is a loud error against an answer, never two different
/// answers, which is the shape it has to keep.
#[test]
fn a_regex_is_refused_by_sql_but_answered_by_the_matcher() {
    let h = &HARNESS;

    let cases: &[(&str, Expr)] = &[
        (
            "anchored at the start",
            Expr::pred(Pred::Title {
                pat: TextPat::Regex(Pattern::parse("/^rust/i").unwrap()),
                negated: false,
            }),
        ),
        (
            "case-insensitive over an accent",
            Expr::pred(Pred::Title {
                pat: TextPat::Regex(Pattern::parse("/CAFÉ/i").unwrap()),
                negated: false,
            }),
        ),
        (
            "negated",
            Expr::pred(Pred::Title {
                pat: TextPat::Regex(Pattern::parse("/ru.t/").unwrap()),
                negated: true,
            }),
        ),
        (
            "over the body",
            Expr::pred(Pred::Content {
                pat: TextPat::Regex(Pattern::parse("/axum/").unwrap()),
                negated: false,
            }),
        ),
        (
            "over tags, per tag",
            Expr::pred(Pred::Tag {
                pat: TextPat::Regex(Pattern::parse("/^rus/").unwrap()),
                negated: false,
            }),
        ),
    ];

    for (name, expr) in cases {
        let err = compile(expr, &SqlCtx::default()).expect_err("SQL refuses regexes");
        assert!(
            matches!(err, freshrss_search::SearchError::Unsupported(_)),
            "{name}: {err}"
        );

        // The matcher answers, and for the title cases its answer is checked here against the regex
        // engine directly rather than against itself.
        let matched = h.matcher_ids(expr);
        if let Pred::Title { pat, negated } = &expr.preds()[0] {
            let TextPat::Regex(re) = pat else {
                unreachable!()
            };
            let expected: BTreeSet<i64> = h
                .rows
                .iter()
                .filter(|row| re.is_match(&row.title) != *negated)
                .map(|row| row.id)
                .collect();
            assert_eq!(matched, expected, "{name}");
        }
        assert!(
            !matched.is_empty(),
            "{name}: the matcher found nothing, so the case proves little"
        );
    }
}

/// `date:` (ingestion, the id in µs) and `pubdate:` (publication, seconds) select different rows.
///
/// If the two spans ever collapsed onto each other a units bug would cancel out and the property
/// would keep passing.
#[test]
fn ingestion_and_publication_dates_are_distinct() {
    let h = &HARNESS;
    let window = |field| {
        h.sql_ids(&Expr::pred(Pred::Date {
            field,
            range: DateRange {
                min: Some(PUBLICATION_EPOCH),
                max: Some(PUBLICATION_EPOCH + 86_400 * 20),
            },
            negated: false,
        }))
    };
    assert_eq!(
        window(DateField::Publication).len(),
        21,
        "publication dates are one per day and the bounds are inclusive, so a 20-day span holds 21 rows"
    );
    assert!(
        window(DateField::Ingestion).is_empty(),
        "ingestion time is ~60 days later than publication; the windows must not overlap"
    );
}

/// A fragment scoped to one user never matches another user's rows, even when their titles, tags and
/// ids interleave with the first user's.
#[test]
fn the_sql_backend_never_crosses_the_user_boundary() {
    let h = &HARNESS;
    let id_user = h.id_user;
    let foreign: BTreeSet<i64> =
        h.rt.block_on(h.db.with(|mut conn| async move {
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT e.id FROM entry e WHERE e.id_user <> ?1 ORDER BY e.id")
                    .bind(id_user)
                    .fetch_all(&mut *conn)
                    .await?;
            Ok(ids)
        }))
        .expect("query runs")
        .into_iter()
        .collect();
    assert!(!foreign.is_empty(), "the corpus needs a second user");

    // Every title Bob has is also one of Alice's, so a missing `id_user` predicate shows up as
    // Bob's ids appearing in a result Alice's own corpus cannot produce.
    for title in TITLES.iter().take(6) {
        let expr = Expr::pred(Pred::Title {
            pat: TextPat::contains(title),
            negated: false,
        });
        let ids = h.sql_ids(&expr);
        assert!(
            ids.is_disjoint(&foreign),
            "intitle:{title:?} leaked another user's rows: {:?}",
            ids.intersection(&foreign).collect::<Vec<_>>()
        );
    }

    // `l:*` is the one predicate whose SQL joins through `tag`, which has no `id_user` of its own.
    let labelled = h.sql_ids(&Expr::pred(Pred::LabelIds {
        ids: IdSet::Any,
        negated: false,
    }));
    assert!(
        labelled.is_disjoint(&foreign),
        "l:* leaked another user's rows through the entrytag join"
    );
}

/// The shapes most likely to diverge, spelled out so a regression names itself instead of surfacing
/// as "case 137 failed".
#[test]
fn the_hard_cases_agree() {
    let h = &HARNESS;
    let contains = |pat: &str| TextPat::contains(pat);
    let cases: Vec<(&str, Expr)> = vec![
        // `_` and `%` are LIKE wildcards; `frss_ilike` must escape them or `a_b` matches `axum`.
        (
            "underscore is not a wildcard",
            Expr::pred(Pred::Title {
                pat: contains("a_b"),
                negated: false,
            }),
        ),
        (
            "percent is not a wildcard",
            Expr::pred(Pred::Title {
                pat: contains("100%"),
                negated: false,
            }),
        ),
        // Accent folding has to happen identically on both sides, in both directions.
        (
            "folded needle vs accented text",
            Expr::pred(Pred::Title {
                pat: contains("ecole"),
                negated: false,
            }),
        ),
        (
            "accented needle vs accented text",
            Expr::pred(Pred::Title {
                pat: contains("école"),
                negated: false,
            }),
        ),
        (
            "accented needle against the body",
            Expr::pred(Pred::Content {
                pat: contains("École"),
                negated: false,
            }),
        ),
        // Whole-tag matching: `#rus` must not match `#rust`, but `#rust` must match `#rust #rss`.
        (
            "whole tag, not a prefix",
            Expr::pred(Pred::Tag {
                pat: contains("rus"),
                negated: false,
            }),
        ),
        (
            "whole tag exact",
            Expr::pred(Pred::Tag {
                pat: contains("rust"),
                negated: false,
            }),
        ),
        (
            "negated whole tag",
            Expr::pred(Pred::Tag {
                pat: contains("axum"),
                negated: true,
            }),
        ),
        (
            "tag with no rows at all",
            Expr::pred(Pred::Tag {
                pat: contains("nonexistent"),
                negated: false,
            }),
        ),
        (
            "negated tag with no rows at all",
            Expr::pred(Pred::Tag {
                pat: contains("nonexistent"),
                negated: true,
            }),
        ),
        // `l:*` is "has at least one label", which is not the same as an empty id list.
        (
            "any label",
            Expr::pred(Pred::LabelIds {
                ids: IdSet::Any,
                negated: false,
            }),
        ),
        (
            "not any label",
            Expr::pred(Pred::LabelIds {
                ids: IdSet::Any,
                negated: true,
            }),
        ),
        (
            "empty label list matches nothing",
            Expr::pred(Pred::LabelIds {
                ids: IdSet::List(vec![]),
                negated: false,
            }),
        ),
        (
            "negated empty label list",
            Expr::pred(Pred::LabelIds {
                ids: IdSet::List(vec![]),
                negated: true,
            }),
        ),
        (
            "label by name, folded",
            Expr::pred(Pred::LabelNames {
                names: vec![contains("etiquette")],
                negated: false,
            }),
        ),
        (
            "label by name, accented",
            Expr::pred(Pred::LabelNames {
                names: vec![contains("étiquette")],
                negated: false,
            }),
        ),
        (
            "label name is a whole name, not a substring",
            Expr::pred(Pred::LabelNames {
                names: vec![contains("tod")],
                negated: false,
            }),
        ),
        (
            "two label names in one leaf",
            Expr::Leaf(vec![Pred::LabelNames {
                names: vec![contains("todo"), contains("done")],
                negated: false,
            }]),
        ),
        // Empty id lists must not produce `IN ()`, which is a SQLite syntax error.
        (
            "empty feed list",
            Expr::pred(Pred::FeedIds {
                ids: vec![],
                negated: false,
            }),
        ),
        (
            "negated empty feed list",
            Expr::pred(Pred::FeedIds {
                ids: vec![],
                negated: true,
            }),
        ),
        (
            "empty category list",
            Expr::pred(Pred::CategoryIds {
                ids: vec![],
                negated: false,
            }),
        ),
        // NULL `lastModified` / `lastUserModified` must be false in both polarities, not true-because-not-false.
        (
            "modified date over nullable column",
            Expr::pred(Pred::Date {
                field: DateField::Modified,
                range: DateRange {
                    min: Some(PUBLICATION_EPOCH),
                    max: None,
                },
                negated: false,
            }),
        ),
        (
            "negated modified date over nullable column",
            Expr::pred(Pred::Date {
                field: DateField::Modified,
                range: DateRange {
                    min: Some(PUBLICATION_EPOCH),
                    max: None,
                },
                negated: true,
            }),
        ),
        (
            "user-modified date over nullable column",
            Expr::pred(Pred::Date {
                field: DateField::UserModified,
                range: DateRange {
                    min: None,
                    max: Some(NOW),
                },
                negated: false,
            }),
        ),
        // A complemented range is the shape `-date:…` produces: the parser keeps the flag and the
        // two backends complement it in different places.
        (
            "complemented date range",
            Expr::pred(Pred::Date {
                field: DateField::Publication,
                range: DateRange {
                    min: Some(PUBLICATION_EPOCH + 86_400 * 30),
                    max: Some(PUBLICATION_EPOCH + 86_400 * 10 - 1),
                },
                negated: true,
            }),
        ),
        // Free text is title OR body, and negating it has to apply De Morgan rather than negate one arm.
        (
            "free text spans title and body",
            Expr::pred(Pred::FreeText {
                pat: contains("rust"),
                negated: false,
            }),
        ),
        (
            "negated free text",
            Expr::pred(Pred::FreeText {
                pat: contains("axum"),
                negated: true,
            }),
        ),
        // Multiple predicates in one leaf are an implicit AND, evaluated without a `Node`.
        (
            "implicit and in one leaf",
            Expr::Leaf(vec![
                Pred::Title {
                    pat: contains("rust"),
                    negated: false,
                },
                Pred::Tag {
                    pat: contains("rust"),
                    negated: false,
                },
            ]),
        ),
        (
            "implicit and with a negation inside",
            Expr::Leaf(vec![
                Pred::Title {
                    pat: contains("rust"),
                    negated: false,
                },
                Pred::Author {
                    pat: contains("bot"),
                    negated: true,
                },
            ]),
        ),
        // The boolean shapes: nesting, double negation, and a negated disjunction.
        (
            "and of two ors",
            Expr::and(vec![
                Expr::or(vec![
                    Expr::pred(Pred::Title {
                        pat: contains("rust"),
                        negated: false,
                    }),
                    Expr::pred(Pred::Title {
                        pat: contains("café"),
                        negated: false,
                    }),
                ]),
                Expr::or(vec![
                    Expr::pred(Pred::Tag {
                        pat: contains("rss"),
                        negated: false,
                    }),
                    Expr::pred(Pred::LabelIds {
                        ids: IdSet::Any,
                        negated: false,
                    }),
                ]),
            ]),
        ),
        (
            "negated or",
            Expr::negate(Expr::or(vec![
                Expr::pred(Pred::Title {
                    pat: contains("rust"),
                    negated: false,
                }),
                Expr::pred(Pred::Content {
                    pat: contains("axum"),
                    negated: false,
                }),
            ])),
        ),
        (
            "double negation",
            Expr::negate(Expr::negate(Expr::pred(Pred::Title {
                pat: contains("école"),
                negated: false,
            }))),
        ),
        (
            "negated and",
            Expr::negate(Expr::and(vec![
                Expr::pred(Pred::FeedIds {
                    ids: vec![h.feed_ids[0]],
                    negated: false,
                }),
                Expr::pred(Pred::CategoryIds {
                    ids: vec![h.category_ids[1]],
                    negated: false,
                }),
            ])),
        ),
        // A category predicate compiles to a subquery over `feed`, not a join — a join would
        // multiply rows and change the id set.
        (
            "category via subquery",
            Expr::pred(Pred::CategoryIds {
                ids: vec![h.category_ids[1]],
                negated: false,
            }),
        ),
        (
            "negated category via subquery",
            Expr::pred(Pred::CategoryIds {
                ids: vec![h.category_ids[1]],
                negated: true,
            }),
        ),
        // TRUE and FALSE have to survive as distinct shapes.
        ("empty leaf is true", Expr::TRUE),
        ("empty or is false", Expr::FALSE),
        (
            "and with a true child",
            Expr::and(vec![
                Expr::TRUE,
                Expr::pred(Pred::Title {
                    pat: contains("rust"),
                    negated: false,
                }),
            ]),
        ),
    ];

    let mut matched_something = 0usize;
    for (name, expr) in &cases {
        h.assert_agree_because(expr, name);
        if !h.sql_ids(expr).is_empty() {
            matched_something += 1;
        }
    }

    // Several of these are meant to match nothing (`#rus`, an empty id list, `Expr::FALSE`), so a
    // per-case non-vacuity check would be guesswork. But if the *majority* stopped matching, the
    // corpus or the needles have drifted and the comparisons above are proving less than they look.
    assert!(
        matched_something * 2 > cases.len(),
        "only {matched_something} of {} cases matched any row; the corpus no longer exercises them",
        cases.len()
    );
}
