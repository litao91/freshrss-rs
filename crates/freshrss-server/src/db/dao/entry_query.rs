//! The entry-listing query builder — a port of FreshRSS's `sqlListWhere` + `listWhereRaw`.
//!
//! Isolated in its own module because it is the most intricate SQL in the crate and because the SQL
//! half of it is *pure*: [`EntryQuery::build_rows`] turns a request into a string and a parameter list
//! with no connection in sight, so every shape below is testable without a database. [`list`] and
//! [`count_unread`] at the bottom are the only two functions that touch one.
//!
//! # Two statements, not one
//!
//! The listing is an inner select of ids wrapped by an outer join back to `entry`:
//!
//! ```sql
//! SELECT e0.… FROM entry e0
//!   INNER JOIN (SELECT e.id FROM entry e … ORDER BY … LIMIT ?) e2 ON e2.id = e0.id
//!   ORDER BY …
//! ```
//!
//! This is the "late row lookup" shape, and it is not decoration. The inner query sorts and filters
//! narrow columns while `content` — the bulk of the table — is read only for the rows that survived,
//! rather than for every row the sort examined. On a database with a hundred thousand articles that is
//! an order of magnitude.
//!
//! The cost is that the `ORDER BY` has to be repeated outside: the join discards the inner order, so the
//! page would arrive sorted by id whatever the reader asked for. That is why the sort expression is
//! built twice, once per alias set, and why [`Sort::CategoryName`] drags its joins into the outer query
//! too.
//!
//! # Pagination
//!
//! Keyset, never `OFFSET`. A cursor carries the boundary row's sort values and its id, and resuming is a
//! strict tuple comparison, so a page neither repeats a row nor skips one. `OFFSET` would do both as
//! soon as an article arrived mid-scroll, and it also gets slower the deeper it goes.
//!
//! Every comparison a cursor makes is **strict**, including the id at the end of the tuple. FreshRSS
//! uses `>=`/`<=` there and so returns the boundary row again on every page; its reader hides the repeat
//! by de-duplicating on id. Excluding it is what makes "no duplicates, no gaps" a property of the query
//! rather than of the client.
//!
//! `id_min`/`id_max` are a different thing and stay inclusive: they bound the *snapshot*, so an article
//! ingested while the reader scrolls cannot shift the pages underneath them.

use rusqlite::{Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};

use freshrss_search::ast::Expr;
use freshrss_search::sql::{compile, SqlCtx};
use freshrss_search::visibility::min_priority;

use crate::db::dao::entry;
use crate::db::sql::SqlValue;
use crate::domain::entry::Entry;
use crate::domain::ids::EntryId;
use crate::domain::list_type::ListType;
use crate::domain::priority::Priority;
use crate::domain::sort::{Order, SecondarySort, Sort};
use crate::domain::state::EntryState;
use crate::error::{Error, Result};

/// The alias set for one of the two statements.
#[derive(Clone, Copy)]
struct Aliases {
    entry: &'static str,
    feed: &'static str,
    category: &'static str,
}

/// The inner id-select.
const INNER: Aliases = Aliases {
    entry: "e",
    feed: "f",
    category: "c",
};

/// The outer late-row-lookup. FreshRSS uses `e0`/`f0`/`c0` and so does this, so a query pasted out of a
/// log reads the same in either codebase.
const OUTER: Aliases = Aliases {
    entry: "e0",
    feed: "f0",
    category: "c0",
};

/// The alias the *search fragment* uses for its own `entrytag` subqueries.
///
/// Deliberately not `et`, which the `t`/`T` scopes already bind: a nested `entrytag et` would shadow
/// the outer join, which is legal SQL and a debugging trap.
const SEARCH_ENTRYTAG: &str = "etx";

/// One boundary value from a cursor, in the type its column actually holds.
///
/// Typed rather than a plain string because SQLite orders every INTEGER below every TEXT: binding
/// `"1699"` against the numeric `date` column compares true for every row, and the page would silently
/// come back from the beginning instead of resuming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CursorValue {
    Int(i64),
    Text(String),
}

impl CursorValue {
    /// Binds as the type `numeric` says the column holds.
    ///
    /// A cursor is opaque client-held state, so one written by an older build can arrive with the wrong
    /// shape; converting it here costs one `parse` and beats returning the wrong page.
    fn bind(&self, numeric: bool) -> SqlValue {
        match (self, numeric) {
            (CursorValue::Int(v), true) => SqlValue::Int(*v),
            (CursorValue::Text(t), false) => SqlValue::Text(t.clone()),
            (CursorValue::Int(v), false) => SqlValue::Text(v.to_string()),
            (CursorValue::Text(t), true) => t
                .trim()
                .parse::<i64>()
                .map(SqlValue::Int)
                .unwrap_or(SqlValue::Text(t.clone())),
        }
    }
}

/// A resume point: the last row of the previous page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub id: EntryId,
    /// The boundary row's sort values, in ladder order: the primary sort value first, then the internal
    /// secondary (feed name, for a category sort), then the reader's secondary sort.
    /// [`Sort::continuation_arity`] says how many there must be.
    pub values: Vec<CursorValue>,
}

/// A listing request, in the terms the reader thinks in.
///
/// Everything is explicit rather than defaulted at the call site, so the SQL a request produces is a
/// pure function of this struct and can be asserted on directly.
#[derive(Debug, Clone)]
pub struct EntryQuery {
    pub id_user: i64,
    pub scope: ListType,
    /// The category, feed or tag id — required exactly when [`ListType::needs_id`].
    pub scope_id: i64,
    pub state: EntryState,
    /// An already-parsed and already-expanded search. `None` is no search, not an empty one.
    pub search: Option<Expr>,
    /// Inclusive lower bound on the entry id.
    pub id_min: Option<EntryId>,
    /// Inclusive upper bound on the entry id. The API defaults this to now, so a refresh landing
    /// mid-scroll cannot insert a page.
    pub id_max: Option<EntryId>,
    pub sort: Sort,
    pub order: Order,
    pub secondary_sort: SecondarySort,
    pub secondary_order: Order,
    pub cursor: Option<Cursor>,
    /// Rows to fetch. A caller that needs to know whether another page exists asks for one more than it
    /// wants and looks at how many came back.
    pub limit: usize,
}

impl Default for EntryQuery {
    fn default() -> Self {
        Self {
            id_user: 0,
            scope: ListType::default(),
            scope_id: 0,
            state: EntryState::ALL,
            search: None,
            id_min: None,
            id_max: None,
            sort: Sort::default(),
            order: Order::default(),
            secondary_sort: SecondarySort::default(),
            secondary_order: Order::default(),
            cursor: None,
            limit: 50,
        }
    }
}

/// A compiled statement: SQL with `?` placeholders, and the values to bind in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub sql: String,
    pub params: Vec<SqlValue>,
}

/// Which of the optional `WHERE` groups to emit.
///
/// The unread count shares the scope, the fences and the search with the listing but not the other two:
/// it *is* a read-state filter, and it counts the whole scope rather than the page after a cursor.
#[derive(Debug, Clone, Copy)]
struct Include {
    state: bool,
    cursor: bool,
    unread_only: bool,
}

const FOR_LISTING: Include = Include {
    state: true,
    cursor: true,
    unread_only: false,
};

const FOR_UNREAD_COUNT: Include = Include {
    state: false,
    cursor: false,
    unread_only: true,
};

/// A `WHERE` body under construction.
#[derive(Default)]
struct Conditions {
    parts: Vec<String>,
    params: Vec<SqlValue>,
}

impl Conditions {
    fn push(&mut self, sql: impl Into<String>) {
        self.parts.push(sql.into());
    }

    fn bind(&mut self, sql: impl Into<String>, params: impl IntoIterator<Item = SqlValue>) {
        self.parts.push(sql.into());
        self.params.extend(params);
    }

    fn into_built(self, prefix: &str) -> Built {
        Built {
            sql: format!("{prefix}{}", self.parts.join(" AND ")),
            params: self.params,
        }
    }
}

/// One rung of the cursor ladder.
struct Rung {
    expr: String,
    sign: &'static str,
    value: SqlValue,
}

impl EntryQuery {
    /// Checks the combination before building anything.
    ///
    /// These are caller mistakes rather than database errors, so they surface as `400` and not `500`.
    pub fn validate(&self) -> Result<()> {
        if self.scope.needs_id() && self.scope_id == 0 {
            return Err(Error::Validation(format!(
                "scope {} needs an id",
                self.scope.as_scope()
            )));
        }
        if !self.scope.needs_id() && self.scope_id != 0 {
            // FreshRSS ignores an id the scope does not use. Answering anyway is how a request for the
            // wrong folder returns the main stream and nobody notices.
            return Err(Error::Validation(format!(
                "scope {} does not take an id",
                self.scope.as_scope()
            )));
        }
        if self.cursor.is_some() && !self.sort.is_resumable() {
            return Err(Error::Validation(format!(
                "sort {} cannot be resumed from a cursor",
                self.sort.as_str()
            )));
        }
        if let Some(cursor) = &self.cursor {
            let want = self.sort.continuation_arity(self.secondary_sort);
            if cursor.values.len() != want {
                return Err(Error::Validation(format!(
                    "a {} cursor carries {want} sort values, got {}",
                    self.sort.as_str(),
                    cursor.values.len()
                )));
            }
        }
        Ok(())
    }

    /// The inner id-select, with `LIMIT` bound as the last parameter.
    pub fn build_ids(&self) -> Result<Built> {
        self.validate()?;
        let a = INNER;
        let where_ = self.where_body(&a, FOR_LISTING)?.into_built("WHERE ");

        // `SELECT DISTINCT` requires every ORDER BY expression to appear in the select list. That is
        // only a constraint for the `T` scope, which joins `entrytag` and so can return an article once
        // per label.
        let distinct = self.scope.needs_distinct() && self.sort != Sort::Rand;
        let mut select = String::from(if distinct {
            "SELECT DISTINCT "
        } else {
            "SELECT "
        });
        select.push_str(&col(&a, "id"));
        if distinct && self.sort != Sort::Id {
            select.push_str(", ");
            select.push_str(&sort_expr(self.sort, &a));
        }

        let mut sql = format!(
            "{select} FROM entry {ae} INNER JOIN feed {af} ON {af}.id = {ae}.id_feed ",
            ae = a.entry,
            af = a.feed
        );
        if self.sort.joins_category() {
            sql.push_str(&format!(
                "INNER JOIN category {ac} ON {ac}.id = {af}.category ",
                ac = a.category,
                af = a.feed
            ));
        }
        if self.scope.joins_entrytag() {
            sql.push_str(&format!(
                "INNER JOIN entrytag et ON et.id_entry = {} ",
                col(&a, "id")
            ));
        }
        sql.push_str(&where_.sql);

        // `DISTINCT` cannot be combined with a non-deterministic ORDER BY, so the random sort groups
        // instead, which collapses the duplicate label rows just as well.
        if self.scope.needs_distinct() && self.sort == Sort::Rand {
            sql.push_str(&format!(" GROUP BY {}", col(&a, "id")));
        }

        sql.push_str(&order_by(self, &a));
        sql.push_str(" LIMIT ?");

        let mut params = where_.params;
        params.push(SqlValue::Int(self.limit as i64));
        Ok(Built { sql, params })
    }

    /// The outer late-row-lookup: full columns for the ids the inner select chose.
    pub fn build_rows(&self) -> Result<Built> {
        let inner = self.build_ids()?;
        let a = OUTER;

        let mut sql = format!(
            "SELECT {} FROM entry {ae} INNER JOIN ({inner}) e2 ON e2.id = {id}",
            entry::columns_aliased(a.entry),
            ae = a.entry,
            inner = inner.sql,
            id = col(&a, "id")
        );
        if self.sort.joins_feed() {
            sql.push_str(&format!(
                " INNER JOIN feed {af} ON {af}.id = {ae}.id_feed",
                af = a.feed,
                ae = a.entry
            ));
        }
        if self.sort.joins_category() {
            sql.push_str(&format!(
                " INNER JOIN category {ac} ON {ac}.id = {af}.category",
                ac = a.category,
                af = a.feed
            ));
        }
        sql.push_str(&order_by(self, &a));

        Ok(Built {
            sql,
            params: inner.params,
        })
    }

    /// How many unread articles the scope holds, ignoring the cursor and the read-state filter.
    ///
    /// The sidebar badge and the list header show the same number, so it is computed from the same
    /// scope, fences and search as the listing — a second, simpler query would drift.
    pub fn build_unread_count(&self) -> Result<Built> {
        self.validate()?;
        let a = INNER;
        let where_ = self.where_body(&a, FOR_UNREAD_COUNT)?;

        // The scope can reference the feed's priority, and `ST` reaches labels through an EXISTS that
        // needs no join, so `feed` is the only table this statement always needs.
        let mut sql = format!(
            "SELECT COUNT(*) FROM entry {ae} INNER JOIN feed {af} ON {af}.id = {ae}.id_feed ",
            ae = a.entry,
            af = a.feed
        );
        if self.scope == ListType::Tag {
            sql.push_str(&format!(
                "INNER JOIN entrytag et ON et.id_entry = {} ",
                col(&a, "id")
            ));
        }
        // `T` counts articles, not article-label pairs, so it reaches labels the same way `ST` does.
        if self.scope == ListType::AnyTag {
            sql.push_str(&format!(
                "WHERE EXISTS (SELECT 1 FROM entrytag et2 WHERE et2.id_entry = {}) AND ",
                col(&a, "id")
            ));
        } else {
            sql.push_str("WHERE ");
        }
        sql.push_str(&where_.parts.join(" AND "));

        Ok(Built {
            sql,
            params: where_.params,
        })
    }

    /// The `WHERE` groups shared by all three statements, starting from the user fence.
    fn where_body(&self, a: &Aliases, include: Include) -> Result<Conditions> {
        let mut w = Conditions::default();
        // First, and non-negotiable: one shared database means every listing is scoped to one user, and
        // forgetting it would show one reader another's articles.
        w.bind(
            format!("{} = ?", col(a, "id_user")),
            [SqlValue::Int(self.id_user)],
        );

        self.scope_conditions(a, &mut w);

        if include.unread_only {
            w.push(format!("{} = 0", col(a, "is_read")));
        } else if include.state {
            self.state_conditions(a, &mut w);
        }

        // A nullable sort column has to be excluded rather than ordered: no comparison against NULL is
        // true, so a cursor could not resume past one, and SQLite's placement of NULLs would decide the
        // page boundary.
        if self.sort.column_is_nullable() {
            w.push(format!("{} IS NOT NULL", sort_expr(self.sort, a)));
        }

        if let Some(min) = self.id_min {
            w.bind(
                format!("{} >= ?", col(a, "id")),
                [SqlValue::Int(min.micros())],
            );
        }
        if let Some(max) = self.id_max {
            w.bind(
                format!("{} <= ?", col(a, "id")),
                [SqlValue::Int(max.micros())],
            );
        }

        if include.cursor {
            if let Some(rungs) = self.cursor_ladder(a) {
                let mut terms = Vec::new();
                let mut params = Vec::new();
                // Rung `k` reads: every level before it equal, and level `k` strictly after. The last
                // rung is the id, which is what makes the whole ladder a total order.
                for k in 0..rungs.len() {
                    let mut conjuncts = Vec::new();
                    for rung in rungs.iter().take(k) {
                        conjuncts.push(format!("{} = ?", rung.expr));
                        params.push(rung.value.clone());
                    }
                    let rung = &rungs[k];
                    conjuncts.push(format!("{} {} ?", rung.expr, rung.sign));
                    params.push(rung.value.clone());
                    terms.push(format!("({})", conjuncts.join(" AND ")));
                }
                w.bind(format!("({})", terms.join(" OR ")), params);
            }
        }

        // An empty search compiles to `1=1`, which is correct and pointless; leaving it out keeps the
        // common case — no search at all and a search that turned out to be empty — identical.
        if let Some(expr) = self.search.as_ref().filter(|expr| !expr.is_true()) {
            let fragment = compile(expr, &self.sql_ctx())?;
            if !fragment.sql.is_empty() {
                w.bind(
                    format!("({})", fragment.sql),
                    fragment.params.iter().map(SqlValue::from),
                );
            }
        }

        Ok(w)
    }

    /// The [`SqlCtx`] the search fragment is compiled with.
    fn sql_ctx(&self) -> SqlCtx<'static> {
        SqlCtx {
            alias: INNER.entry,
            entrytag_alias: SEARCH_ENTRYTAG,
            // The user fence is emitted above, so the label subquery need not repeat it.
            scoped_to_user: true,
        }
    }

    /// The scope's own conditions: the priority floor and whatever the scope names.
    fn scope_conditions(&self, a: &Aliases, w: &mut Conditions) {
        if let Some(floor) = self.priority_floor() {
            w.bind(
                format!("{}.priority >= ?", a.feed),
                [SqlValue::Int(i64::from(floor.get()))],
            );
        }
        match self.scope {
            // Scopes expressed entirely by the floor, or that want everything.
            ListType::MainStream
            | ListType::AllButHidden
            | ListType::IncludingHidden
            | ListType::Important
            | ListType::AnyTag => {}
            ListType::Starred => w.push(format!("{} = 1", col(a, "is_favorite"))),
            ListType::Category => w.bind(
                format!("{}.category = ?", a.feed),
                [SqlValue::Int(self.scope_id)],
            ),
            ListType::Feed => w.bind(
                format!("{} = ?", col(a, "id_feed")),
                [SqlValue::Int(self.scope_id)],
            ),
            ListType::Tag => w.bind("et.id_tag = ?", [SqlValue::Int(self.scope_id)]),
            ListType::StarredOrTagged => w.push(format!(
                "({fav} = 1 OR EXISTS (SELECT 1 FROM entrytag et2 WHERE et2.id_entry = {id}))",
                fav = col(a, "is_favorite"),
                id = col(a, "id")
            )),
        }
    }

    /// The priority floor for this scope, taking into account what the search itself demands.
    fn priority_floor(&self) -> Option<Priority> {
        // `min_priority` is the floor a search needs in order to be satisfiable at all: naming a hidden
        // feed has to lift it into the listing, or `f:12` could never return anything.
        let visibility = self
            .search
            .as_ref()
            .and_then(|expr| min_priority(expr).map(Priority::new));
        self.scope.priority_floor(visibility)
    }

    /// The read/starred filter, as two groups ANDed together.
    fn state_conditions(&self, a: &Aliases, w: &mut Conditions) {
        for cond in &self.state.ands {
            w.push(format!("{} = {}", col(a, cond.column()), cond.value()));
        }
        if !self.state.ors.is_empty() {
            let alternatives = self
                .state
                .ors
                .iter()
                .map(|cond| format!("{} = {}", col(a, cond.column()), cond.value()))
                .collect::<Vec<_>>()
                .join(" OR ");
            w.push(format!("({alternatives})"));
        }
    }

    /// The cursor's comparison ladder, or `None` when there is no cursor to resume from.
    fn cursor_ladder(&self, a: &Aliases) -> Option<Vec<Rung>> {
        let cursor = self.cursor.as_ref()?;
        let sign = self.order.sign();
        let id = col(a, "id");

        // Sorting by id *is* the keyset, so the cursor just tightens the fence. Strict, so the boundary
        // row does not come back.
        if self.sort == Sort::Id {
            return Some(vec![Rung {
                expr: id,
                sign,
                value: SqlValue::Int(cursor.id.micros()),
            }]);
        }

        let mut ladder = vec![Rung {
            expr: sort_expr(self.sort, a),
            sign,
            value: cursor.values[0].bind(self.sort.value_is_numeric()),
        }];
        // Index into `cursor.values`, which carries one value per rung except the id's own.
        let mut next = 1;

        // A category sort has an internal secondary on the feed name, ahead of the reader's own: two
        // folders can hold feeds of the same name, and without this a page boundary could fall inside a
        // run of ties. It follows the primary direction, not the reader's secondary one.
        if self.sort == Sort::CategoryName {
            ladder.push(Rung {
                expr: format!("{}.name", a.feed),
                sign,
                value: cursor.values[next].bind(false),
            });
            next += 1;
        }
        // A secondary sort of `id` *is* the final rung, so it gets no rung of its own: emitting both
        // would compare the id twice and, if the reader picked the opposite direction, produce an order
        // the cursor could not resume from at all.
        if self.sort.takes_secondary_sort() && self.secondary_sort != SecondarySort::Id {
            ladder.push(Rung {
                expr: secondary_expr(self.secondary_sort, a),
                sign: self.secondary_order.sign(),
                value: cursor.values[next].bind(self.secondary_sort.value_is_numeric()),
            });
        }

        ladder.push(Rung {
            expr: id,
            sign,
            value: SqlValue::Int(cursor.id.micros()),
        });
        Some(ladder)
    }
}

/// One column reference, qualified.
fn col(a: &Aliases, name: &str) -> String {
    format!("{}.{name}", a.entry)
}

/// The SQL expression a primary sort orders by.
fn sort_expr(sort: Sort, a: &Aliases) -> String {
    match sort {
        Sort::Id => col(a, "id"),
        Sort::Date => col(a, "date"),
        Sort::CategoryName => format!("{}.name", a.category),
        Sort::FeedName => format!("{}.name", a.feed),
        Sort::LastModified => col(a, "lastModified"),
        Sort::LastUserModified => col(a, "lastUserModified"),
        Sort::Length => format!("LENGTH({})", col(a, "content")),
        Sort::Link => col(a, "link"),
        Sort::Title => col(a, "title"),
        Sort::Rand => "RANDOM()".to_owned(),
    }
}

/// The SQL expression a secondary sort orders by.
fn secondary_expr(sort: SecondarySort, a: &Aliases) -> String {
    match sort {
        SecondarySort::Id => col(a, "id"),
        SecondarySort::Date => col(a, "date"),
        SecondarySort::Link => col(a, "link"),
        SecondarySort::Title => col(a, "title"),
    }
}

/// The `ORDER BY` clause, identical in shape for both statements.
fn order_by(q: &EntryQuery, a: &Aliases) -> String {
    let order = q.order.as_str();
    let mut out = format!(" ORDER BY {} {order}", sort_expr(q.sort, a));
    if q.sort == Sort::CategoryName {
        out.push_str(&format!(", {}.name {order}", a.feed));
    }
    // A secondary sort of `id` is the tie-breaker below, so it is not emitted twice — and it must not
    // be, because the tie-breaker follows the *primary* direction, which is what makes the keyset total.
    if q.sort.takes_secondary_sort() && q.secondary_sort != SecondarySort::Id {
        out.push_str(&format!(
            ", {} {}",
            secondary_expr(q.secondary_sort, a),
            q.secondary_order.as_str()
        ));
    }
    // Sorting by id already is the tie-breaker, and a random sort has no stable order to break ties in
    // (nor a cursor to resume from), so neither gets one.
    if !matches!(q.sort, Sort::Id | Sort::Rand) {
        // Without this two articles with the same title are unordered relative to each other and a
        // cursor cannot resume between them.
        out.push_str(&format!(", {} {order}", col(a, "id")));
    }
    out
}

/// The cursor that resumes immediately after `id`, under this query's sort.
///
/// This is what makes prev/next work for any sort rather than only for the id sort: the reader asks for
/// the article beside the one it is showing, and "beside" means beside *in the current listing order*.
/// Building it needs the boundary row's sort values, so they are read back with the same expressions the
/// `ORDER BY` uses — one row, by primary key.
pub fn cursor_after(conn: &Connection, q: &EntryQuery, id: EntryId) -> Result<Cursor> {
    q.validate()?;
    if q.sort == Sort::Id {
        // The id sort carries no boundary values, and the ladder would be a single rung anyway.
        return Ok(Cursor {
            id,
            values: Vec::new(),
        });
    }
    let a = INNER;
    let mut selects = vec![(sort_expr(q.sort, &a), q.sort.value_is_numeric())];
    if q.sort == Sort::CategoryName {
        selects.push((format!("{}.name", a.feed), false));
    }
    if q.sort.takes_secondary_sort() && q.secondary_sort != SecondarySort::Id {
        selects.push((
            secondary_expr(q.secondary_sort, &a),
            q.secondary_sort.value_is_numeric(),
        ));
    }

    let mut sql = format!(
        "SELECT {} FROM entry {} INNER JOIN feed {} ON {f}.id = {e}.id_feed ",
        selects
            .iter()
            .map(|(expr, _)| expr.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        a.entry,
        a.feed,
        e = a.entry,
        f = a.feed
    );
    if q.sort.joins_category() {
        sql.push_str(&format!(
            "LEFT JOIN category {c} ON {c}.id = {f}.category ",
            c = a.category,
            f = a.feed
        ));
    }
    sql.push_str(&format!("WHERE {} = ?", col(&a, "id")));

    let numeric: Vec<bool> = selects.iter().map(|(_, n)| *n).collect();
    let values = conn
        .query_row(&sql, [id.micros()], |row| {
            (0..numeric.len())
                .map(|i| {
                    // The type has to match the column's, because SQLite orders every INTEGER below
                    // every TEXT and a mismatched bind would resume from the wrong end of the list.
                    Ok(match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Integer(v) if numeric[i] => CursorValue::Int(v),
                        rusqlite::types::ValueRef::Integer(v) => CursorValue::Text(v.to_string()),
                        rusqlite::types::ValueRef::Text(t) => {
                            CursorValue::Text(String::from_utf8_lossy(t).into_owned())
                        }
                        rusqlite::types::ValueRef::Null => CursorValue::Text(String::new()),
                        rusqlite::types::ValueRef::Real(v) => CursorValue::Text(v.to_string()),
                        rusqlite::types::ValueRef::Blob(_) => CursorValue::Text(String::new()),
                    })
                })
                .collect::<SqlResult<Vec<_>>>()
        })
        .map_err(Error::from)?;

    Ok(Cursor { id, values })
}

/// Runs the listing and reads the rows.
pub fn list(conn: &Connection, q: &EntryQuery) -> Result<Vec<Entry>> {
    let built = q.build_rows()?;
    let mut stmt = conn.prepare(&built.sql)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(built.params.iter()),
            entry::from_row,
        )?
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(rows)
}

/// How many unread articles the same scope, fences and search admit.
pub fn count_unread(conn: &Connection, q: &EntryQuery) -> Result<i64> {
    let built = q.build_unread_count()?;
    Ok(conn.query_row(
        &built.sql,
        rusqlite::params_from_iter(built.params.iter()),
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use freshrss_search::ast::{Expr, Pred};
    use freshrss_search::parser::parse;

    use super::*;
    use crate::domain::state::Cond;

    const NOW: i64 = 1_700_000_000;

    fn base() -> EntryQuery {
        EntryQuery {
            id_user: 7,
            ..EntryQuery::default()
        }
    }

    fn ids(q: &EntryQuery) -> Built {
        q.build_ids().expect("builds")
    }

    fn rows(q: &EntryQuery) -> Built {
        q.build_rows().expect("builds")
    }

    fn err(q: &EntryQuery) -> Error {
        q.build_ids().expect_err("should have been rejected")
    }

    // --- the user fence ----------------------------------------------------

    #[test]
    fn every_statement_is_scoped_to_one_user_first() {
        // The one invariant a shared database cannot afford to lose, so it is asserted on all three
        // statements rather than only on the listing.
        let q = base();
        for built in [ids(&q), q.build_unread_count().unwrap()] {
            assert!(
                built.sql.contains("WHERE e.id_user = ?"),
                "{}",
                built.sql
            );
            assert_eq!(built.params[0], SqlValue::Int(7));
        }
        assert!(rows(&q).sql.contains("e0.id_user"), "{}", rows(&q).sql);
    }

    #[test]
    fn a_search_fragment_never_reuses_the_scopes_entrytag_alias() {
        let expr = parse("labels:todo", NOW).unwrap();
        let q = EntryQuery {
            scope: ListType::AnyTag,
            search: Some(expr.clone()),
            ..base()
        };
        let built = ids(&q);
        assert!(built.sql.contains("INNER JOIN entrytag et ON"), "{}", built.sql);
        assert!(
            built.sql.contains("FROM entrytag etx"),
            "the label subquery must not shadow the scope's join: {}",
            built.sql
        );
    }

    // --- scopes ------------------------------------------------------------

    #[test]
    fn each_scope_emits_its_own_priority_floor() {
        let floors = [
            (ListType::MainStream, Some(10)),
            (ListType::AllButHidden, Some(0)),
            (ListType::Important, Some(20)),
            (ListType::Category, Some(0)),
            (ListType::IncludingHidden, None),
            (ListType::Starred, None),
            (ListType::Feed, None),
            (ListType::Tag, None),
            (ListType::AnyTag, None),
            (ListType::StarredOrTagged, None),
        ];
        for (scope, floor) in floors {
            let q = EntryQuery {
                scope,
                scope_id: i64::from(scope.needs_id()),
                ..base()
            };
            let built = ids(&q);
            match floor {
                Some(v) => {
                    assert!(built.sql.contains("f.priority >= ?"), "{scope:?}: {}", built.sql);
                    assert!(
                        built.params.contains(&SqlValue::Int(v)),
                        "{scope:?} should floor at {v}: {:?}",
                        built.params
                    );
                }
                None => assert!(
                    !built.sql.contains("f.priority"),
                    "{scope:?} puts no constraint on priority: {}",
                    built.sql
                ),
            }
        }
    }

    #[test]
    fn a_search_naming_a_hidden_feed_lifts_the_floor() {
        // Without this, `f:12` on a feed the user hid could never return anything: the scope would
        // filter out the very row the search asked for.
        let expr = parse("f:12", NOW).unwrap();
        let q = EntryQuery {
            search: Some(expr.clone()),
            ..base()
        };
        let built = ids(&q);
        assert!(
            built.params.contains(&SqlValue::Int(i64::from(Priority::HIDDEN.get()))),
            "{:?}",
            built.params
        );
    }

    #[test]
    fn a_negated_feed_does_not_lift_the_floor() {
        // `-f:12` excludes a feed, so there is no reason to widen the listing to reach it.
        let expr = parse("-f:12", NOW).unwrap();
        let q = EntryQuery {
            search: Some(expr.clone()),
            ..base()
        };
        assert!(
            !ids(&q).params.contains(&SqlValue::Int(i64::from(Priority::HIDDEN.get()))),
            "{:?}",
            ids(&q).params
        );
    }

    #[test]
    fn the_scoped_predicates() {
        let cases: &[(ListType, i64, &str)] = &[
            (ListType::Starred, 0, "e.is_favorite = 1"),
            (ListType::Feed, 3, "e.id_feed = ?"),
            (ListType::Category, 4, "f.category = ?"),
            (ListType::Tag, 5, "et.id_tag = ?"),
        ];
        for (scope, id, expected) in cases {
            let q = EntryQuery {
                scope: *scope,
                scope_id: *id,
                ..base()
            };
            let built = ids(&q);
            assert!(built.sql.contains(expected), "{scope:?}: {}", built.sql);
            if *id != 0 {
                assert!(
                    built.params.contains(&SqlValue::Int(*id)),
                    "{scope:?}: {:?}",
                    built.params
                );
            }
        }
    }

    #[test]
    fn starred_or_tagged_reaches_labels_through_an_exists() {
        let q = EntryQuery {
            scope: ListType::StarredOrTagged,
            ..base()
        };
        let built = ids(&q);
        assert!(
            built.sql.contains("(e.is_favorite = 1 OR EXISTS (SELECT 1 FROM entrytag et2"),
            "{}",
            built.sql
        );
        // An EXISTS rather than a join, so an article with three labels is not counted three times.
        assert!(!built.sql.contains("INNER JOIN entrytag"), "{}", built.sql);
    }

    #[test]
    fn the_label_scopes_join_entrytag_and_only_any_tag_deduplicates() {
        let tag = ids(&EntryQuery {
            scope: ListType::Tag,
            scope_id: 1,
            ..base()
        });
        assert!(tag.sql.contains("INNER JOIN entrytag et"), "{}", tag.sql);
        assert!(!tag.sql.contains("DISTINCT"), "one id_tag cannot yield two rows: {}", tag.sql);

        let any = ids(&EntryQuery {
            scope: ListType::AnyTag,
            ..base()
        });
        assert!(any.sql.starts_with("SELECT DISTINCT e.id"), "{}", any.sql);
    }

    #[test]
    fn distinct_carries_the_order_by_column_into_the_select_list() {
        // `SELECT DISTINCT … ORDER BY <expr>` requires <expr> to be selected.
        let built = ids(&EntryQuery {
            scope: ListType::AnyTag,
            sort: Sort::Date,
            ..base()
        });
        assert!(
            built.sql.starts_with("SELECT DISTINCT e.id, e.date FROM"),
            "{}",
            built.sql
        );
        // Sorting by id needs no extra column, and the random sort cannot be DISTINCT at all.
        let by_id = ids(&EntryQuery {
            scope: ListType::AnyTag,
            ..base()
        });
        assert!(by_id.sql.starts_with("SELECT DISTINCT e.id FROM"), "{}", by_id.sql);
        let random = ids(&EntryQuery {
            scope: ListType::AnyTag,
            sort: Sort::Rand,
            ..base()
        });
        assert!(!random.sql.contains("DISTINCT"), "{}", random.sql);
        assert!(random.sql.contains("GROUP BY e.id"), "{}", random.sql);
    }

    // --- state -------------------------------------------------------------

    #[test]
    fn the_state_groups_are_anded_then_ored() {
        let q = EntryQuery {
            state: EntryState {
                ands: vec![Cond::NotRead],
                ors: vec![Cond::NotRead, Cond::Favorite],
            },
            ..base()
        };
        let sql = ids(&q).sql;
        assert!(sql.contains("e.is_read = 0"), "{sql}");
        assert!(
            sql.contains("(e.is_read = 0 OR e.is_favorite = 1)"),
            "{sql}"
        );
    }

    #[test]
    fn an_unconstrained_state_emits_nothing() {
        let sql = ids(&base()).sql;
        assert!(!sql.contains("is_read"), "{sql}");
        assert!(!sql.contains("is_favorite"), "{sql}");
    }

    #[test]
    fn the_unread_count_ignores_the_state_filter_and_the_cursor() {
        // It *is* a read filter, so honouring the caller's would count nothing whenever the caller
        // asked for read articles; and it counts the whole scope, not the page after a cursor.
        let q = EntryQuery {
            state: EntryState::read(),
            cursor: Some(Cursor {
                id: EntryId::from_unix(NOW),
                values: Vec::new(),
            }),
            ..base()
        };
        let built = q.build_unread_count().unwrap();
        assert!(built.sql.contains("e.is_read = 0"), "{}", built.sql);
        assert!(!built.sql.contains("e.is_read = 1"), "{}", built.sql);
        assert!(!built.sql.contains("e.id <"), "no cursor: {}", built.sql);
        assert!(built.sql.starts_with("SELECT COUNT(*)"), "{}", built.sql);
        // But it does keep the snapshot fence and the search, or the badge would disagree with the list.
        let q = EntryQuery {
            id_max: Some(EntryId::from_unix(NOW)),
            ..base()
        };
        assert!(q.build_unread_count().unwrap().sql.contains("e.id <= ?"));
    }

    #[test]
    fn the_unread_count_counts_any_tag_articles_not_article_label_pairs() {
        let built = EntryQuery {
            scope: ListType::AnyTag,
            ..base()
        }
        .build_unread_count()
        .unwrap();
        assert!(!built.sql.contains("INNER JOIN entrytag"), "{}", built.sql);
        assert!(built.sql.contains("WHERE EXISTS (SELECT 1 FROM entrytag et2"), "{}", built.sql);
    }

    // --- fences and cursors ------------------------------------------------

    #[test]
    fn the_snapshot_fences_are_inclusive() {
        let q = EntryQuery {
            id_min: Some(EntryId::from_micros(100)),
            id_max: Some(EntryId::from_micros(200)),
            ..base()
        };
        let built = ids(&q);
        assert!(built.sql.contains("e.id >= ?"), "{}", built.sql);
        assert!(built.sql.contains("e.id <= ?"), "{}", built.sql);
        // After the user fence and the main-stream priority floor.
        assert_eq!(built.params[2], SqlValue::Int(100));
        assert_eq!(built.params[3], SqlValue::Int(200));
    }

    #[test]
    fn an_id_sort_cursor_tightens_the_fence_strictly() {
        for (order, sign) in [(Order::Desc, "e.id < ?"), (Order::Asc, "e.id > ?")] {
            let q = EntryQuery {
                order,
                cursor: Some(Cursor {
                    id: EntryId::from_micros(500),
                    values: Vec::new(),
                }),
                ..base()
            };
            let built = ids(&q);
            assert!(built.sql.contains(sign), "{order:?}: {}", built.sql);
            // Strict, not `<=`: an inclusive comparison would return the boundary row again.
            assert!(
                !built.sql.contains(&sign.replace('?', "? =")),
                "{}",
                built.sql
            );
            assert!(
                built.params.contains(&SqlValue::Int(500)),
                "{:?}",
                built.params
            );
        }
    }

    #[test]
    fn a_simple_sort_cursor_expands_to_two_terms() {
        let q = EntryQuery {
            sort: Sort::Date,
            order: Order::Desc,
            cursor: Some(Cursor {
                id: EntryId::from_micros(500),
                values: vec![CursorValue::Int(NOW)],
            }),
            ..base()
        };
        let built = ids(&q);
        assert!(
            built.sql.contains("((e.date < ?) OR (e.date = ? AND e.id < ?))"),
            "{}",
            built.sql
        );
        assert_eq!(
            built.params[2..],
            [
                SqlValue::Int(NOW),
                SqlValue::Int(NOW),
                SqlValue::Int(500),
                SqlValue::Int(50),
            ]
        );
    }

    #[test]
    fn a_feed_sort_cursor_expands_to_three_terms() {
        let q = EntryQuery {
            sort: Sort::FeedName,
            order: Order::Asc,
            secondary_sort: SecondarySort::Title,
            secondary_order: Order::Desc,
            cursor: Some(Cursor {
                id: EntryId::from_micros(500),
                values: vec![
                    CursorValue::Text("Example".into()),
                    CursorValue::Text("A title".into()),
                ],
            }),
            ..base()
        };
        let built = ids(&q);
        assert!(
            built.sql.contains(
                "((f.name > ?) OR (f.name = ? AND e.title < ?) \
                 OR (f.name = ? AND e.title = ? AND e.id > ?))"
            ),
            "{}",
            built.sql
        );
        // The secondary sort follows its own direction (`<`), the id follows the primary one (`>`).
        assert_eq!(
            built.params[2..],
            [
                SqlValue::Text("Example".into()),
                SqlValue::Text("Example".into()),
                SqlValue::Text("A title".into()),
                SqlValue::Text("Example".into()),
                SqlValue::Text("A title".into()),
                SqlValue::Int(500),
                SqlValue::Int(50),
            ]
        );
    }

    #[test]
    fn a_category_sort_cursor_expands_to_four_terms() {
        let q = EntryQuery {
            sort: Sort::CategoryName,
            secondary_sort: SecondarySort::Date,
            cursor: Some(Cursor {
                id: EntryId::from_micros(500),
                values: vec![
                    CursorValue::Text("News".into()),
                    CursorValue::Text("Example".into()),
                    CursorValue::Int(NOW),
                ],
            }),
            ..base()
        };
        let built = ids(&q);
        // The feed name sits between the folder and the reader's own secondary sort.
        assert!(built.sql.contains("c.name < ?"), "{}", built.sql);
        assert!(
            built.sql.contains("(c.name = ? AND f.name < ?)"),
            "{}",
            built.sql
        );
        assert!(
            built.sql.contains("(c.name = ? AND f.name = ? AND e.date = ? AND e.id < ?)"),
            "{}",
            built.sql
        );
        assert_eq!(built.sql.matches("c.name = ?").count(), 3, "{}", built.sql);
        // user fence + priority floor + (1 + 2 + 3 + 4) ladder values + LIMIT.
        assert_eq!(built.params.len(), 13, "{:?}", built.params);
    }

    #[test]
    fn an_id_secondary_sort_adds_no_rung_of_its_own() {
        // The final rung already compares the id, so repeating it would bind the boundary twice — and if
        // the reader had picked the opposite direction the two would contradict each other and the
        // cursor could not resume at all.
        let q = EntryQuery {
            sort: Sort::FeedName,
            secondary_sort: SecondarySort::Id,
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: vec![CursorValue::Text("Example".into())],
            }),
            ..base()
        };
        let built = ids(&q);
        assert!(
            built.sql.contains("((f.name < ?) OR (f.name = ? AND e.id < ?))"),
            "{}",
            built.sql
        );
    }

    #[test]
    fn a_cursor_binds_a_numeric_boundary_as_a_number() {
        // SQLite orders every INTEGER below every TEXT, so binding "1700000000" against `e.date`
        // would compare true for every row and the listing would restart from the top.
        let q = EntryQuery {
            sort: Sort::Date,
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: vec![CursorValue::Int(NOW)],
            }),
            ..base()
        };
        assert_eq!(ids(&q).params[2], SqlValue::Int(NOW));

        // And a cursor written by an older build, which carried strings, is coerced rather than
        // silently returning the wrong page.
        let q = EntryQuery {
            sort: Sort::Date,
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: vec![CursorValue::Text(NOW.to_string())],
            }),
            ..base()
        };
        assert_eq!(ids(&q).params[2], SqlValue::Int(NOW));
    }

    #[test]
    fn a_nullable_sort_excludes_the_rows_it_cannot_order() {
        for sort in [Sort::LastModified, Sort::LastUserModified] {
            let built = ids(&EntryQuery { sort, ..base() });
            assert!(
                built.sql.contains(&format!("{} IS NOT NULL", sort_column(sort))),
                "{sort:?}: {}",
                built.sql
            );
        }
        // `entry.date` and `entry.id` are NOT NULL, so the guard would be noise.
        assert!(!ids(&EntryQuery { sort: Sort::Date, ..base() }).sql.contains("IS NOT NULL"));
    }

    fn sort_column(sort: Sort) -> &'static str {
        match sort {
            Sort::LastModified => "e.lastModified",
            Sort::LastUserModified => "e.lastUserModified",
            _ => unreachable!(),
        }
    }

    // --- ordering ----------------------------------------------------------

    #[test]
    fn every_sort_ends_in_the_id_tiebreaker_except_the_id_sort() {
        for sort in [
            Sort::Date,
            Sort::Title,
            Sort::Link,
            Sort::Length,
            Sort::LastModified,
            Sort::FeedName,
        ] {
            let sql = ids(&EntryQuery { sort, ..base() }).sql;
            assert!(
                sql.ends_with(&format!("ORDER BY {} DESC, e.id DESC LIMIT ?", order_head(sort))),
                "{sort:?}: {sql}"
            );
        }
        // The category sort puts its internal feed-name secondary in between.
        let sql = ids(&EntryQuery {
            sort: Sort::CategoryName,
            ..base()
        })
        .sql;
        assert!(
            sql.ends_with("ORDER BY c.name DESC, f.name DESC, e.id DESC LIMIT ?"),
            "{sql}"
        );
        // Sorting by id already is the tie-breaker; adding it again would be `e.id DESC, e.id DESC`.
        let sql = ids(&base()).sql;
        assert!(sql.ends_with("ORDER BY e.id DESC LIMIT ?"), "{sql}");
    }

    fn order_head(sort: Sort) -> &'static str {
        match sort {
            Sort::Date => "e.date",
            Sort::Title => "e.title",
            Sort::Link => "e.link",
            Sort::Length => "LENGTH(e.content)",
            Sort::LastModified => "e.lastModified",
            Sort::FeedName => "f.name",
            Sort::CategoryName => "c.name",
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_category_sort_carries_an_internal_secondary_before_the_readers_own() {
        let built = ids(&EntryQuery {
            sort: Sort::CategoryName,
            secondary_sort: SecondarySort::Title,
            secondary_order: Order::Asc,
            ..base()
        });
        assert!(
            built.sql.contains("ORDER BY c.name DESC, f.name DESC, e.title ASC, e.id DESC"),
            "{}",
            built.sql
        );
    }

    #[test]
    fn the_secondary_sort_only_applies_where_ties_are_common() {
        // Sorting by date already ends in a unique key, so a secondary sort would change nothing and
        // only cost the reader a control that appears to do something.
        let built = ids(&EntryQuery {
            sort: Sort::Date,
            secondary_sort: SecondarySort::Title,
            ..base()
        });
        assert!(!built.sql.contains("e.title"), "{}", built.sql);
    }

    #[test]
    fn the_random_sort_has_no_tiebreaker() {
        // `RANDOM(), e.id` would defeat the point, and the sort cannot be resumed anyway.
        let sql = ids(&EntryQuery { sort: Sort::Rand, ..base() }).sql;
        assert!(sql.contains("ORDER BY RANDOM()"), "{sql}");
        assert!(!sql.contains("RANDOM() DESC, e.id"), "{sql}");
    }

    // --- the late row lookup ----------------------------------------------

    #[test]
    fn the_outer_query_repeats_the_order_by() {
        // The join back to `entry` discards the inner order, so without this the page arrives sorted by
        // id whatever the reader asked for.
        let built = rows(&EntryQuery {
            sort: Sort::Title,
            ..base()
        });
        assert_eq!(built.sql.matches("ORDER BY").count(), 2, "{}", built.sql);
        assert!(built.sql.contains("INNER JOIN (SELECT e.id"), "{}", built.sql);
        assert!(built.sql.ends_with("ORDER BY e0.title DESC, e0.id DESC"), "{}", built.sql);
    }

    #[test]
    fn the_outer_query_selects_the_wide_columns_once() {
        let built = rows(&base());
        for column in ["title", "author", "content", "link", "date", "tags", "attributes"] {
            assert!(built.sql.contains(&format!("e0.{column}")), "{column}: {}", built.sql);
        }
        // And the inner query does not, which is the whole point of the shape.
        let inner = ids(&base());
        assert!(!inner.sql.contains("content"), "{}", inner.sql);
    }

    #[test]
    fn the_name_sorts_join_their_tables_on_the_outside_too() {
        let category = rows(&EntryQuery {
            sort: Sort::CategoryName,
            ..base()
        });
        assert!(category.sql.contains("INNER JOIN feed f0"), "{}", category.sql);
        assert!(
            category.sql.contains("INNER JOIN category c0 ON c0.id = f0.category"),
            "{}",
            category.sql
        );

        let feed = rows(&EntryQuery {
            sort: Sort::FeedName,
            ..base()
        });
        assert!(feed.sql.contains("INNER JOIN feed f0"), "{}", feed.sql);
        assert!(!feed.sql.contains("category c0"), "{}", feed.sql);

        let plain = rows(&base());
        assert!(!plain.sql.contains("f0"), "{}", plain.sql);
    }

    #[test]
    fn the_outer_query_binds_nothing_of_its_own() {
        // Everything the outer statement needs was already bound by the inner one; a second parameter
        // list would be off by however many the inner had.
        let query = EntryQuery {
            sort: Sort::Date,
            id_max: Some(EntryId::from_unix(NOW)),
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: vec![CursorValue::Int(NOW)],
            }),
            ..base()
        };
        assert_eq!(rows(&query).params, ids(&query).params);
    }

    // --- the search fragment ----------------------------------------------

    #[test]
    fn a_search_is_parenthesized_and_bound_after_the_fences() {
        let expr = parse("intitle:rust f:3", NOW).unwrap();
        let q = EntryQuery {
            search: Some(expr.clone()),
            id_max: Some(EntryId::from_micros(9)),
            ..base()
        };
        let built = ids(&q);
        assert!(
            built.sql.contains("AND ((instr(e.title_folded, ?) > 0) AND (e.id_feed IN (?)))"),
            "{}",
            built.sql
        );
        // The fence comes first, so the search's own parameters follow it.
        assert_eq!(built.params[2], SqlValue::Int(9));
        assert_eq!(built.params[3], SqlValue::Text("rust".into()));
        assert_eq!(built.params[4], SqlValue::Int(3));
    }

    #[test]
    fn an_empty_search_adds_nothing() {
        let expr = Expr::TRUE;
        let bare = ids(&base());
        let with_empty = ids(&EntryQuery {
            search: Some(expr.clone()),
            ..base()
        });
        assert_eq!(bare.sql, with_empty.sql);
        assert_eq!(bare.params, with_empty.params);
    }

    #[test]
    fn an_unexpanded_saved_query_is_refused() {
        // `expand` runs before this; a reference that reached the builder would otherwise compile to an
        // error the API maps to a 500.
        let expr = Expr::pred(Pred::SavedQuery {
            target: freshrss_search::ast::QueryRef::Ids(freshrss_search::ast::IdSet::Any),
            negated: false,
        });
        let e = EntryQuery {
            search: Some(expr.clone()),
            ..base()
        }
        .build_ids()
        .expect_err("must not compile");
        assert!(matches!(e, Error::Search(_)), "{e}");
    }

    // --- validation --------------------------------------------------------

    #[test]
    fn a_scoped_query_without_its_id_is_rejected() {
        for scope in [ListType::Category, ListType::Feed, ListType::Tag] {
            let e = err(&EntryQuery { scope, ..base() });
            assert!(matches!(&e, Error::Validation(m) if m.contains("needs an id")), "{e}");
        }
    }

    #[test]
    fn an_id_the_scope_does_not_use_is_rejected() {
        let e = err(&EntryQuery {
            scope: ListType::MainStream,
            scope_id: 3,
            ..base()
        });
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("does not take an id")),
            "{e}"
        );
    }

    #[test]
    fn a_random_sort_cannot_be_resumed() {
        let e = err(&EntryQuery {
            sort: Sort::Rand,
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: Vec::new(),
            }),
            ..base()
        });
        assert!(matches!(&e, Error::Validation(m) if m.contains("cannot be resumed")), "{e}");
    }

    #[test]
    fn a_cursor_with_the_wrong_arity_is_rejected() {
        for (sort, values) in [
            (Sort::Date, 0),
            (Sort::Date, 2),
            (Sort::FeedName, 1),
            (Sort::FeedName, 3),
            (Sort::CategoryName, 2),
            (Sort::CategoryName, 4),
        ] {
            let e = err(&EntryQuery {
                sort,
                secondary_sort: SecondarySort::Title,
                cursor: Some(Cursor {
                    id: EntryId::from_micros(1),
                    values: vec![CursorValue::Int(0); values],
                }),
                ..base()
            });
            assert!(
                matches!(&e, Error::Validation(m) if m.contains("sort values")),
                "{sort:?} with {values}: {e}"
            );
        }
        // The id sort carries no values at all, so an empty list is right.
        assert!(ids(&EntryQuery {
            cursor: Some(Cursor {
                id: EntryId::from_micros(1),
                values: Vec::new()
            }),
            ..base()
        })
        .sql
        .contains("e.id < ?"));
    }

    // --- placeholders ------------------------------------------------------

    #[test]
    fn placeholders_and_parameters_line_up_for_every_shape() {
        let expr = parse("intitle:rust labels:todo f:3,4", NOW).unwrap();
        let queries = vec![
            base(),
            EntryQuery {
                scope: ListType::AnyTag,
                ..base()
            },
            EntryQuery {
                scope: ListType::Tag,
                scope_id: 2,
                ..base()
            },
            EntryQuery {
                scope: ListType::StarredOrTagged,
                ..base()
            },
            EntryQuery {
                state: EntryState::unread_or_starred(),
                ..base()
            },
            EntryQuery {
                search: Some(expr.clone()),
                ..base()
            },
            EntryQuery {
                sort: Sort::CategoryName,
                secondary_sort: SecondarySort::Date,
                cursor: Some(Cursor {
                    id: EntryId::from_micros(1),
                    values: vec![
                        CursorValue::Text("a".into()),
                        CursorValue::Text("b".into()),
                        CursorValue::Int(2),
                    ],
                }),
                ..base()
            },
            EntryQuery {
                sort: Sort::LastModified,
                id_min: Some(EntryId::from_micros(1)),
                id_max: Some(EntryId::from_micros(2)),
                ..base()
            },
        ];
        for query in &queries {
            for built in [ids(query), rows(query), query.build_unread_count().unwrap()] {
                assert_eq!(
                    built.sql.matches('?').count(),
                    built.params.len(),
                    "{}\n{:?}",
                    built.sql,
                    built.params
                );
            }
        }
    }

    #[test]
    fn a_text_needle_reaches_the_sql_as_a_parameter_never_as_a_literal() {
        // The search string is untrusted; the only thing standing between it and an injection is that
        // every value here is a `?`.
        let expr = parse(r#"intitle:"'; DROP TABLE entry; --""#, NOW).unwrap();
        let built = ids(&EntryQuery {
            search: Some(expr.clone()),
            ..base()
        });
        assert!(
            !built.sql.contains("DROP"),
            "the needle reached the SQL text: {}",
            built.sql
        );
        // Folded, because that is what a containment needle is — which is itself worth knowing: the
        // statement text still never sees it.
        assert!(
            built.params.iter().any(|p| matches!(p, SqlValue::Text(t) if t.contains("drop table entry"))),
            "{:?}",
            built.params
        );
    }

    #[test]
    fn building_is_deterministic() {
        let expr = parse("intitle:rust f:1", NOW).unwrap();
        let q = EntryQuery {
            sort: Sort::Title,
            search: Some(expr.clone()),
            ..base()
        };
        assert_eq!(ids(&q), ids(&q));
        assert_eq!(rows(&q), rows(&q));
    }

    #[test]
    fn a_cursor_survives_a_json_round_trip() {
        // The API hands the reader an opaque cursor and gets it back, so its wire form has to be
        // stable and its numbers have to stay numbers.
        let cursor = Cursor {
            id: EntryId::from_micros(1_700_000_000_123_456),
            values: vec![
                CursorValue::Text("News".into()),
                CursorValue::Int(42),
            ],
        };
        let json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(serde_json::from_str::<Cursor>(&json).unwrap(), cursor);
        assert!(
            json.contains("1700000000123456"),
            "entry ids are strings in the API envelope but the cursor is opaque: {json}"
        );
    }
}
