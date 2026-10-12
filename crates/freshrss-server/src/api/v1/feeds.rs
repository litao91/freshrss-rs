//! Subscription endpoints, backing `#rd-add-dialog` and the sidebar's feed rows.
//!
//! Creating a feed here only records it; the fetch that discovers the real feed URL behind a
//! website address and pulls the first articles is the refresh pipeline's job.

use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use url::Url;

use crate::api::dto::FeedDto;
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::{category as cat_dao, feed as feed_dao};
use crate::domain::feed::Feed;
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;
use crate::error::Error;
use crate::router::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", get(show).put(update).delete(remove))
        .route("/{id}/move", post(move_to_category))
        .route("/{id}/entries", delete(truncate_entries))
}

/// Normalizes and validates a feed URL.
///
/// Ports `FreshRSS_http_Util::checkUrl`: a bare `example.com` is what people actually paste into
/// the add dialog, so a missing scheme is filled in rather than rejected.
///
/// Only `http` and `https` are accepted. Rejecting the rest here rather than at fetch time keeps
/// `file:`, `gopher:` and friends out of the database entirely, so a later code path that forgets
/// to check cannot be talked into reading a local file.
pub fn normalize_url(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::BadUrl {
            url: raw.to_string(),
            reason: "the address is empty".into(),
        });
    }

    // Fill in the scheme the way a browser's address bar would.
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    let parsed = Url::parse(&candidate).map_err(|err| Error::BadUrl {
        url: raw.to_string(),
        reason: err.to_string(),
    })?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::BadUrl {
                url: raw.to_string(),
                reason: format!("only http and https addresses are supported, got {other:?}"),
            });
        }
    }
    if parsed.host_str().is_none() || parsed.host_str().is_some_and(str::is_empty) {
        return Err(Error::BadUrl {
            url: raw.to_string(),
            reason: "the address has no host".into(),
        });
    }

    Ok(parsed.to_string())
}

#[derive(Deserialize)]
pub struct CreateFeedRequest {
    /// A feed address, or a website address to discover one from.
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Folder to file it under. Omitted means the user's `Uncategorized`.
    #[serde(default)]
    pub category_id: Option<i64>,
    #[serde(default)]
    pub priority: Option<i8>,
    #[serde(default)]
    pub ttl: Option<i32>,
    #[serde(default)]
    pub kind: Option<i16>,
    #[serde(default)]
    pub muted: Option<bool>,
}

#[derive(Deserialize)]
pub struct UpdateFeedRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub category_id: Option<i64>,
    /// Explicit `null` clears the folder, making the feed an orphan.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub clear_category: Option<bool>,
    pub priority: Option<i8>,
    pub ttl: Option<i32>,
    pub kind: Option<i16>,
    pub muted: Option<bool>,
    pub path_entries: Option<String>,
    pub path_entries_filter: Option<String>,
    #[serde(default)]
    pub http_auth: Option<HttpAuthRequest>,
}

#[derive(Deserialize)]
pub struct HttpAuthRequest {
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Distinguishes "field absent" from "field present and null", which `Option<T>` alone cannot do.
///
/// Needed because `category_id: null` means "remove from its folder" while an absent
/// `category_id` means "leave it alone".
fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(deserializer)?.or(Some(true)))
}

#[derive(Deserialize)]
pub struct MoveRequest {
    pub category_id: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<FeedDto>>> {
    let id_user = user.id;
    let dtos = state
        .db
        .with(move |conn| {
            Ok(feed_dao::list(conn, id_user)?
                .iter()
                .map(FeedDto::new)
                .collect())
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dtos))
}

pub async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<CreateFeedRequest>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    let url = normalize_url(&req.url)?;
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        // Fall back to the host so the sidebar is never blank before the first fetch fills in the
        // feed's real title — the same fallback `FreshRSS_Feed::name()` uses.
        .map(str::to_string)
        .unwrap_or_else(|| host_of(&url));

    let kind = match req.kind {
        Some(k) => FeedKind::from_i16(k).map_err(|e| crate::error::validation(e.to_string()))?,
        None => FeedKind::default(),
    };
    let priority = req.priority.map(Priority::new).unwrap_or_default();
    let ttl = Ttl::new(req.ttl.unwrap_or(0), req.muted.unwrap_or(false));
    let requested_category = req.category_id;

    let dto = state
        .db
        .with(move |conn| {
            if feed_dao::find_by_url(conn, id_user, &url)?.is_some() {
                return Err(Error::Conflict(
                    "you are already subscribed to that address".into(),
                ));
            }

            // Resolve the folder: an explicit id must belong to this user, otherwise the request
            // would file the feed under someone else's folder (or fail the foreign key).
            let category = match requested_category {
                Some(id) => {
                    let _ = cat_dao::find_by_id(conn, id, id_user)?
                        .ok_or_else(|| Error::NotFound(format!("folder {id}")))?;
                    Some(id)
                }
                None => crate::db::dao::user::default_category_id(conn, id_user)?,
            };

            let feed = Feed {
                category,
                kind,
                priority,
                ttl,
                ..Feed::new(id_user, url.clone(), name)
            };
            let id = feed_dao::insert(conn, &feed)?;
            let stored = feed_dao::find_by_id(conn, id, id_user)?
                .ok_or_else(|| Error::Internal("feed vanished after insert".into()))?;
            Ok(FeedDto::new(&stored))
        })
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(dto)))
}

pub async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<FeedDto>> {
    let id_user = user.id;
    let dto = state
        .db
        .with(move |conn| Ok(FeedDto::new(&require_feed(conn, id, id_user)?)))
        .await
        .map_err(ApiError)?;
    Ok(Json(dto))
}

pub async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
    Json(req): Json<UpdateFeedRequest>,
) -> ApiResult<Json<FeedDto>> {
    let id_user = user.id;
    let dto = state
        .db
        .with(move |conn| {
            let feed = require_feed(conn, id, id_user)?;

            if let Some(name) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                feed_dao::update_name(conn, id, id_user, name)?;
            }
            if let Some(raw) = req.url.as_deref() {
                let url = normalize_url(raw)?;
                if url != feed.url
                    && feed_dao::find_by_url(conn, id_user, &url)?
                        .is_some_and(|other| other.id != id)
                {
                    return Err(Error::Conflict(
                        "you are already subscribed to that address".into(),
                    ));
                }
                feed_dao::update_url(conn, id, id_user, &url)?;
            }
            if req.clear_category == Some(true) {
                // `None` moves the feed out of its folder; the sidebar then lists it as an orphan.
                feed_dao::update_category(conn, id, id_user, None)?;
            } else if let Some(cat) = req.category_id {
                let _ = cat_dao::find_by_id(conn, cat, id_user)?
                    .ok_or_else(|| Error::NotFound(format!("folder {cat}")))?;
                feed_dao::update_category(conn, id, id_user, Some(cat))?;
            }
            if let Some(priority) = req.priority {
                feed_dao::update_priority(conn, id, id_user, Priority::new(priority))?;
            }
            if let Some(kind) = req.kind {
                let kind = FeedKind::from_i16(kind)
                    .map_err(|e| crate::error::validation(e.to_string()))?;
                feed_dao::update_kind(conn, id, id_user, kind)?;
            }
            // ttl and muted interact: the mute flag is the sign of the stored value, so applying
            // one without regard to the other would silently unmute a feed.
            if req.ttl.is_some() || req.muted.is_some() {
                let seconds = req.ttl.unwrap_or_else(|| feed.ttl.seconds());
                let muted = req.muted.unwrap_or_else(|| feed.ttl.is_muted());
                feed_dao::update_ttl(conn, id, id_user, Ttl::new(seconds, muted))?;
            }
            if req.path_entries.is_some() || req.path_entries_filter.is_some() {
                let path_entries = req
                    .path_entries
                    .as_deref()
                    .or(feed.path_entries.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let filter = req
                    .path_entries_filter
                    .as_deref()
                    .or(feed.path_entries_filter.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                feed_dao::update_scraping(conn, id, id_user, path_entries, filter)?;
            }
            if let Some(auth) = &req.http_auth {
                let mut updated = feed.clone();
                updated.set_http_auth(auth.username.as_deref(), auth.password.as_deref());
                feed_dao::update_http_auth(conn, id, id_user, updated.http_auth.as_deref())?;
            }

            Ok(FeedDto::new(&require_feed(conn, id, id_user)?))
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dto))
}

pub async fn move_to_category(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
    Json(req): Json<MoveRequest>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    state
        .db
        .with(move |conn| {
            require_feed(conn, id, id_user)?;
            if let Some(cat) = req.category_id {
                let _ = cat_dao::find_by_id(conn, cat, id_user)?
                    .ok_or_else(|| Error::NotFound(format!("folder {cat}")))?;
            }
            Ok(feed_dao::update_category(
                conn,
                id,
                id_user,
                req.category_id,
            )?)
        })
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Unsubscribes. Deletes the feed and, via `ON DELETE CASCADE`, every article and label link.
pub async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    state
        .db
        .with(move |conn| {
            require_feed(conn, id, id_user)?;
            Ok(feed_dao::delete(conn, id, id_user)?)
        })
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Empties a feed's articles but keeps the subscription, and resets `lastUpdate` so the next
/// refresh treats the feed as new and re-fetches everything.
pub async fn truncate_entries(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let id_user = user.id;
    let deleted = state
        .db
        .with_writer(move |conn| {
            require_feed(conn, id, id_user)?;
            Ok(feed_dao::truncate_entries(conn, id, id_user)?)
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

/// Loads a feed the caller owns, or 404s.
fn require_feed(conn: &rusqlite::Connection, id: i64, id_user: i64) -> crate::error::Result<Feed> {
    feed_dao::find_by_id(conn, id, id_user)?.ok_or_else(|| Error::NotFound(format!("feed {id}")))
}

fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}
