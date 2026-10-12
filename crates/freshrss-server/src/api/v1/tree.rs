//! The sidebar tree and the headline counts.

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use sha1::{Digest, Sha1};

use crate::api::dto::{CountsDto, TreeDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::router::AppState;
use crate::service::tree as tree_service;

/// The whole sidebar: folders with their feeds, orphaned feeds, headline counts and the problem
/// badge. One request, so navigating between folders never waits on a second round trip.
pub async fn tree(State(state): State<AppState>, user: CurrentUser) -> ApiResult<Json<TreeDto>> {
    let id_user = user.id;
    let tree = state
        .db
        .with(move |mut conn| async move { tree_service::build(&mut conn, id_user).await })
        .await
        .map_err(ApiError)?;
    Ok(Json(tree))
}

/// Just the counts, for a cheap refresh after a mutation.
///
/// Carries an `ETag` so a reader that re-checks after every mark-as-read does not re-transfer (and
/// re-render) a sidebar that has not changed.
pub async fn counts(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    let id_user = user.id;
    let counts: CountsDto = state
        .db
        .with(move |mut conn| async move { tree_service::counts(&mut conn, id_user).await })
        .await
        .map_err(ApiError)?;

    let body = serde_json::to_vec(&counts).map_err(ApiError::from)?;
    let etag = HeaderValue::from_str(&format!("\"{}\"", hex::encode(Sha1::digest(&body))))
        .expect("hex is always a valid header value");

    let cached = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag.to_str().unwrap_or_default());
    if cached {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    Ok((
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-cache"),
            ),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
        ],
        body,
    )
        .into_response())
}
