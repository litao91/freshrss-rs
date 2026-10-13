//! Refresh jobs, and the list of feeds that need attention.
//!
//! A refresh is not a request: it fetches from the network, so it can outlast any timeout a proxy or
//! a browser would tolerate, and the user should be able to close the tab and come back to the
//! answer. So `POST /refresh` registers a job, spawns the run and replies `202` with the id; the
//! client polls `GET /refresh/{id}`. `GET /refresh/status` is the cheap version the top-bar spinner
//! asks on load, so a page reload mid-run picks the job back up instead of starting a second one.

use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::api::dto::{ProblemFeedDto, ProblemsDto, RefreshJobDto, RefreshStatusDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::feed as feed_dao;
use crate::domain::ids::now_unix;
use crate::error::Error;
use crate::router::AppState;
use crate::service::jobs::Begin;
use crate::service::refresh::run::{self, Scope};
use crate::service::refresh::Settings;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(start))
        .route("/status", get(status))
        .route("/{id}", get(job))
}

/// Starts a refresh of everything the caller owns that is due, and returns straight away.
///
/// Pressing it twice does not start two runs: the second request gets the id of the one already
/// running, so a client that double-presses (or two tabs open at once) converge on one job instead
/// of fetching every feed twice.
pub async fn start(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<impl IntoResponse> {
    let id = match state.jobs.begin(user.id, now_unix()) {
        Begin::Running(id) => id,
        Begin::New(job) => {
            let id = job.id.clone();
            spawn_job(state, id.clone(), user.id);
            id
        }
    };
    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": id }))))
}

/// Runs one user's due feeds in the background and records the outcome against `job_id`.
///
/// Detached rather than tracked: a shutdown does not wait for it. That is safe because a refresh's
/// database work is crash-safe by construction — new articles go through `entrytmp`, and every step
/// is one the next refresh repeats or ignores — so killing one part-way leaves something the next
/// run reconciles rather than something corrupt.
fn spawn_job(state: AppState, job_id: String, id_user: i64) {
    tokio::spawn(async move {
        let settings = Settings::from_config(&state.config);
        let result = run::run(
            &state.db,
            &state.fetcher,
            &settings,
            Scope::User(id_user),
            now_unix(),
        )
        .await;
        if let Err(err) = &result {
            tracing::warn!(job = %job_id, %err, "a refresh job could not run");
        }
        state.jobs.finish(&job_id, result, now_unix());
    });
}

/// One job's progress and result.
///
/// A job belonging to someone else is reported as missing rather than forbidden: job ids are random
/// and unguessable, but "exists and is not yours" would still confirm that a stranger is refreshing
/// something, and 404 costs nothing.
pub async fn job(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<String>,
) -> ApiResult<Json<RefreshJobDto>> {
    let found = state
        .jobs
        .get(&id)
        .filter(|job| job.id_user == user.id || user.is_admin)
        .ok_or_else(|| Error::NotFound(format!("refresh job {id}")))?;
    Ok(Json(RefreshJobDto::from(&found)))
}

/// Whether a job is running for this user, for the top-bar spinner on load.
pub async fn status(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<RefreshStatusDto>> {
    Ok(Json(match state.jobs.latest_for(user.id) {
        Some(job) => RefreshStatusDto::from(&job),
        None => RefreshStatusDto::idle(),
    }))
}

/// The feeds behind `#rd-problems-dialog`: the ones whose last check failed, and the ones muted.
///
/// Muted feeds belong here even though muting is often deliberate, because the two ways a feed gets
/// muted are indistinguishable afterwards — the user muted it, or it answered 410 and the refresh
/// pipeline muted it for them. The second one needs surfacing, and there is no way to tell them
/// apart without showing both.
pub async fn problems(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<ProblemsDto>> {
    let id_user = user.id;
    let feeds = state
        .db
        .with(move |mut conn| async move { feed_dao::list_with_problems(&mut conn, id_user).await })
        .await
        .map_err(ApiError)?;

    Ok(Json(ProblemsDto {
        count: feeds.len() as i64,
        feeds: feeds.iter().map(ProblemFeedDto::from).collect(),
    }))
}
