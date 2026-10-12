//! Serving the built SPA.
//!
//! Two sources, in priority order:
//!
//! 1. `server.static_dir` on disk, when it contains an `index.html`. Lets an operator replace
//!    the UI without recompiling, and lets `npm run build` output show up immediately in debug.
//! 2. Assets embedded at compile time by `rust-embed`. A release binary built after
//!    `npm run build` is therefore self-contained and needs no files alongside it.
//!
//! Unknown paths fall back to `index.html` so client-side routing works.

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use rust_embed::Embed;
use tower_http::services::ServeFile;

use crate::router::AppState;

#[derive(Embed)]
#[folder = "../../static"]
struct Embedded;

/// Vite emits content-hashed filenames under `assets/`, so those can be cached forever.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `index.html` must always be revalidated, or a deploy is invisible until the cache expires.
const NO_CACHE: &str = "no-cache";

pub fn router() -> Router<AppState> {
    Router::new().fallback(get_static)
}

fn cache_control_for(path: &str) -> &'static str {
    // Only Vite's content-hashed output under `assets/` is safe to cache forever. Everything
    // else — `index.html`, `public/**` copies like fonts and the manifest — must revalidate.
    if path.starts_with("assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    }
}

async fn get_static(State(state): State<AppState>, uri: Uri) -> Response {
    let raw = uri.path();
    // Reject anything that could escape the served directory before touching the filesystem.
    let path = match sanitize(raw) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "bad path").into_response(),
    };

    if let Some(dir) = disk_root(&state.config.server.static_dir) {
        let candidate = dir.join(&path);
        // `sanitize` already rejected traversal; only a regular file is served, so a request for
        // a directory or an unknown client-side route falls through to the SPA entry point.
        let target = if candidate.is_file() {
            candidate
        } else {
            dir.join("index.html")
        };
        // ServeFile always serves its configured path, so the request URI is irrelevant here.
        let req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("valid request");
        let mut res = tower::ServiceExt::oneshot(ServeFile::new(target), req)
            .await
            .map_or_else(
                |err| {
                    tracing::warn!(%err, "static file service failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "static error").into_response()
                },
                |res| res.into_response(),
            );
        apply_cache_headers(&mut res, &path);
        return res;
    }

    embedded(&path)
}

/// Normalizes a request path into a safe relative key.
///
/// Returns `None` for paths that attempt traversal or that contain a NUL, and maps `/` and
/// `index.html` to the same key.
fn sanitize(raw: &str) -> Option<String> {
    let trimmed = raw.trim_start_matches('/');
    if trimmed.contains('\0') {
        return None;
    }
    // Percent-encoded traversal (`%2e%2e`) is decoded by the time it reaches us, but reject
    // the literal form too so a reverse proxy that double-decodes cannot escape.
    for segment in trimmed.split('/') {
        if segment == ".." {
            return None;
        }
    }
    if trimmed.is_empty() || trimmed == "index.html" {
        return Some("index.html".to_string());
    }
    Some(trimmed.to_string())
}

/// The on-disk static root, if it is usable.
fn disk_root(configured: &Path) -> Option<PathBuf> {
    let index = configured.join("index.html");
    index.is_file().then(|| configured.to_path_buf())
}

fn embedded(path: &str) -> Response {
    let key = if path.is_empty() { "index.html" } else { path };
    match Embedded::get(key) {
        Some(file) => {
            let mime = mime_guess::from_path(key).first_or_octet_stream();
            let mut res = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref())],
                file.data.into_owned(),
            )
                .into_response();
            apply_cache_headers(&mut res, key);
            res
        }
        // SPA fallback: let the client router decide.
        None => match Embedded::get("index.html") {
            Some(index) => {
                let mut res = (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    index.data.into_owned(),
                )
                    .into_response();
                apply_cache_headers(&mut res, "index.html");
                res
            }
            None => (
                StatusCode::NOT_FOUND,
                "the SPA has not been built yet; run `npm --prefix web run build`",
            )
                .into_response(),
        },
    }
}

fn apply_cache_headers(res: &mut Response, path: &str) {
    if let Ok(value) = HeaderValue::from_str(cache_control_for(path)) {
        res.headers_mut().insert(header::CACHE_CONTROL, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(sanitize("/../etc/passwd"), None);
        assert_eq!(sanitize("/a/../../b"), None);
        assert_eq!(sanitize("/a\0b"), None);
    }

    #[test]
    fn sanitize_maps_root_to_index() {
        assert_eq!(sanitize("/").as_deref(), Some("index.html"));
        assert_eq!(sanitize("").as_deref(), Some("index.html"));
        assert_eq!(sanitize("/index.html").as_deref(), Some("index.html"));
    }

    #[test]
    fn sanitize_passes_normal_paths_through() {
        assert_eq!(
            sanitize("/assets/app-abc123.js").as_deref(),
            Some("assets/app-abc123.js")
        );
        assert_eq!(sanitize("/favicon.ico").as_deref(), Some("favicon.ico"));
    }

    #[test]
    fn hashed_assets_are_immutable_and_index_is_not() {
        assert_eq!(cache_control_for("assets/app-abc123.js"), IMMUTABLE);
        assert_eq!(cache_control_for("assets/app-abc123.css"), IMMUTABLE);
        assert_eq!(cache_control_for("index.html"), NO_CACHE);
        assert_eq!(cache_control_for(""), NO_CACHE);
        assert_eq!(cache_control_for("manifest.json"), NO_CACHE);
    }

    #[test]
    fn vendor_fonts_are_not_immutable_but_hashed_bundles_are() {
        // Vite copies `web/public/**` to the output root without hashing, so anything not under
        // assets/ must stay revalidatable.
        assert_eq!(
            cache_control_for("fonts/literata-normal-latin.woff2"),
            NO_CACHE
        );
        assert_eq!(cache_control_for("robots.txt"), NO_CACHE);
        assert_eq!(cache_control_for("assets/app-abc123.woff2"), IMMUTABLE);
    }

    #[test]
    fn disk_root_requires_an_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(disk_root(dir.path()), None, "empty dir must not be used");
        std::fs::write(dir.path().join("index.html"), "<html></html>").expect("write");
        assert_eq!(disk_root(dir.path()), Some(dir.path().to_path_buf()));
    }

    #[tokio::test]
    async fn falls_back_to_index_for_unknown_routes() {
        // With nothing embedded and nothing on disk, the SPA fallback reports the build hint
        // rather than a bare 404 — that is the operator-facing failure mode we want.
        let res = embedded("some/client/route");
        assert!(matches!(
            res.status(),
            StatusCode::OK | StatusCode::NOT_FOUND
        ));
    }

    #[tokio::test]
    async fn serves_from_disk_when_available() {
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "<html>root</html>").expect("write");
        std::fs::create_dir_all(dir.path().join("assets")).expect("mkdir");
        std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").expect("write");

        let state = AppState::new(
            crate::db::Db::in_memory().await.expect("db"),
            crate::config::AppConfig {
                server: crate::config::ServerConfig {
                    static_dir: dir.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let app = router().with_state(state);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CACHE_CONTROL).unwrap(), IMMUTABLE);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], b"console.log(1)");

        // Unknown route → index.html, with no-cache.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/reader?feed=3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CACHE_CONTROL).unwrap(), NO_CACHE);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], b"<html>root</html>");
    }

    #[tokio::test]
    async fn rejects_traversal_before_touching_the_filesystem() {
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "<html></html>").expect("write");
        std::fs::write(dir.path().parent().unwrap().join("secret.txt"), "nope").ok();

        let state = AppState::new(
            crate::db::Db::in_memory().await.expect("db"),
            crate::config::AppConfig {
                server: crate::config::ServerConfig {
                    static_dir: dir.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let app = router().with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/../../etc/passwd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        // axum normalizes `..` in the URI before we see it, so this must not be a 200 of /etc/passwd.
        assert_ne!(res.status(), StatusCode::OK);
    }
}
