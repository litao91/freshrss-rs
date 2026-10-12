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

use crate::config::AppConfig;
use crate::db::Db;

/// Everything a handler may need. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<AppConfig>,
}

impl AppState {
    pub fn new(db: Db, config: AppConfig) -> Self {
        Self {
            db,
            config: Arc::new(config),
        }
    }
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
    match state
        .db
        .with(|conn| {
            let one: i64 = conn.query_row("SELECT 1", [], |r| r.get(0))?;
            Ok(one)
        })
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "error", "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn version(State(_state): State<AppState>) -> impl IntoResponse {
    let schema: i64 = _state
        .db
        .with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0))
        })
        .await
        .unwrap_or(0);

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

    fn app() -> Router {
        router(AppState::new(
            Db::in_memory().expect("db"),
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
        let (status, body) = get_json(app(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn version_reports_crate_and_schema() {
        let (status, body) = get_json(app(), "/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "freshrss-server");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            body["schema"].as_i64().unwrap(),
            crate::db::migrate::SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn api_routes_take_precedence_over_the_spa_fallback() {
        // The static handler is a `fallback`, so an exact route must win. If this ever returns
        // HTML, the router has been shadowed by static serving.
        let (status, body) = get_json(app(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn unknown_client_routes_fall_through_to_static_serving() {
        // Either the built SPA's index.html (200) or, when nothing is embedded and no static dir
        // exists, the "run npm build" hint (404). What must never happen is a 500.
        let res = app()
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
