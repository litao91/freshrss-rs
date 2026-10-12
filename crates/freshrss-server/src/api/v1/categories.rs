//! Folder (category) endpoints, backing `#rd-folder-dialog` and the sidebar's folder rows.

use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::api::dto::{CategoryDto, FeedDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::{category as cat_dao, feed as feed_dao};
use crate::domain::attributes::Attributes;
use crate::domain::category::Category;
use crate::error::Error;
use crate::router::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/order", put(reorder))
        .route("/{id}", get(show).put(update).delete(remove))
        .route("/{id}/feeds", get(feeds))
        .route("/{id}/position", put(set_position))
}

#[derive(Deserialize)]
pub struct CreateCategoryRequest {
    pub name: String,
}

#[derive(Deserialize)]
pub struct UpdateCategoryRequest {
    /// New name. `None` leaves it unchanged.
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct SetPositionRequest {
    pub position: i64,
}

#[derive(Deserialize)]
pub struct ReorderRequest {
    /// The complete desired sidebar order. Positions are assigned from the slice index, so a
    /// partial list would collide with the folders it omits.
    pub ids: Vec<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<CategoryDto>>> {
    let id_user = user.id;
    let dtos = state
        .db
        .with(move |conn| {
            let categories = cat_dao::list(conn, id_user)?;
            let feeds = feed_dao::list(conn, id_user)?;

            let mut by_category: std::collections::HashMap<i64, Vec<_>> =
                std::collections::HashMap::new();
            for feed in feeds {
                if let Some(cat) = feed.category {
                    by_category.entry(cat).or_default().push(feed);
                }
            }
            Ok(categories
                .iter()
                .map(|c| CategoryDto::new(c, by_category.remove(&c.id).unwrap_or_default()))
                .collect())
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dtos))
}

pub async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<CreateCategoryRequest>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    let name = Category::normalize_name(&req.name).map_err(crate::error::validation)?;

    let dto = state
        .db
        .with(move |conn| {
            // The uniqueness check produces a clear 409. Without it the UNIQUE(id_user, name)
            // constraint would surface as an opaque database error.
            if cat_dao::find_by_name(conn, id_user, &name)?.is_some() {
                return Err(Error::Conflict(format!(
                    "a folder named {name:?} already exists"
                )));
            }
            let position = cat_dao::next_position(conn, id_user)?;
            let id = cat_dao::insert(
                conn,
                &cat_dao::NewCategory {
                    id_user,
                    name: &name,
                    kind: Default::default(),
                    position,
                    attributes: Attributes::new(),
                },
            )?;
            let category = cat_dao::find_by_id(conn, id, id_user)?
                .ok_or_else(|| Error::Internal("folder vanished after insert".into()))?;
            Ok(CategoryDto::new(&category, Vec::new()))
        })
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(dto)))
}

pub async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<CategoryDto>> {
    let id_user = user.id;
    let dto = state
        .db
        .with(move |conn| {
            let category = require_category(conn, id, id_user)?;
            let feeds = feed_dao::list_by_category(conn, id_user, id)?;
            Ok(CategoryDto::new(&category, feeds))
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dto))
}

pub async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
    Json(req): Json<UpdateCategoryRequest>,
) -> ApiResult<Json<CategoryDto>> {
    let id_user = user.id;
    let dto = state
        .db
        .with(move |conn| {
            require_category(conn, id, id_user)?;
            if let Some(raw) = req.name.as_deref() {
                let name = Category::normalize_name(raw).map_err(crate::error::validation)?;
                // Only worth checking when the name is actually changing.
                let existing = cat_dao::find_by_name(conn, id_user, &name)?;
                if existing.is_some_and(|c| c.id != id) {
                    return Err(Error::Conflict(format!(
                        "a folder named {name:?} already exists"
                    )));
                }
                cat_dao::update_name(conn, id, id_user, &name)?;
            }
            let category = require_category(conn, id, id_user)?;
            let feeds = feed_dao::list_by_category(conn, id_user, id)?;
            Ok(CategoryDto::new(&category, feeds))
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dto))
}

pub async fn set_position(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
    Json(req): Json<SetPositionRequest>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    state
        .db
        .with(move |conn| {
            require_category(conn, id, id_user)?;
            Ok(cat_dao::update_position(conn, id, id_user, req.position)?)
        })
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn reorder(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<ReorderRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let id_user = user.id;
    let updated = state
        .db
        .with(move |conn| Ok(cat_dao::reorder(conn, id_user, &req.ids)?))
        .await
        .map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "repositioned": updated })))
}

/// Deletes a folder.
///
/// Its feeds are **not** deleted — `feed.category` is `ON DELETE SET NULL`, so they become orphans
/// and stay in the sidebar. Removing a folder is a filing operation; it should not be able to
/// destroy subscriptions. The response reports how many feeds were orphaned so the UI can say so.
pub async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    let orphaned = state
        .db
        .with(move |conn| {
            require_category(conn, id, id_user)?;
            let orphaned = feed_dao::list_by_category(conn, id_user, id)?.len() as i64;
            cat_dao::delete(conn, id, id_user)?;
            Ok(orphaned)
        })
        .await
        .map_err(ApiError)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": id.to_string(), "orphaned_feeds": orphaned })),
    ))
}

/// Loads a folder the caller owns, or 404s.
///
/// A 404 rather than a 403 for someone else's folder: the distinction would confirm the id exists.
fn require_category(
    conn: &rusqlite::Connection,
    id: i64,
    id_user: i64,
) -> crate::error::Result<Category> {
    cat_dao::find_by_id(conn, id, id_user)?.ok_or_else(|| Error::NotFound(format!("folder {id}")))
}

/// Feeds belonging to a folder, for the sidebar's nested list.
pub async fn feeds(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<Vec<FeedDto>>> {
    let id_user = user.id;
    let dtos = state
        .db
        .with(move |conn| {
            require_category(conn, id, id_user)?;
            Ok(feed_dao::list_by_category(conn, id_user, id)?
                .iter()
                .map(FeedDto::new)
                .collect())
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dtos))
}
