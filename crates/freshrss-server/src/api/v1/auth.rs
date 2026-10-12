//! Registration, login, logout, password change and session management.

use axum::{
    extract::{Path as PathParam, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::cookie::{self, SameSite};
use crate::api::dto::{SessionDto, UserDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::{is_secure_request, CurrentUser};
use crate::db::dao::session as session_dao;
use crate::error::Error;
use crate::router::AppState;
use crate::service::auth;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(current_user))
        .route("/password", put(change_password))
}

/// Builds the `Set-Cookie` header for a session.
///
/// `SameSite=Strict` rather than `Lax`: the reader has no cross-origin entry points that should
/// arrive with a session attached, so Strict costs nothing and removes CSRF-by-navigation as a
/// class of bug. `Secure` follows the request scheme, because browsers reject a Secure cookie over
/// plain HTTP — setting it unconditionally would silently break every localhost deployment.
fn session_cookie(
    state: &AppState,
    session_id: &str,
    secure: bool,
) -> (header::HeaderName, String) {
    let max_age = i64::from(state.config.session.max_age_days) * 86_400;
    (
        header::SET_COOKIE,
        cookie::build(
            &state.config.session.cookie_name,
            session_id,
            Some(max_age),
            secure,
            SameSite::Strict,
        ),
    )
}

/// Opens a session for an already-authenticated user.
///
/// Registration uses this directly rather than going through [`auth::login`]: the password has
/// just been verified by construction, and re-running the credential check would either fail (the
/// plaintext is gone) or force us to keep it around longer than necessary.
fn open_session(
    state: &AppState,
    id_user: i64,
    headers: &HeaderMap,
) -> crate::error::Result<session_dao::Session> {
    let ttl = i64::from(state.config.session.max_age_days) * 86_400;
    let client = client_info(headers);
    state.db.blocking_with(|conn| {
        Ok(session_dao::create(
            conn,
            id_user,
            ttl,
            client.ip,
            client.user_agent,
        )?)
    })
}

/// The visitor's address and user agent, recorded against the session for the account menu's
/// device list.
fn client_info(headers: &HeaderMap) -> auth::ClientInfo<'_> {
    auth::ClientInfo {
        // `X-Forwarded-For` first, because behind a proxy every peer address is the proxy's.
        ip: headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(|list| list.split(',').next().unwrap_or("").trim())
            .filter(|ip| !ip.is_empty())
            .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok())),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    }
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(req): Json<auth::RegisterRequest>,
) -> ApiResult<impl IntoResponse> {
    let user = auth::register(&state.db, &state.config, req).map_err(ApiError)?;

    // Registering also signs the user in: making them log straight back in is friction with no
    // security benefit, since they just proved control of the password.
    let session = open_session(&state, user.id, &headers).map_err(ApiError)?;

    let (name, value) = session_cookie(&state, &session.id, is_secure_request(&headers, &uri));
    Ok((
        StatusCode::CREATED,
        [(name, value)],
        Json(json!({ "user": UserDto::from(&user) })),
    ))
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(req): Json<auth::LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let client = client_info(&headers);
    let (user, session) = auth::login(&state.db, &state.config, req, client).map_err(ApiError)?;
    let (name, value) = session_cookie(&state, &session.id, is_secure_request(&headers, &uri));
    Ok((
        [(name, value)],
        Json(json!({ "user": UserDto::from(&user) })),
    ))
}

async fn logout(State(state): State<AppState>, user: CurrentUser) -> ApiResult<impl IntoResponse> {
    auth::logout(&state.db, &user.session_id).map_err(ApiError)?;
    let removal = cookie::removal(&state.config.session.cookie_name);
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, removal)]))
}

async fn current_user(user: CurrentUser) -> Json<serde_json::Value> {
    Json(json!({
        "user": {
            "id": user.id.to_string(),
            "username": user.username,
            "is_admin": user.is_admin,
            "initial": user.display_initial(),
        }
    }))
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub current: String,
    #[serde(rename = "next")]
    pub next_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<ChangePasswordRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let revoked = auth::change_password(
        &state.db,
        user.id,
        &req.current,
        &req.next_password,
        // Keep the session that just proved it knows the old password; sign out the rest.
        Some(&user.session_id),
    )
    .map_err(ApiError)?;
    Ok(Json(json!({ "revoked_sessions": revoked })))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<SessionDto>>> {
    let id_user = user.id;
    let current = user.session_id.clone();
    let sessions = state
        .db
        .with(move |conn| {
            let rows = session_dao::list_for_user(conn, id_user)?;
            Ok(rows
                .into_iter()
                .map(|s| SessionDto {
                    current: s.id == current,
                    id: s.id,
                    created_at: s.created_at,
                    expires_at: s.expires_at,
                    ip: s.ip,
                    user_agent: s.user_agent,
                })
                .collect())
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(sessions))
}

pub async fn revoke_session(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<String>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    let id_for_error = id.clone();
    // Scoped to the caller, so an id belonging to someone else is a 404 rather than a 403: a 403
    // would confirm the session exists.
    let revoked = state
        .db
        .with(move |conn| {
            let Some(session) = session_dao::find(conn, &id)? else {
                return Err(Error::NotFound(format!("session {id}")));
            };
            if session.id_user != id_user {
                return Err(Error::NotFound(format!("session {id}")));
            }
            Ok(session_dao::revoke(conn, &id)?)
        })
        .await
        .map_err(ApiError)?;

    if !revoked {
        return Err(ApiError(Error::NotFound(format!("session {id_for_error}"))));
    }
    Ok(StatusCode::NO_CONTENT)
}
