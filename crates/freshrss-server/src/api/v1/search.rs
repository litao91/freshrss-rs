//! The search tooling endpoints.
//!
//! These are cheap and they are the verification tools for the whole grammar. `parse` shows the reader
//! what the query they typed actually means — the canonical text round-trips, so a saved search cannot
//! silently change meaning later — and `preview` answers "does this article match, and which part of the
//! query decided it", which is the only way to debug a filter action that marked the wrong thing read.
//!
//! Without them the search grammar is a black box that can only be exercised by typing queries into the
//! list and squinting.

use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

use freshrss_search::ast::Expr;
use freshrss_search::matcher::{matches, SearchTarget};
use freshrss_search::render::render;

use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::{entry as entry_dao, feed as feed_dao, tag as tag_dao};
use crate::domain::entry::Entry;
use crate::domain::ids::EntryId;
use crate::error::{Error, Result};
use crate::router::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/parse", post(parse))
        .route("/preview", post(preview))
}

#[derive(Debug, Deserialize)]
pub struct ParseRequest {
    pub q: String,
}

#[derive(Debug, Serialize)]
pub struct ParseResponse {
    /// The query re-rendered from its own tree. Comparing it with what was typed is how a reader finds
    /// out that `a b` means `a AND b`, or that a saved search expanded.
    pub canonical: String,
    /// The tree itself, for anything that wants to show the structure.
    pub ast: Expr,
    /// Things that are not errors but are worth saying: a query that does not round-trip would change
    /// meaning when saved, and a query that matches everything is usually a typo.
    pub warnings: Vec<String>,
    /// The minimum feed priority the query needs to be satisfiable, which is what lifts a hidden feed
    /// into the listing when the query names it. `null` when the query says nothing about visibility.
    pub min_priority: Option<i8>,
}

pub async fn parse(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<ParseRequest>,
) -> ApiResult<Json<ParseResponse>> {
    let id_user = user.id;
    let text = body.q.clone();
    let expr = crate::service::search::compile(&text, id_user, &state.db)
        .await
        .map_err(ApiError)?;

    let canonical = render(&expr);
    let mut warnings = Vec::new();
    if expr.is_true() && !text.trim().is_empty() {
        warnings.push("this query matches every article".to_owned());
    }
    if expr.is_false() {
        warnings.push("this query cannot match anything".to_owned());
    }
    // A tree that does not survive its own rendering would silently change meaning the moment it is
    // saved, so this says so rather than letting the reader find out later. `round_trip` cannot be used
    // directly: it re-parses the *original* text, and by here saved searches have already been expanded.
    if !canonical.is_empty() {
        let now = crate::domain::ids::now_unix();
        match freshrss_search::parser::parse(&canonical, now) {
            Ok(reparsed) if reparsed != expr => warnings.push(
                "this query does not re-parse to the same tree; saving it would change its meaning"
                    .to_owned(),
            ),
            Ok(_) => {}
            Err(err) => warnings.push(format!("re-parsing this query failed: {err}")),
        }
    }

    Ok(Json(ParseResponse {
        min_priority: freshrss_search::visibility::min_priority(&expr),
        ast: expr,
        canonical,
        warnings,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PreviewRequest {
    pub q: String,
    /// The article to test, as a string because entry ids exceed 2^53.
    pub entry_id: String,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub matches: bool,
    /// One line per predicate in the query, in the order written, saying whether that predicate on its
    /// own matched. The boolean structure is not re-explained — the reader can see it in `canonical`.
    pub explain: Vec<PredExplain>,
}

#[derive(Debug, Serialize)]
pub struct PredExplain {
    /// The operator as the reader wrote it.
    pub operator: String,
    pub matched: bool,
    /// True when the predicate is negated, so `matched` means "the article was *kept*".
    pub negated: bool,
}

pub async fn preview(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<PreviewRequest>,
) -> ApiResult<Json<PreviewResponse>> {
    let id_user = user.id;
    let id: i64 = body
        .entry_id
        .trim()
        .parse()
        .map_err(|_| ApiError(Error::Validation(format!(
            "{:?} is not an entry id",
            body.entry_id
        ))))?;
    let expr = crate::service::search::compile(&body.q, id_user, &state.db)
        .await
        .map_err(ApiError)?;

    let db = state.db.clone();
    let response = tokio::task::spawn_blocking(move || -> Result<PreviewResponse> {
        let conn = db.get()?;
        let entry = entry_dao::find_by_id(&conn, EntryId::from_micros(id), id_user)?
            .ok_or_else(|| Error::NotFound(format!("entry {id}")))?;
        // The folder and the labels are not columns on `entry`, so they have to be resolved before the
        // matcher can answer `c:` and `labels:` at all.
        let category_id = feed_dao::find_by_id(&conn, entry.id_feed, id_user)?
            .and_then(|f| f.category);
        let labels = tag_dao::labels_for_entry(&conn, entry.id.micros())?;
        let target = SearchableEntry::new(entry, category_id, labels);

        let matched = matches(&expr, &target)?;
        // Each predicate on its own. `Expr::pred` rather than slicing the tree, because a predicate's
        // polarity belongs to it and evaluating it in isolation is what "why did this match" means.
        let explain = expr
            .preds()
            .iter()
            .map(|pred| {
                let alone = Expr::pred((*pred).clone());
                PredExplain {
                    operator: render(&alone),
                    negated: pred.is_negated(),
                    matched: matches(&alone, &target).unwrap_or(false),
                }
            })
            .collect();

        Ok(PreviewResponse {
            matches: matched,
            explain,
        })
    })
    .await??;

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// The matcher's view of a stored article
// ---------------------------------------------------------------------------

/// An [`Entry`] plus the two column forms the matcher has to see.
///
/// [`SearchTarget`] hands out `&str`, but the domain type stores authors and tags as `Vec<String>`
/// while both backends compare against the *joined* forms — `"a; b"` and `"#a #b"`, the text SQL reads
/// out of the column. Joining per predicate would be quadratic, so the wrapper joins once.
///
/// It also carries the folder and the labels, which are not columns on `entry` at all: the folder lives
/// on the feed and the labels in `entrytag`. Whoever builds this has to resolve them, and getting that
/// wrong makes `c:` and `labels:` answer a different question than the SQL does.
pub struct SearchableEntry {
    pub entry: Entry,
    author_column: String,
    tags_column: String,
    category_id: Option<i64>,
    label_ids: Vec<i64>,
    label_names: Vec<String>,
}

impl SearchableEntry {
    /// Builds one from a stored entry and the lookups the caller already has.
    pub fn new(
        entry: Entry,
        category_id: Option<i64>,
        labels: Vec<crate::domain::tag::Tag>,
    ) -> Self {
        let author_column = entry.author_to_db();
        let tags_column = entry.tags_to_db();
        let mut label_ids = Vec::with_capacity(labels.len());
        let mut label_names = Vec::with_capacity(labels.len());
        for tag in labels {
            label_ids.push(tag.id);
            label_names.push(tag.name);
        }
        Self {
            entry,
            author_column,
            tags_column,
            category_id,
            label_ids,
            label_names,
        }
    }
}

impl SearchTarget for SearchableEntry {
    fn id(&self) -> Option<i64> {
        Some(self.entry.id.micros())
    }
    fn feed_id(&self) -> i64 {
        self.entry.id_feed
    }
    fn category_id(&self) -> Option<i64> {
        self.category_id
    }
    fn title(&self) -> &str {
        &self.entry.title
    }
    fn content(&self) -> &str {
        &self.entry.content
    }
    fn author(&self) -> &str {
        &self.author_column
    }
    fn link(&self) -> &str {
        &self.entry.link
    }
    fn tags(&self) -> &str {
        &self.tags_column
    }
    fn label_ids(&self) -> &[i64] {
        &self.label_ids
    }
    fn label_names(&self) -> &[String] {
        &self.label_names
    }
    fn date(&self) -> i64 {
        self.entry.date
    }
    fn last_modified(&self) -> Option<i64> {
        self.entry.last_modified
    }
    fn last_user_modified(&self) -> Option<i64> {
        self.entry.last_user_modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attributes::Attributes;
    use crate::domain::ids::EntryId;
    use crate::domain::tag::Tag;

    fn entry() -> Entry {
        let mut e = Entry::new(1, 2, "guid-1");
        e.id = EntryId::from_micros(1_700_000_000_000_000);
        e.title = "l'École des chiens".to_owned();
        e.content = "<p>Body mentioning axum</p>".to_owned();
        e.authors = vec!["Ada Lovelace".to_owned(), "Grace Hopper".to_owned()];
        e.link = "https://example.com/a".to_owned();
        e.tags = vec!["rust".to_owned(), "rss".to_owned()];
        e.date = 1_700_000_000;
        e.last_modified = Some(1_700_000_100);
        e.last_user_modified = None;
        e
    }

    fn tag(id: i64, name: &str) -> Tag {
        Tag {
            id,
            id_user: 1,
            name: name.to_owned(),
            attributes: Attributes::new(),
        }
    }

    fn m(q: &str, target: &SearchableEntry) -> bool {
        let expr = freshrss_search::parser::parse(q, 1_700_000_000).expect("parses");
        matches(&expr, target).expect("evaluates")
    }

    #[test]
    fn the_wrapper_hands_the_matcher_the_column_forms() {
        let s = SearchableEntry::new(entry(), Some(9), vec![tag(3, "todo"), tag(4, "reading")]);
        // The joined forms are what SQL compares against, so the matcher has to see the same text.
        assert_eq!(s.author(), "Ada Lovelace; Grace Hopper");
        assert_eq!(s.tags(), "#rust #rss");
        assert_eq!(s.category_id(), Some(9));
        assert_eq!(s.label_ids(), &[3, 4]);
        assert_eq!(s.label_names(), &["todo".to_owned(), "reading".to_owned()]);
    }

    #[test]
    fn a_stored_entry_can_be_matched() {
        let s = SearchableEntry::new(entry(), None, Vec::new());
        assert!(m("intitle:ecole", &s), "accent-folded on both sides");
        assert!(m("intext:axum", &s));
        assert!(m("author:Hopper", &s));
        assert!(m("inurl:example.com", &s));
        assert!(m("#rust", &s));
        assert!(!m("#ru", &s), "a prefix is not a tag");
        assert!(!m("labels:todo", &s), "no labels attached");
        assert!(!m("c:9", &s), "no folder resolved");
        assert!(m("date:2023-11-14/2023-11-15", &s));
        assert!(m("pubdate:2023-11-14", &s));
    }

    #[test]
    fn a_null_user_timestamp_matches_nothing_in_either_polarity() {
        let s = SearchableEntry::new(entry(), None, Vec::new());
        // SQL cannot compare against NULL, and `NOT (NULL BETWEEN …)` is NULL too, which a WHERE clause
        // drops. The matcher has to agree or the two backends diverge on every never-edited article.
        assert!(!m("userdate:2023-11-14", &s));
        assert!(!m("-userdate:2023-11-14", &s));
        assert!(m("mdate:2023-11-14", &s), "this one was edited upstream");
    }

    #[test]
    fn an_author_needle_may_span_the_separator() {
        // The matcher sees the joined column, exactly as SQL does, so a quoted needle can cross it.
        let s = SearchableEntry::new(entry(), None, Vec::new());
        assert!(m(r#"author:"Lovelace; Grace""#, &s));
        assert!(!m("author:Hopper; Ada", &s), "the order in the column matters");
    }

    #[test]
    fn labels_and_folders_match_once_they_are_resolved() {
        let s = SearchableEntry::new(entry(), Some(9), vec![tag(3, "todo")]);
        assert!(m("labels:todo", &s));
        assert!(m("l:3", &s));
        assert!(m("l:*", &s));
        assert!(m("c:9", &s));
        assert!(!m("labels:tod", &s), "a label name is matched whole");
        assert!(!m("c:10", &s));
    }
}
