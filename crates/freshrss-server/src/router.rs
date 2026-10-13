//! The HTTP router. Mounted by `main.rs`; tests drive it with `tower::ServiceExt::oneshot`.

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde_json::json;
use std::sync::Arc;

use freshrss_feed::http::{Guard, ReqwestFetcher};
use freshrss_feed::Fetcher;

use crate::config::{AppConfig, HttpConfig};
use crate::db::Db;
use crate::service::jobs::Jobs;

/// Everything a handler may need. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<AppConfig>,
    /// How feed bodies are fetched. Shared by every request and by the scheduler, so the connection
    /// pool and the network policy are the same everywhere.
    pub fetcher: Arc<dyn Fetcher>,
    /// The refresh jobs `POST /refresh` starts and `GET /refresh/{id}` reports on.
    pub jobs: Arc<Jobs>,
}

impl AppState {
    /// Builds the real thing: a `reqwest` fetcher obeying the configured network guard.
    ///
    /// # Panics
    ///
    /// If `reqwest` cannot build a client at all, which means a TLS backend problem and is fatal at
    /// startup rather than something to recover from per request.
    pub fn new(db: Db, config: AppConfig) -> Self {
        let fetcher = ReqwestFetcher::with_guard(guard_from(&config.http))
            .expect("building the HTTP client does not perform IO");
        Self::with_fetcher(db, config, Arc::new(fetcher))
    }

    /// As [`AppState::new`] with a caller-supplied fetcher, so tests and the CLI can drive the whole
    /// API against fixtures instead of the network.
    pub fn with_fetcher(db: Db, config: AppConfig, fetcher: Arc<dyn Fetcher>) -> Self {
        Self {
            db,
            config: Arc::new(config),
            fetcher,
            jobs: Arc::new(Jobs::default()),
        }
    }
}

/// The network policy for outbound feed fetches.
///
/// An allowlist entry that parses as neither a host, an address nor a network is dropped rather than
/// being fatal, so this is where the operator is told about it — once, at startup, rather than as a
/// mysterious refusal on every refresh of the feed it was meant to permit.
fn guard_from(http: &HttpConfig) -> Guard {
    for entry in Guard::rejected(&http.internal_host_allowlist) {
        tracing::warn!(
            entry = %entry,
            "ignoring an internal_host_allowlist entry that is not a host, an address or a network"
        );
    }
    Guard::new(http.block_private_networks, &http.internal_host_allowlist)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .nest("/api/v1", crate::api::v1::router())
        .merge(crate::static_files::router())
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    // A live database round-trip, not just a 200: `healthz` is what a container orchestrator
    // probes, so it must fail when the store is unusable.
    let probe = state
        .db
        .with(|mut conn| async move {
            let one: i64 = sqlx::query_scalar("SELECT 1").fetch_one(&mut *conn).await?;
            Ok(one)
        })
        .await;
    match probe {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "error", "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn version(State(state): State<AppState>) -> impl IntoResponse {
    let schema: i64 = state.db.schema_version().await.unwrap_or(0);

    (
        StatusCode::OK,
        [(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        )],
        Json(json!({
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "schema": schema,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn app() -> Router {
        router(AppState::new(
            Db::in_memory().await.expect("db"),
            AppConfig::default(),
        ))
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .expect("request");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn healthz_reports_ok() {
        let (status, body) = get_json(app().await, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn version_reports_crate_and_schema() {
        let (status, body) = get_json(app().await, "/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "freshrss-server");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            body["schema"].as_i64().unwrap(),
            crate::db::bundled_schema_version()
        );
    }

    #[tokio::test]
    async fn api_routes_take_precedence_over_the_spa_fallback() {
        // The static handler is a `fallback`, so an exact route must win. If this ever returns
        // HTML, the router has been shadowed by static serving.
        let (status, body) = get_json(app().await, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn unknown_client_routes_fall_through_to_static_serving() {
        // Either the built SPA's index.html (200) or, when nothing is embedded and no static dir
        // exists, the "run npm build" hint (404). What must never happen is a 500.
        let res = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/reader?feed=36")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert!(
            matches!(res.status(), StatusCode::OK | StatusCode::NOT_FOUND),
            "unexpected status {}",
            res.status()
        );
    }
}
