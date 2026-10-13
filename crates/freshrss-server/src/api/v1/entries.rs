//! The reading endpoints: the list, the article, and the four mutations the reader makes most.
//!
//! These are deliberately thin. Parsing the request into an [`EntryQuery`] is the only real work; the
//! listing builder owns the SQL and [`crate::service::entries`] owns the rendering. That is what lets
//! `mark-all-read` accept exactly the same filters as the list — it builds the same query and updates
//! the ids it selects instead of fetching them.

use axum::{
    extract::{Path as PathParam, Query, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::api::dto::{EntryDetailDto, EntryPageDto, LabelRefDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::entry_query::{self, count_unread, list, Cursor, EntryQuery};
use crate::db::dao::{entry as entry_dao, feed as feed_dao, tag as tag_dao};
use crate::domain::attributes::keys;
use crate::domain::ids::{now_micros, EntryId};
use crate::domain::list_type::ListType;
use crate::domain::sort::{Order, SecondarySort, Sort};
use crate::domain::state::{Cond, EntryState};
use crate::error::{Error, Result};
use crate::router::AppState;
use crate::service::entries as entry_service;
use crate::service::ingest::Sanitize;
use crate::service::refresh::{fulltext, Settings};
use freshrss_search::ast::Expr;

/// The largest page the API will serve.
///
/// Above this a request stops being a page and starts being an export, and the response stops fitting
/// comfortably in the reader's scroll buffer.
const MAX_LIMIT: usize = 500;

const DEFAULT_LIMIT: usize = 50;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_entries))
        .route("/read", post(mark_read))
        .route("/unread", post(mark_unread))
        .route("/star", post(star))
        .route("/unstar", post(unstar))
        .route("/mark-all-read", post(mark_all_read))
        .route("/{id}", get(show))
        .route("/{id}/fulltext", post(fulltext))
        .route("/{id}/labels", post(add_label))
        .route("/{id}/labels/{name}", delete(remove_label))
}

/// The `GET /entries` query string.
///
/// Everything is a string and parsed by hand rather than deserialized straight into the domain types,
/// so a bad `sort=colour` produces "unknown sort" and a `400` instead of a serde message naming a Rust
/// enum variant.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    /// One of [`ListType::ALL`]'s scope names.
    pub scope: Option<String>,
    /// The category, feed or tag id the scope needs.
    pub id: Option<String>,
    /// `unread`, `read` or `any` (the default).
    pub read: Option<String>,
    /// `yes`, `no` or `any` (the default).
    pub starred: Option<String>,
    /// Widens rather than narrows: "unread, or starred" instead of "unread and starred".
    pub or_unread: Option<bool>,
    pub or_starred: Option<bool>,
    /// BooleanSearch text. A parse failure is a `400` with the code `bad_search`.
    pub search: Option<String>,
    pub sort: Option<String>,
    pub order: Option<String>,
    pub secondary_sort: Option<String>,
    pub secondary_order: Option<String>,
    /// Snapshot fence. Defaults to now, so articles arriving mid-scroll cannot shift the pages.
    pub id_max: Option<String>,
    pub id_min: Option<String>,
    /// Opaque; pass back the value the previous page returned.
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl ListParams {
    /// Builds the listing request. `now` is injected so a page and its `id_max` fence agree.
    fn into_query(self, id_user: i64, search: Option<Expr>, now: i64) -> Result<EntryQuery> {
        let scope = match self.scope.as_deref() {
            None | Some("") | Some("main") => ListType::MainStream,
            Some(text) => ListType::parse(text).ok_or_else(|| {
                Error::Validation(format!(
                    "{text:?} is not a scope; expected one of {}",
                    ListType::ALL
                        .iter()
                        .map(|t| t.as_scope())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?,
        };
        let scope_id = parse_id(self.id.as_deref(), "id")?.unwrap_or(0);
        if scope.needs_id() && scope_id == 0 {
            return Err(Error::Validation(format!(
                "scope {} needs an id",
                scope.as_scope()
            )));
        }

        let limit = self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let sort = match self.sort.as_deref() {
            None | Some("") => Sort::default(),
            Some(text) => Sort::parse(text)
                .ok_or_else(|| Error::Validation(format!("{text:?} is not a sort")))?,
        };
        let order = parse_order(self.order.as_deref(), "order")?;
        let secondary_sort = match self.secondary_sort.as_deref() {
            None | Some("") => SecondarySort::default(),
            Some(text) => SecondarySort::parse(text)
                .ok_or_else(|| Error::Validation(format!("{text:?} is not a secondary sort")))?,
        };
        let secondary_order = parse_order(self.secondary_order.as_deref(), "secondary_order")?;

        let cursor = match self.cursor.as_deref() {
            None | Some("") => None,
            Some(text) => Some(entry_service::decode_cursor(text)?),
        };

        Ok(EntryQuery {
            id_user,
            scope,
            scope_id,
            state: state_from(&self)?,
            search,
            id_min: parse_id(self.id_min.as_deref(), "id_min")?.map(EntryId::from_micros),
            // The fence defaults to now rather than to "no fence": without it an article ingested
            // between two pages becomes the newest in the scope and every following page shifts by one.
            id_max: Some(
                parse_id(self.id_max.as_deref(), "id_max")?
                    .map_or(EntryId::from_micros(now), EntryId::from_micros),
            ),
            sort,
            order,
            secondary_sort,
            secondary_order,
            cursor,
            limit,
        })
    }
}

/// Reads the `read`/`starred`/`or_*` parameters into an [`EntryState`].
///
/// `read` and `starred` narrow; the two `or_` flags widen, and they land in a different group because
/// "unread or starred" is not "unread and starred". Mixing the two would silently turn the reader's
/// default view into an empty one.
fn state_from(params: &ListParams) -> Result<EntryState> {
    let mut state = EntryState::ALL;

    match normalized(params.read.as_deref()).as_str() {
        "" | "any" | "all" => {}
        "unread" | "no" | "0" | "false" => state.ands.push(Cond::NotRead),
        "read" | "yes" | "1" | "true" => state.ands.push(Cond::Read),
        other => {
            return Err(Error::Validation(format!(
                "{other:?} is not a read filter; expected unread, read or any"
            )))
        }
    }

    match normalized(params.starred.as_deref()).as_str() {
        "" | "any" | "all" => {}
        "yes" | "true" | "1" | "starred" => state.ands.push(Cond::Favorite),
        "no" | "false" | "0" | "unstarred" => state.ands.push(Cond::NotFavorite),
        other => {
            return Err(Error::Validation(format!(
                "{other:?} is not a starred filter; expected yes, no or any"
            )))
        }
    }

    if params.or_unread == Some(true) {
        state.ors.push(Cond::NotRead);
    }
    if params.or_starred == Some(true) {
        state.ors.push(Cond::Favorite);
    }
    Ok(state)
}

/// Lowercases and trims an optional string parameter, mapping `None` to the empty string so the match
/// arms below can treat "absent" and "explicitly anything" as the same answer.
fn normalized(text: Option<&str>) -> String {
    text.unwrap_or_default().trim().to_ascii_lowercase()
}

fn parse_order(text: Option<&str>, field: &str) -> Result<Order> {
    match text {
        None | Some("") => Ok(Order::default()),
        Some(value) => Order::parse(value).ok_or_else(|| {
            Error::Validation(format!(
                "{value:?} is not a direction for {field}; expected asc or desc"
            ))
        }),
    }
}

/// Parses an id parameter. Entry ids exceed 2^53, so the API takes and returns them as strings.
fn parse_id(text: Option<&str>, field: &str) -> Result<Option<i64>> {
    match text.map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| Error::Validation(format!("{value:?} is not a number for {field}"))),
    }
}

/// Parses and expands a `search=` parameter, or returns `None` for no search at all.
///
/// Done before anything touches the database, so a malformed query is a cheap `400` rather than a
/// wasted connection.
async fn compile_search(
    text: Option<&str>,
    id_user: i64,
    state: &AppState,
) -> ApiResult<Option<Expr>> {
    match text.map(str::trim) {
        None | Some("") => Ok(None),
        Some(q) => Ok(Some(
            crate::service::search::compile(q, id_user, &state.db)
                .await
                .map_err(ApiError)?,
        )),
    }
}

/// Parses an entry id out of a path segment.
fn entry_id(raw: String) -> Result<EntryId> {
    let micros: i64 = raw
        .trim()
        .parse()
        .map_err(|_| Error::Validation(format!("{raw:?} is not an entry id")))?;
    Ok(EntryId::from_micros(micros))
}

pub async fn list_entries(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<EntryPageDto>> {
    let id_user = user.id;
    let now = now_micros();

    // The search is parsed before the database is touched, so a malformed one is a cheap 400.
    let parsed = match params.search.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(crate::service::search::compile(text, id_user, &state.db).await?),
    };
    let query = params.into_query(id_user, parsed, now).map_err(ApiError)?;

    let page = state
        .db
        .with(move |mut conn| async move {
            // One extra row, so `has_more` does not need a second query.
            let mut probe = query.clone();
            probe.limit = query.limit.saturating_add(1);

            let rows = list(&mut conn, &probe).await?;
            let has_more = rows.len() > query.limit;
            let rows = if has_more {
                rows.into_iter().take(query.limit).collect()
            } else {
                rows
            };
            let unread = count_unread(&mut conn, &query).await?;

            let cursor = match (has_more, rows.last()) {
                (true, Some(last)) => {
                    let cursor = entry_query::cursor_after(&mut conn, &query, last.id).await?;
                    Some(entry_service::encode_cursor(&cursor)?)
                }
                _ => None,
            };

            let items = entry_service::summaries(&mut conn, id_user, &rows).await?;
            Ok(EntryPageDto {
                items,
                cursor,
                has_more,
                id_max: query.id_max.unwrap_or_else(|| EntryId::from_micros(now)),
                unread_in_scope: unread,
            })
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(page))
}

pub async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<String>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<EntryDetailDto>> {
    let id_user = user.id;
    let id = entry_id(id).map_err(ApiError)?;
    let now = now_micros();

    let parsed = compile_search(params.search.as_deref(), id_user, &state).await?;
    let query = params.into_query(id_user, parsed, now).map_err(ApiError)?;

    let detail = state
        .db
        .with(move |mut conn| async move {
            let entry = entry_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .ok_or_else(|| Error::NotFound(format!("entry {id:?}")))?;
            let (full, feed) = entry_service::full(&mut conn, &entry).await?;

            // The neighbours in *this* listing, not in the database: the reading pane's prev/next
            // buttons have to follow the order the reader is looking at, which a `sort=title` list
            // and a `sort=date` list disagree about.
            let mut next_query = query.clone();
            next_query.limit = 1;
            next_query.cursor = Some(entry_query::cursor_after(&mut conn, &query, id).await?);
            let next = list(&mut conn, &next_query)
                .await?
                .into_iter()
                .next()
                .map(|e| e.id);

            // Reversing every level of the ladder is what "the article before this one" means.
            let mut prev_query = query.clone();
            prev_query.limit = 1;
            prev_query.order = query.order.reverse();
            prev_query.secondary_order = query.secondary_order.reverse();
            prev_query.cursor = Some(Cursor {
                id,
                values: entry_query::cursor_after(&mut conn, &query, id)
                    .await?
                    .values,
            });
            let prev = list(&mut conn, &prev_query)
                .await?
                .into_iter()
                .next()
                .map(|e| e.id);

            Ok(EntryDetailDto {
                entry: full,
                prev_id: prev,
                next_id: next,
                feed,
            })
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(detail))
}

#[derive(Debug, Deserialize)]
pub struct IdsRequest {
    /// Entry ids as strings, because they exceed 2^53.
    pub ids: Vec<String>,
}

/// How many rows a bulk mutation will touch in one statement.
///
/// SQLite's bind-parameter ceiling is far above this, but a request naming a hundred thousand articles
/// is a mistake worth reporting rather than executing.
const MAX_BULK_IDS: usize = 10_000;

async fn mark(
    state: &AppState,
    user: &CurrentUser,
    body: IdsRequest,
    kind: Mark,
) -> ApiResult<Json<MarkedDto>> {
    if body.ids.is_empty() {
        return Ok(Json(MarkedDto { marked: 0 }));
    }
    if body.ids.len() > MAX_BULK_IDS {
        return Err(ApiError(Error::Validation(format!(
            "too many ids: {} is more than {MAX_BULK_IDS}",
            body.ids.len()
        ))));
    }
    let ids = body
        .ids
        .into_iter()
        .map(entry_id)
        .collect::<Result<Vec<_>>>()
        .map_err(ApiError)?;

    let id_user = user.id;
    let marked = state
        .db
        // The writer lock, because this updates `entry`: without it a bulk mark-read and a refresh
        // commit contend for SQLite's single write lock and one of them gets SQLITE_BUSY.
        .with_writer(move |mut conn| async move {
            let n = match kind {
                Mark::Read => entry_dao::mark_read(&mut conn, id_user, &ids, true).await?,
                Mark::Unread => entry_dao::mark_read(&mut conn, id_user, &ids, false).await?,
                Mark::Star => entry_dao::mark_favorite(&mut conn, id_user, &ids, true).await?,
                Mark::Unstar => entry_dao::mark_favorite(&mut conn, id_user, &ids, false).await?,
            };
            Ok(n)
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(MarkedDto { marked }))
}

#[derive(Debug, Clone, Copy)]
enum Mark {
    Read,
    Unread,
    Star,
    Unstar,
}

#[derive(Debug, serde::Serialize)]
pub struct MarkedDto {
    /// How many rows actually changed. Lower than the number of ids sent is normal: another tab may
    /// have got there first, and an id belonging to someone else changes nothing.
    pub marked: usize,
}

pub async fn mark_read(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<IdsRequest>,
) -> ApiResult<Json<MarkedDto>> {
    mark(&state, &user, body, Mark::Read).await
}

pub async fn mark_unread(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<IdsRequest>,
) -> ApiResult<Json<MarkedDto>> {
    mark(&state, &user, body, Mark::Unread).await
}

pub async fn star(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<IdsRequest>,
) -> ApiResult<Json<MarkedDto>> {
    mark(&state, &user, body, Mark::Star).await
}

pub async fn unstar(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<IdsRequest>,
) -> ApiResult<Json<MarkedDto>> {
    mark(&state, &user, body, Mark::Unstar).await
}

/// `POST /entries/mark-all-read`, the `Shift+A` shortcut.
///
/// Accepts the same filters as the list and updates whatever they select, rather than taking a list of
/// ids from the client. A reader who has scrolled through nine pages has only loaded nine pages; marking
/// "all read" from the client's side would leave everything below the fold unread, which is not what the
/// button says.
#[derive(Debug, Deserialize)]
pub struct MarkAllReadRequest {
    pub scope: Option<String>,
    pub id: Option<String>,
    pub search: Option<String>,
    pub read: Option<String>,
    pub starred: Option<String>,
    pub or_unread: Option<bool>,
    pub or_starred: Option<bool>,
    pub id_max: Option<String>,
    pub id_min: Option<String>,
    /// Also star them. The reader's "mark all read" does not, but a filter action does.
    pub star: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
pub struct MarkAllReadDto {
    pub marked: usize,
}

pub async fn mark_all_read(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(body): Json<MarkAllReadRequest>,
) -> ApiResult<Json<MarkAllReadDto>> {
    let id_user = user.id;
    let now = now_micros();

    let parsed = compile_search(body.search.as_deref(), id_user, &state).await?;
    let params = ListParams {
        scope: body.scope,
        id: body.id,
        read: body.read,
        starred: body.starred,
        or_unread: body.or_unread,
        or_starred: body.or_starred,
        search: None,
        sort: None,
        order: None,
        secondary_sort: None,
        secondary_order: None,
        id_max: body.id_max,
        id_min: body.id_min,
        cursor: None,
        // The sort and the page size are irrelevant to an update, and asking for one row would mark one
        // row; the limit is lifted below.
        limit: None,
    };
    let query = params.into_query(id_user, parsed, now).map_err(ApiError)?;
    let star = body.star.unwrap_or(false);

    let marked = state
        .db
        // The writer lock, because this updates every `entry` row the filters select.
        .with_writer(move |mut conn| async move {
            // Select the ids first. Updating through the same join the listing uses would work, but
            // it would hold a write lock for as long as the sort takes, and the id-select is the
            // part that benefits from the narrow columns anyway.
            let mut select = query.clone();
            select.limit = usize::MAX;
            select.cursor = None;
            let rows = list(&mut conn, &select).await?;
            if rows.is_empty() {
                return Ok(0);
            }
            let ids: Vec<EntryId> = rows.iter().map(|e| e.id).collect();
            let marked = entry_dao::mark_read(&mut conn, id_user, &ids, true).await?;
            if star {
                entry_dao::mark_favorite(&mut conn, id_user, &ids, true).await?;
            }
            Ok(marked)
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(MarkAllReadDto { marked }))
}

#[derive(Debug, Deserialize)]
pub struct AddLabelRequest {
    /// One or more label names. Created if they do not exist yet — labels are per-user, so there is no
    /// shared namespace to protect.
    pub names: Vec<String>,
}

pub async fn add_label(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<String>,
    Json(body): Json<AddLabelRequest>,
) -> ApiResult<Json<Vec<LabelRefDto>>> {
    let id_user = user.id;
    let id = entry_id(id).map_err(ApiError)?;
    let names: Vec<String> = body
        .names
        .into_iter()
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return Err(ApiError(Error::Validation(
            "a label needs a name".to_owned(),
        )));
    }

    let labels = state
        .db
        .with(move |mut conn| async move {
            // Refuse early rather than after creating labels: an id belonging to another user must
            // not be discoverable, and must certainly not be writable.
            if entry_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .is_none()
            {
                return Err(Error::NotFound(format!("entry {id:?}")));
            }
            for name in &names {
                let id_tag = tag_dao::find_or_create(&mut conn, id_user, name).await?;
                tag_dao::tag_entry(&mut conn, id_tag, id.micros(), true).await?;
            }
            // Every label on the article, so the reader replaces its chip list rather than
            // appending to it and getting duplicates when two tabs race.
            Ok(tag_dao::labels_for_entry(&mut conn, id.micros())
                .await?
                .into_iter()
                .map(|t| LabelRefDto {
                    id: t.id,
                    name: t.name,
                })
                .collect())
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(labels))
}

pub async fn remove_label(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam((id, name)): PathParam<(String, String)>,
) -> ApiResult<Json<Vec<LabelRefDto>>> {
    let id_user = user.id;
    let id = entry_id(id).map_err(ApiError)?;

    let labels = state
        .db
        .with(move |mut conn| async move {
            if entry_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .is_none()
            {
                return Err(Error::NotFound(format!("entry {id:?}")));
            }
            if let Some(tag) = tag_dao::find_by_name(&mut conn, id_user, &name).await? {
                tag_dao::untag_entry(&mut conn, tag.id, id.micros()).await?;
            }
            // Removing a label that was not there is not an error: the reader's view and the
            // database can disagree after two tabs, and the wanted end state is the same either way.
            Ok(tag_dao::labels_for_entry(&mut conn, id.micros())
                .await?
                .into_iter()
                .map(|t| LabelRefDto {
                    id: t.id,
                    name: t.name,
                })
                .collect())
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(labels))
}

/// The reading pane's "full article" request.
#[derive(Debug, Deserialize, Default)]
pub struct FulltextRequest {
    /// Overrides the feed's `pathEntries`. Present makes this a preview: nothing is stored.
    #[serde(default)]
    pub selector: Option<String>,
    /// Overrides the feed's `path_entries_filter`.
    #[serde(default)]
    pub filter: Option<String>,
}

/// The body of one article's page, extracted on demand.
#[derive(Debug, Serialize)]
pub struct FulltextDto {
    /// Sanitized HTML, safe for the pane's one `{@html}`.
    pub content_html: String,
    /// How many elements the selector matched; zero never comes back, it is an error instead.
    pub matched: usize,
    /// The page the body came from, after redirects and meta-refresh hops.
    pub url: String,
    /// The selector that matched, so the pane and the settings preview can say what was used.
    pub selector: String,
    /// True when the result was stored on the entry, so a reload shows it without re-fetching.
    pub stored: bool,
}

/// Fetches an article's own page and extracts the body from it.
///
/// The result is stored in the entry's attributes and returned by `GET /entries/{id}` as
/// `full_content`, so asking once is enough: a reload, another device and the next refresh all see
/// it without fetching again. Storing it in an attribute rather than in `content` is what keeps an
/// upstream edit detected at the next refresh from silently discarding what the reader asked for.
pub async fn fulltext(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(raw_id): PathParam<String>,
    body: Option<Json<FulltextRequest>>,
) -> ApiResult<Json<FulltextDto>> {
    let req = body.map(|Json(req)| req).unwrap_or_default();
    let id_user = user.id;
    let id = entry_id(raw_id).map_err(ApiError)?;

    let (entry, feed) = state
        .db
        .with(move |mut conn| async move {
            let entry = entry_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .ok_or_else(|| Error::NotFound(format!("entry {id:?}")))?;
            let feed = feed_dao::find_by_id(&mut conn, entry.id_feed, id_user)
                .await?
                .ok_or_else(|| Error::NotFound(format!("feed {}", entry.id_feed)))?;
            Ok((entry, feed))
        })
        .await
        .map_err(ApiError)?;

    let settings = Settings::from_config(&state.config);
    let sanitize = Sanitize {
        force_https_domains: settings.force_https_domains.clone(),
        max_length: settings.max_content_length,
    };
    let extracted = fulltext::on_demand(
        &*state.fetcher,
        &feed,
        &entry,
        &settings,
        &sanitize,
        req.selector.as_deref(),
        req.filter.as_deref(),
    )
    .await
    .map_err(ApiError)?;
    let Some(extracted) = extracted else {
        let tried = match req.selector.as_deref() {
            Some(selector) => format!("selector {selector:?}"),
            None => format!(
                "any of the default selectors ({})",
                fulltext::DEFAULT_SELECTORS.join(", ")
            ),
        };
        return Err(ApiError(Error::Validation(format!(
            "the article page has no element matching the {tried}"
        ))));
    };

    // An explicit selector is a preview of a configuration the user is still editing; storing it
    // would let a settings screen rewrite articles from under the reader.
    let stored = req.selector.is_none();
    if stored {
        let mut entry = entry;
        entry
            .attributes
            .set_str(keys::FULL_CONTENT, Some(&extracted.html));
        let attributes = entry.attributes.clone();
        state
            .db
            .with_writer(move |mut conn| async move {
                entry_dao::update_attributes(&mut conn, id.micros(), id_user, &attributes).await
            })
            .await
            .map_err(ApiError)?;
    }

    Ok(Json(FulltextDto {
        content_html: extracted.html,
        matched: extracted.matched,
        url: extracted.url,
        selector: extracted.selector,
        stored,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ListParams {
        ListParams {
            scope: None,
            id: None,
            read: None,
            starred: None,
            or_unread: None,
            or_starred: None,
            search: None,
            sort: None,
            order: None,
            secondary_sort: None,
            secondary_order: None,
            id_max: None,
            id_min: None,
            cursor: None,
            limit: None,
        }
    }

    fn build(p: ListParams) -> Result<EntryQuery> {
        p.into_query(7, None, 1_700_000_000_000_000)
    }

    #[test]
    fn the_defaults_are_the_main_stream_newest_first() {
        let q = build(params()).expect("builds");
        assert_eq!(q.id_user, 7);
        assert_eq!(q.scope, ListType::MainStream);
        assert_eq!(q.sort, Sort::Id);
        assert_eq!(q.order, Order::Desc);
        assert_eq!(q.limit, DEFAULT_LIMIT);
        assert!(q.state.is_unconstrained());
        assert!(q.cursor.is_none());
        // The fence defaults to now, so an article arriving mid-scroll cannot shift the pages.
        assert_eq!(q.id_max, Some(EntryId::from_micros(1_700_000_000_000_000)));
        assert_eq!(q.id_min, None);
    }

    #[test]
    fn the_limit_is_clamped_not_trusted() {
        assert_eq!(
            build(ListParams {
                limit: Some(100_000),
                ..params()
            })
            .expect("builds")
            .limit,
            MAX_LIMIT
        );
        assert_eq!(
            build(ListParams {
                limit: Some(0),
                ..params()
            })
            .expect("builds")
            .limit,
            1
        );
    }

    #[test]
    fn scope_names_and_letters_both_resolve() {
        for (text, expected) in [
            ("main", ListType::MainStream),
            ("all", ListType::AllButHidden),
            ("hidden", ListType::IncludingHidden),
            ("important", ListType::Important),
            ("starred", ListType::Starred),
            ("any_tag", ListType::AnyTag),
            ("starred_or_tagged", ListType::StarredOrTagged),
            ("A", ListType::AllButHidden),
            ("Z", ListType::IncludingHidden),
        ] {
            let q = build(ListParams {
                scope: Some(text.to_owned()),
                id: Some("1".to_owned()),
                ..params()
            })
            .expect("builds");
            assert_eq!(q.scope, expected, "{text}");
        }
    }

    #[test]
    fn a_scope_that_needs_an_id_says_so() {
        let e = build(ListParams {
            scope: Some("feed".to_owned()),
            ..params()
        })
        .expect_err("rejected");
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("needs an id")),
            "{e}"
        );
        assert!(e.is_client_error());
    }

    #[test]
    fn an_unknown_scope_or_sort_names_what_was_wrong() {
        let e = build(ListParams {
            scope: Some("everything".to_owned()),
            ..params()
        })
        .expect_err("rejected");
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("not a scope")),
            "{e}"
        );

        let e = build(ListParams {
            sort: Some("colour".to_owned()),
            ..params()
        })
        .expect_err("rejected");
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("not a sort")),
            "{e}"
        );

        let e = build(ListParams {
            order: Some("sideways".to_owned()),
            ..params()
        })
        .expect_err("rejected");
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("direction")),
            "{e}"
        );
    }

    #[test]
    fn ids_are_strings_because_they_exceed_a_js_number() {
        // 2^53 is 9007199254740992; an entry id in microseconds is already past it.
        let big = "1700000000123456";
        let q = build(ListParams {
            id_max: Some(big.to_owned()),
            ..params()
        })
        .expect("builds");
        assert_eq!(q.id_max, Some(EntryId::from_micros(1_700_000_000_123_456)));

        let e = parse_id(Some("not a number"), "id_max").expect_err("rejected");
        assert!(matches!(e, Error::Validation(_)), "{e}");
        assert!(parse_id(None, "id_max").expect("ok").is_none());
        assert!(parse_id(Some("  "), "id_max").expect("ok").is_none());
    }

    #[test]
    fn the_read_filter_maps_onto_the_state_group() {
        for (text, expected) in [
            ("unread", vec![Cond::NotRead]),
            ("read", vec![Cond::Read]),
            ("no", vec![Cond::NotRead]),
            ("yes", vec![Cond::Read]),
            ("any", vec![]),
        ] {
            let q = build(ListParams {
                read: Some(text.to_owned()),
                ..params()
            })
            .expect("builds");
            assert_eq!(q.state.ands, expected, "{text}");
        }
        assert!(
            build(ListParams {
                read: Some("UNREAD".to_owned()),
                ..params()
            })
            .expect("builds")
            .state
            .ands
            .contains(&Cond::NotRead),
            "case-insensitive"
        );

        let e = build(ListParams {
            read: Some("maybe".to_owned()),
            ..params()
        })
        .expect_err("rejected");
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("read filter")),
            "{e}"
        );
    }

    #[test]
    fn the_starred_filter_maps_onto_the_state_group() {
        for (text, expected) in [
            ("yes", vec![Cond::Favorite]),
            ("no", vec![Cond::NotFavorite]),
            ("any", vec![]),
        ] {
            let q = build(ListParams {
                starred: Some(text.to_owned()),
                ..params()
            })
            .expect("builds");
            assert_eq!(q.state.ands, expected, "{text}");
        }
    }

    #[test]
    fn read_and_starred_compose_but_the_or_flags_widen() {
        let q = build(ListParams {
            read: Some("read".to_owned()),
            starred: Some("yes".to_owned()),
            ..params()
        })
        .expect("builds");
        assert_eq!(q.state.ands, vec![Cond::Read, Cond::Favorite]);
        assert!(q.state.ors.is_empty());

        // `or_unread` is the "unread or starred" view: it widens rather than narrowing, so it lands in
        // the other group.
        let q = build(ListParams {
            or_unread: Some(true),
            or_starred: Some(true),
            ..params()
        })
        .expect("builds");
        assert_eq!(q.state, EntryState::unread_or_starred());
    }

    #[test]
    fn a_cursor_is_decoded_and_a_bad_one_is_a_client_error() {
        let cursor = Cursor {
            id: EntryId::from_micros(1_700_000_000_000_000),
            values: Vec::new(),
        };
        let encoded = entry_service::encode_cursor(&cursor).expect("encodes");
        let q = build(ListParams {
            cursor: Some(encoded),
            ..params()
        })
        .expect("builds");
        assert_eq!(q.cursor, Some(cursor));

        let e = build(ListParams {
            cursor: Some("!!!!".to_owned()),
            ..params()
        })
        .expect_err("rejected");
        assert!(
            e.is_client_error(),
            "a stale cursor is a 400, not a 500: {e}"
        );
    }

    #[test]
    fn an_explicit_fence_overrides_the_default() {
        let q = build(ListParams {
            id_max: Some("123".to_owned()),
            ..params()
        })
        .expect("builds");
        assert_eq!(q.id_max, Some(EntryId::from_micros(123)));
    }

    #[test]
    fn entry_ids_in_paths_are_validated() {
        assert_eq!(
            entry_id("1700000000000000".into()).expect("ok").micros(),
            1_700_000_000_000_000
        );
        for bad in ["", "abc", "1.5", "-"] {
            assert!(entry_id(bad.to_owned()).is_err(), "{bad:?}");
        }
    }
}
