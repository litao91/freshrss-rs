//! Subscription portability: OPML export and import, and the feed-icon endpoint.
//!
//! Handlers only translate between HTTP and the services in [`crate::service`]; the rules about what a
//! document may contain, what gets skipped, and where an icon comes from all live there.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path as PathParam, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::feed as feed_dao;
use crate::domain::ids::now_unix;
use crate::error::Error;
use crate::router::AppState;
use crate::service::import::ImportReport;
use crate::service::refresh::Settings;
use crate::service::{export, favicon};

/// A cap on the OPML upload, applied to that one route rather than globally.
///
/// A subscription list is the one body a user legitimately posts that can be large — a decade of
/// feeds with their selector maps is easily over a megabyte — while every other endpoint takes a small
/// JSON object. Raising the limit only here keeps the rest of the surface at axum's default without
/// making an upload fail at an arbitrary size.
pub const IMPORT_BODY_LIMIT: usize = 8 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new().route("/export/opml", get(export_opml)).route(
        "/import/opml",
        post(import_opml).layer(DefaultBodyLimit::max(IMPORT_BODY_LIMIT)),
    )
}

/// The whole subscription tree as an OPML document, offered as a download.
pub async fn export_opml(State(state): State<AppState>, user: CurrentUser) -> ApiResult<Response> {
    let doc = export::opml(&state.db, user.id).await.map_err(ApiError)?;
    let body = freshrss_feed::opml::write(&doc);

    // The filename carries the day, the way PHP's exporter does, so repeated downloads do not
    // overwrite each other in the browser's download folder.
    let day = jiff::Timestamp::from_millisecond(now_unix().saturating_mul(1000))
        .map(|ts| {
            ts.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|_| "export".to_owned());

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/x-opml; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"feeds_{day}.opml.xml\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    Ok((StatusCode::OK, headers, body).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ImportParams {
    /// Preview the upload without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// File every feed under this folder, ignoring the document's own nesting. A string because ids go
    /// over the wire as strings; empty or absent means "honour the document".
    #[serde(default)]
    pub force_category: Option<String>,
}

/// Imports an uploaded OPML document, returning what it did.
pub async fn import_opml(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(params): Query<ImportParams>,
    body: Bytes,
) -> ApiResult<Json<ImportReport>> {
    let force_category = match params.force_category.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            raw.parse::<i64>()
                .map_err(|_| Error::Validation(format!("{raw:?} is not a folder id")))?,
        ),
    };
    let report =
        crate::service::import::opml(&state.db, user.id, &body, params.dry_run, force_category)
            .await
            .map_err(ApiError)?;
    Ok(Json(report))
}

/// A feed's icon, from the cache when it is fresh and by fetching it otherwise.
///
/// Registered inside [`super::feeds`]' router so it lives under `/feeds/{id}/icon` beside the other
/// per-feed routes; axum routes a whole `/feeds` subtree through that nest, so the path could not be
/// added from here.
pub async fn feed_icon(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Response> {
    let id_user = user.id;
    let feed = state
        .db
        .with(move |mut conn| async move {
            feed_dao::find_by_id(&mut conn, id, id_user)
                .await?
                .ok_or_else(|| Error::NotFound(format!("feed {id}")))
        })
        .await
        .map_err(ApiError)?;

    let settings = Settings::from_config(&state.config);
    let icon = favicon::icon(&state.db, &*state.fetcher, &feed, &settings, now_unix())
        .await
        .map_err(ApiError)?;

    match icon {
        Some(icon) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&icon.content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            );
            // A week, matching the cache TTL: the icon is stable, and the browser re-requesting it on
            // every render is exactly the traffic the server-side cache exists to absorb.
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=604800"),
            );
            Ok((StatusCode::OK, headers, icon.bytes).into_response())
        }
        // A feed with no icon is a normal state, not a fault, so the endpoint 404s and the reader
        // falls back to the letter avatar it already renders.
        None => Err(ApiError(Error::NotFound(format!("feed {id} has no icon")))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use freshrss_feed::http::reqwest::FixtureError;
    use freshrss_feed::http::{Fetcher, FixtureFetcher, FixtureResponse};
    use tower::ServiceExt as _;

    use crate::config::AppConfig;
    use crate::db::dao::feed as feed_dao;
    use crate::db::Db;
    use crate::domain::attributes::keys;
    use crate::domain::feed::Feed;
    use crate::router::AppState;
    use crate::service::auth::{self, ClientInfo, LoginRequest};

    /// A routed app with one signed-in user, plus the cookie that authenticates as them.
    async fn authed(fetcher: Arc<dyn Fetcher>) -> (Router, String, Db, i64) {
        let db = Db::in_memory().await.expect("db");
        let config = AppConfig::default();
        // The policy-free path: these tests are about import and export, not registration, and
        // must not flip when the shipped default mode changes.
        let user = auth::create_user(&db, "alice", None, "correct horse", false)
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
        let router = crate::router::router(AppState::with_fetcher(db.clone(), config, fetcher));
        (router, cookie, db, user.id)
    }

    fn req(method: axum::http::Method, uri: &str, cookie: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(body)
            .expect("request")
    }

    async fn seed_feed(db: &Db, id_user: i64, url: &str, icon: Option<&str>) -> i64 {
        let url = url.to_owned();
        let icon = icon.map(str::to_owned);
        db.with(move |mut conn| async move {
            let mut feed = Feed::new(id_user, &url, "Example");
            feed.website = "https://e.example/".into();
            feed.category = crate::db::dao::user::default_category_id(&mut conn, id_user).await?;
            if let Some(icon) = icon {
                feed.attributes.set(keys::FEED_ICON_URL, icon);
            }
            feed_dao::insert(&mut conn, &feed).await
        })
        .await
        .expect("seeded")
    }

    #[tokio::test]
    async fn export_serves_a_downloadable_opml_document() {
        let (router, cookie, db, id_user) = authed(Arc::new(FixtureFetcher::new())).await;
        seed_feed(&db, id_user, "https://e.example/feed.xml", None).await;

        let res = router
            .oneshot(req(
                axum::http::Method::GET,
                "/api/v1/export/opml",
                &cookie,
                Body::empty(),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/x-opml; charset=utf-8"
        );
        let disposition = res
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            disposition.starts_with("attachment; filename=\"feeds_"),
            "{disposition}"
        );
        assert!(disposition.ends_with(".opml.xml\""), "{disposition}");

        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let doc = freshrss_feed::opml::parse(std::str::from_utf8(&body).unwrap()).expect("parses");
        assert!(
            doc.outlines.iter().any(|o| o
                .children
                .iter()
                .any(|f| f.xml_url.as_deref() == Some("https://e.example/feed.xml"))),
            "the seeded feed is in the export"
        );
    }

    #[tokio::test]
    async fn import_accepts_a_raw_body_and_reports() {
        let (router, cookie, db, _id_user) = authed(Arc::new(FixtureFetcher::new())).await;
        let body = freshrss_feed::opml::write(&freshrss_feed::opml::Opml {
            outlines: vec![freshrss_feed::opml::Outline::feed(
                "Example",
                "https://e.example/feed.xml",
            )],
            ..Default::default()
        });

        let res = router
            .oneshot(req(
                axum::http::Method::POST,
                "/api/v1/import/opml",
                &cookie,
                Body::from(body),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let report: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(report["created_feeds"], 1);

        let feeds: i64 = crate::db::support::scalar(&db, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, 1);
    }

    #[tokio::test]
    async fn import_dry_run_writes_nothing() {
        let (router, cookie, db, _id_user) = authed(Arc::new(FixtureFetcher::new())).await;
        let body = freshrss_feed::opml::write(&freshrss_feed::opml::Opml {
            outlines: vec![freshrss_feed::opml::Outline::feed(
                "Example",
                "https://e.example/feed.xml",
            )],
            ..Default::default()
        });

        let res = router
            .oneshot(req(
                axum::http::Method::POST,
                "/api/v1/import/opml?dry_run=true",
                &cookie,
                Body::from(body),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let report: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(report["created_feeds"], 1, "what would be added");
        let feeds: i64 = crate::db::support::scalar(&db, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(feeds, 0, "a dry run writes nothing");
    }

    #[tokio::test]
    async fn import_rejects_a_malformed_document_with_400() {
        let (router, cookie, _db, _id_user) = authed(Arc::new(FixtureFetcher::new())).await;
        let res = router
            .oneshot(req(
                axum::http::Method::POST,
                "/api/v1/import/opml",
                &cookie,
                Body::from("<rss><channel/></rss>"),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn import_requires_authentication() {
        let (router, _cookie, _db, _id_user) = authed(Arc::new(FixtureFetcher::new())).await;
        let res = router
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/api/v1/import/opml")
                    .body(Body::from("<opml/>"))
                    .expect("request"),
            )
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    fn png() -> (Vec<u8>, FixtureResponse) {
        let bytes = vec![0x89, b'P', b'N', b'G'];
        let response = FixtureResponse::Body {
            status: 200,
            body: bytes.clone(),
            etag: None,
            last_modified: None,
            content_type: Some("image/png".to_owned()),
        };
        (bytes, response)
    }

    #[tokio::test]
    async fn the_feed_icon_endpoint_serves_the_fetched_bytes() {
        let (bytes, response) = png();
        let fetcher = Arc::new(FixtureFetcher::new().with("https://e.example/icon.png", response));
        let (router, cookie, db, id_user) = authed(fetcher).await;
        let feed_id = seed_feed(
            &db,
            id_user,
            "https://e.example/feed.xml",
            Some("https://e.example/icon.png"),
        )
        .await;

        let res = router
            .oneshot(req(
                axum::http::Method::GET,
                &format!("/api/v1/feeds/{feed_id}/icon"),
                &cookie,
                Body::empty(),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("max-age=604800"),
            "the icon is cacheable"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], &bytes[..]);
    }

    #[tokio::test]
    async fn the_feed_icon_endpoint_404s_when_there_is_no_icon() {
        let fetcher = Arc::new(FixtureFetcher::new().with(
            "https://e.example/favicon.ico",
            FixtureResponse::Error(FixtureError::Http(404)),
        ));
        let (router, cookie, db, id_user) = authed(fetcher).await;
        // No published icon, so the endpoint derives favicon.ico, which the fixture 404s.
        let feed_id = seed_feed(&db, id_user, "https://e.example/feed.xml", None).await;

        let res = router
            .oneshot(req(
                axum::http::Method::GET,
                &format!("/api/v1/feeds/{feed_id}/icon"),
                &cookie,
                Body::empty(),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_feed_icon_endpoint_404s_for_another_users_feed() {
        let fetcher = Arc::new(FixtureFetcher::new());
        let (router, cookie, _db, _id_user) = authed(fetcher).await;
        // A feed id alice does not own must 404 rather than leak whether it exists.
        let res = router
            .oneshot(req(
                axum::http::Method::GET,
                "/api/v1/feeds/999/icon",
                &cookie,
                Body::empty(),
            ))
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
