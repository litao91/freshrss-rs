//! Starter-pack endpoints: list the curated packs and install one.

use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::router::AppState;
use crate::service::packages;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{slug}/install", post(install))
}

#[derive(Debug, Serialize)]
pub struct PackFeedDto {
    pub url: String,
    pub title: String,
}

#[derive(Debug, Serialize)]
pub struct PackDto {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub feeds: Vec<PackFeedDto>,
}

impl From<&packages::Pack> for PackDto {
    fn from(pack: &packages::Pack) -> Self {
        Self {
            slug: pack.slug.to_owned(),
            name: pack.name.to_owned(),
            description: pack.description.to_owned(),
            feeds: pack
                .feeds
                .iter()
                .map(|feed| PackFeedDto {
                    url: feed.url.to_owned(),
                    title: feed.title.to_owned(),
                })
                .collect(),
        }
    }
}

/// The registry, for the "add some feeds to get started" screen.
pub async fn list(_user: CurrentUser) -> ApiResult<Json<Vec<PackDto>>> {
    Ok(Json(packages::PACKS.iter().map(PackDto::from).collect()))
}

/// Installs one pack, running it through the same import path as an uploaded OPML document.
///
/// An unknown slug is a 404: the pack is addressed by name in the path, so a typo or a removed pack is
/// "no such thing" rather than a validation error about a body it never sent.
pub async fn install(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(slug): PathParam<String>,
) -> ApiResult<impl IntoResponse> {
    let report = packages::install(&state.db, user.id, &slug, false)
        .await
        .map_err(ApiError)?;
    Ok((StatusCode::OK, Json(report)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use freshrss_feed::http::FixtureFetcher;
    use tower::ServiceExt as _;

    use crate::config::AppConfig;
    use crate::db::Db;
    use crate::router::AppState;
    use crate::service::auth::{self, ClientInfo, LoginRequest};
    use crate::service::packages;

    async fn authed() -> (Router, String, Db) {
        let db = Db::in_memory().await.expect("db");
        let config = AppConfig::default();
        // The policy-free path: these tests are about starter packs, not registration.
        auth::create_user(&db, "alice", None, "correct horse", false)
            .await
            .expect("user");
        let (_, session) = auth::login(
            &db,
            &config,
            LoginRequest {
                username: "alice".into(),
                password: "correct horse".into(),
            },
            ClientInfo::default(),
        )
        .await
        .expect("login");
        let cookie = format!("{}={}", config.session.cookie_name, session.id);
        let fetcher = Arc::new(FixtureFetcher::new());
        let router = crate::router::router(AppState::with_fetcher(db.clone(), config, fetcher));
        (router, cookie, db)
    }

    async fn send(
        router: Router,
        method: axum::http::Method,
        uri: &str,
        cookie: &str,
    ) -> (StatusCode, serde_json::Value) {
        let res = router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("request");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn list_returns_the_registry() {
        let (router, cookie, _db) = authed().await;
        let (status, body) =
            send(router, axum::http::Method::GET, "/api/v1/packages", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        let packs = body.as_array().expect("array");
        assert_eq!(packs.len(), packages::PACKS.len());
        let rust = packs
            .iter()
            .find(|p| p["slug"] == "rust")
            .expect("rust pack");
        assert_eq!(rust["name"], "Rust");
        assert!(rust["feeds"].as_array().expect("feeds").len() >= 3);
        assert!(rust["feeds"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("http"));
    }

    #[tokio::test]
    async fn install_creates_the_packs_feeds() {
        let (router, cookie, db) = authed().await;
        let (status, body) = send(
            router,
            axum::http::Method::POST,
            "/api/v1/packages/science/install",
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let expected = packages::get("science").expect("pack").feeds.len();
        assert_eq!(body["created_feeds"].as_u64().unwrap(), expected as u64);

        let feeds: i64 = crate::db::support::scalar(&db, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, expected as i64);
    }

    #[tokio::test]
    async fn install_of_an_unknown_slug_is_404() {
        let (router, cookie, _db) = authed().await;
        let (status, _) = send(
            router,
            axum::http::Method::POST,
            "/api/v1/packages/no-such-pack/install",
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
