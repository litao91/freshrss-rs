//! Subscription endpoints, backing `#rd-add-dialog` and the sidebar's feed rows.
//!
//! Handlers only translate: the rules about what may be subscribed to, and what happens when the
//! address turns out not to be a feed, live in [`crate::service::subscription`].

use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::api::dto::{deserialize_id, CreatedFeedDto, DiscoveredDto, FeedDto, FeedRefreshDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::{category as cat_dao, feed as feed_dao};
use crate::domain::feed::Feed;
use crate::domain::ids::now_unix;
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;
use crate::error::Error;
use crate::router::AppState;
use crate::service::refresh::{refresh_feed, Settings};
use crate::service::subscription::{self, NewSubscription};
use sqlx::SqliteConnection;

pub use crate::service::subscription::normalize_url;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        // Registered before `/{id}` in the source, but axum's matcher prefers the static segment
        // regardless of order — which is the only reason `POST /feeds/discover` is not swallowed by
        // `/feeds/{id}` and answered with a 405 for a method that path does not have.
        .route("/discover", post(discover))
        .route("/{id}", get(show).put(update).delete(remove))
        .route("/{id}/move", post(move_to_category))
        .route("/{id}/refresh", post(refresh_one))
        .route("/{id}/entries", delete(truncate_entries))
}

#[derive(Deserialize)]
pub struct DiscoverRequest {
    /// A feed address, or a website address to discover one from.
    pub url: String,
}

/// Looks for a feed at an address, or behind the page it serves.
///
/// Fetches from the network — up to eleven requests, one for the address given and one per candidate
/// — so it is the slowest endpoint here and the one a user is most likely to press repeatedly while
/// staring at a dialog that has not answered yet.
pub async fn discover(
    State(state): State<AppState>,
    _user: CurrentUser,
    Json(req): Json<DiscoverRequest>,
) -> ApiResult<Json<DiscoveredDto>> {
    let url = normalize_url(&req.url)?;
    let settings = Settings::from_config(&state.config);
    let found = crate::service::discovery::discover(&*state.fetcher, &settings, &url)
        .await
        .map_err(ApiError)?;
    Ok(Json(DiscoveredDto::from(&found)))
}

#[derive(Deserialize)]
pub struct CreateFeedRequest {
    /// A feed address, or a website address to discover one from.
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Folder to file it under. Omitted or `null` means the user's `Uncategorized`.
    #[serde(default, deserialize_with = "deserialize_id")]
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
    #[serde(default, deserialize_with = "deserialize_id")]
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
    #[serde(default, deserialize_with = "deserialize_id")]
    pub category_id: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<FeedDto>>> {
    let id_user = user.id;
    let dtos = state
        .db
        .with(move |mut conn| async move {
            Ok(feed_dao::list(&mut conn, id_user)
                .await?
                .iter()
                .map(FeedDto::new)
                .collect())
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dtos))
}

/// Subscribes and pulls the first articles in the same request.
///
/// Fetching inline rather than deferring to the next tick is what makes the add dialog able to say
/// "12 new articles" instead of "added", and what makes an address that is not a feed fail here
/// rather than becoming a broken row in the sidebar.
pub async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<CreateFeedRequest>,
) -> ApiResult<impl IntoResponse> {
    let kind = match req.kind {
        Some(k) => FeedKind::from_i16(k).map_err(|e| crate::error::validation(e.to_string()))?,
        None => FeedKind::default(),
    };
    let default_ttl = user.config.ttl_default;
    let spec = NewSubscription {
        id_user: user.id,
        url: normalize_url(&req.url)?,
        name: req.name,
        category: req.category_id,
        kind,
        priority: req.priority.map(Priority::new).unwrap_or_default(),
        ttl: requested_ttl(req.ttl.unwrap_or(0), req.muted.unwrap_or(false), default_ttl),
    };

    let added = subscription::add(
        &state.db,
        &*state.fetcher,
        &Settings::from_config(&state.config),
        &spec,
        now_unix(),
    )
    .await
    .map_err(ApiError)?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedFeedDto {
            feed: FeedDto::new(&added.feed),
            new_entries: added.outcome.new_entries,
        }),
    ))
}

/// Refreshes one feed now, whether or not it is due.
///
/// This is the dialog's per-row "retry", so a muted feed is refreshed too: unmuting and retrying is
/// one action from the user's point of view, and refusing because the ttl is negative would leave
/// them with no way to test whether the feed came back.
pub async fn refresh_one(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<FeedRefreshDto>> {
    let id_user = user.id;
    let feed = state
        .db
        .with(move |mut conn| async move { require_feed(&mut conn, id, id_user).await })
        .await
        .map_err(ApiError)?;

    let outcome = refresh_feed(
        &state.db,
        &*state.fetcher,
        &feed,
        &Settings::from_config(&state.config),
        now_unix(),
    )
    .await
    .map_err(ApiError)?;
    Ok(Json(FeedRefreshDto::from(&outcome)))
}

pub async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<FeedDto>> {
    let id_user = user.id;
    let dto = state
        .db
        .with(move |mut conn| async move {
            Ok(FeedDto::new(&require_feed(&mut conn, id, id_user).await?))
        })
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
    let default_ttl = user.config.ttl_default;
    let dto = state
        .db
        .with(move |mut conn| async move {
            let feed = require_feed(&mut conn, id, id_user).await?;

            if let Some(name) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                feed_dao::update_name(&mut conn, id, id_user, name).await?;
            }
            if let Some(raw) = req.url.as_deref() {
                let url = normalize_url(raw)?;
                if url != feed.url
                    && feed_dao::find_by_url(&mut conn, id_user, &url)
                        .await?
                        .is_some_and(|other| other.id != id)
                {
                    return Err(Error::Conflict(
                        "you are already subscribed to that address".into(),
                    ));
                }
                feed_dao::update_url(&mut conn, id, id_user, &url).await?;
            }
            if req.clear_category == Some(true) {
                // `None` moves the feed out of its folder; the sidebar then lists it as an orphan.
                feed_dao::update_category(&mut conn, id, id_user, None).await?;
            } else if let Some(cat) = req.category_id {
                let _ = cat_dao::find_by_id(&mut conn, cat, id_user)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("folder {cat}")))?;
                feed_dao::update_category(&mut conn, id, id_user, Some(cat)).await?;
            }
            if let Some(priority) = req.priority {
                feed_dao::update_priority(&mut conn, id, id_user, Priority::new(priority)).await?;
            }
            if let Some(kind) = req.kind {
                let kind = FeedKind::from_i16(kind)
                    .map_err(|e| crate::error::validation(e.to_string()))?;
                feed_dao::update_kind(&mut conn, id, id_user, kind).await?;
            }
            // ttl and muted interact: the mute flag is the sign of the stored value, so applying
            // one without regard to the other would silently unmute a feed.
            if req.ttl.is_some() || req.muted.is_some() {
                let seconds = req.ttl.unwrap_or_else(|| feed.ttl.seconds());
                let muted = req.muted.unwrap_or_else(|| feed.ttl.is_muted());
                let ttl = requested_ttl(seconds, muted, default_ttl);
                feed_dao::update_ttl(&mut conn, id, id_user, ttl).await?;
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
                feed_dao::update_scraping(&mut conn, id, id_user, path_entries, filter).await?;
            }
            if let Some(auth) = &req.http_auth {
                let mut updated = feed.clone();
                updated.set_http_auth(auth.username.as_deref(), auth.password.as_deref());
                feed_dao::update_http_auth(&mut conn, id, id_user, updated.http_auth.as_deref())
                    .await?;
            }

            Ok(FeedDto::new(&require_feed(&mut conn, id, id_user).await?))
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
        .with(move |mut conn| async move {
            require_feed(&mut conn, id, id_user).await?;
            if let Some(cat) = req.category_id {
                let _ = cat_dao::find_by_id(&mut conn, cat, id_user)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("folder {cat}")))?;
            }
            feed_dao::update_category(&mut conn, id, id_user, req.category_id).await
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
        // The writer lock: `feed` has `ON DELETE CASCADE` into `entry`, so deleting a subscription
        // is one of the largest entry writes in the system.
        .with_writer(move |mut conn| async move {
            require_feed(&mut conn, id, id_user).await?;
            feed_dao::delete(&mut conn, id, id_user).await
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
        .with_writer(move |mut conn| async move {
            require_feed(&mut conn, id, id_user).await?;
            feed_dao::truncate_entries(&mut conn, id, id_user).await
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

/// Loads a feed the caller owns, or 404s.
async fn require_feed(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
) -> crate::error::Result<Feed> {
    feed_dao::find_by_id(conn, id, id_user)
        .await?
        .ok_or_else(|| Error::NotFound(format!("feed {id}")))
}

/// An interval and a mute flag from a request body, as the one stored integer.
///
/// Muting goes through [`Ttl::mute`] rather than `Ttl::new(seconds, true)`, because a feed left on
/// its owner's default has an interval of 0 and `-0` is `0` — which reads back as *not muted*, so the
/// request would succeed and do nothing. Every feed added through the dialog starts that way.
fn requested_ttl(seconds: i32, muted: bool, default_seconds: i64) -> Ttl {
    let ttl = Ttl::new(seconds, false);
    if muted {
        ttl.mute(i32::try_from(default_seconds).unwrap_or(i32::MAX))
    } else {
        ttl
    }
}
