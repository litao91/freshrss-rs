//! Two guards on the API: a cross-origin check and a rate limit.
//!
//! Both exist because the API is session-cookie authenticated. The cookie is `SameSite=Lax`, which
//! stops a foreign site's *forms*, but a `fetch` from a compromised page on another origin with
//! `credentials: "include"` is exactly the request Lax does not block, and the endpoints that cost
//! the server money — login attempts, outbound fetches, whole-tree refreshes — are the ones an
//! attacker would hammer.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::cookie;
use crate::api::error::ApiError;
use crate::error::Error;
use crate::router::AppState;

/// What a request counts against, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Bucket {
    /// `POST /auth/login` and `/auth/register`: the only endpoints an unauthenticated stranger can
    /// reach, and the only ones worth brute-forcing.
    Auth,
    /// Anything that makes the server open outbound connections on the caller's behalf.
    Fetch,
    /// Whole-tree refreshes, which fan out into one fetch per due feed.
    Refresh,
    /// Everything else: unlimited, because limiting ordinary reads would punish a reader that
    /// scrolls.
    Unmetered,
}

fn bucket(method: &Method, path: &str) -> Bucket {
    if method == Method::GET || method == Method::HEAD {
        return Bucket::Unmetered;
    }
    // A layer on the API router sees the path with the `/api/v1` prefix stripped by `nest`, but the
    // same function is trivially reusable on the whole router, so accept both spellings.
    let path = path.strip_prefix("/api/v1").unwrap_or(path);
    if method == Method::POST && (path == "/auth/login" || path == "/auth/register") {
        Bucket::Auth
    } else if method == Method::POST && (path == "/feeds/discover" || path.ends_with("/refresh")) {
        Bucket::Fetch
    } else if method == Method::POST && path == "/refresh" {
        Bucket::Refresh
    } else {
        Bucket::Unmetered
    }
}

/// Fixed-window counters, per bucket and per caller.
///
/// In memory and per process, which is the right scope: the thing being protected is this
/// process's CPU and its outbound connections, not a shared resource. A restart resets the windows,
/// and an attacker who can restart the server has already won.
#[derive(Debug, Default)]
pub struct Limiter {
    windows: Mutex<HashMap<(String, Bucket), Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    count: u32,
}

impl Limiter {
    /// Records one request and reports whether it is over `per_minute`.
    ///
    /// Also prunes: a map keyed by session id would otherwise grow by one entry per login attempt
    /// ever made, which is precisely the shape of traffic this exists to survive.
    fn allow(&self, key: &str, bucket: Bucket, per_minute: u32, window: Duration) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if windows.len() > 1024 {
            windows.retain(|_, w| now.duration_since(w.started) < window);
        }
        let entry = windows.entry((key.to_owned(), bucket)).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(entry.started) >= window {
            *entry = Window {
                started: now,
                count: 0,
            };
        }
        entry.count += 1;
        entry.count <= per_minute
    }
}

/// The guard itself: refuses cross-origin mutations and metered requests that are over their limit.
pub async fn guard(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    if let Some(response) = check_origin(&req) {
        return response;
    }

    let bucket = bucket(req.method(), req.uri().path());
    if bucket != Bucket::Unmetered {
        let per_minute = state.config.limits.rate_limit_per_minute.max(1);
        // The session cookie when there is one, else the peer address: an authenticated caller is
        // limited per account so two users behind one NAT do not share a budget, and an
        // unauthenticated one per address because that is all there is to key on.
        let key = req
            .headers()
            .get(header::COOKIE)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| cookie::parse(h, &state.config.session.cookie_name))
            .unwrap_or_else(|| peer_addr(&req));
        if !state
            .limiter
            .allow(&key, bucket, per_minute, Duration::from_secs(60))
        {
            let retry = 60;
            let mut response = ApiError(Error::RateLimited {
                retry_in_secs: retry,
            })
            .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, retry.into());
            return response;
        }
    }

    next.run(req).await
}

/// Refuses a mutation whose `Origin` is not the host it was sent to.
///
/// Absent `Origin` passes: `curl`, the CLI and same-origin `fetch` in older browsers omit it, and
/// refusing them would break every non-browser client for a defence that only browsers can provide.
/// Present-but-different is the cross-site case, and that is refused.
fn check_origin(req: &Request<Body>) -> Option<Response> {
    let method = req.method();
    if matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    ) {
        return None;
    }
    let origin = req.headers().get(header::ORIGIN)?;
    let origin = origin.to_str().ok()?;
    if origin == "null" {
        // A sandboxed iframe or a data: document. Neither is this reader.
        return Some(refused());
    }
    let origin_host = url::Url::parse(origin)
        .ok()?
        .host_str()
        .map(str::to_owned)?;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_owned())?;
    if origin_host.eq_ignore_ascii_case(&host) {
        None
    } else {
        Some(refused())
    }
}

fn refused() -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": { "code": "origin_mismatch", "message": "the request's Origin is not this host" }
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn peer_addr(req: &Request<Body>) -> String {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_unmetered() {
        assert_eq!(bucket(&Method::GET, "/api/v1/entries"), Bucket::Unmetered);
        assert_eq!(
            bucket(&Method::GET, "/api/v1/auth/login"),
            Bucket::Unmetered
        );
    }

    #[test]
    fn the_expensive_writes_are_metered_by_kind() {
        assert_eq!(bucket(&Method::POST, "/api/v1/auth/login"), Bucket::Auth);
        assert_eq!(
            bucket(&Method::POST, "/auth/register"),
            Bucket::Auth,
            "the prefix-stripped spelling too"
        );
        assert_eq!(
            bucket(&Method::POST, "/api/v1/feeds/discover"),
            Bucket::Fetch
        );
        assert_eq!(
            bucket(&Method::POST, "/api/v1/feeds/7/refresh"),
            Bucket::Fetch
        );
        assert_eq!(bucket(&Method::POST, "/api/v1/refresh"), Bucket::Refresh);
        assert_eq!(
            bucket(&Method::POST, "/api/v1/entries/read"),
            Bucket::Unmetered,
            "marking articles read is cheap and must not be throttled into uselessness"
        );
        assert_eq!(
            bucket(&Method::DELETE, "/api/v1/feeds/7"),
            Bucket::Unmetered
        );
    }

    #[test]
    fn a_window_admits_up_to_its_limit_then_refuses() {
        let limiter = Limiter::default();
        for i in 0..5 {
            assert!(
                limiter.allow("k", Bucket::Auth, 5, Duration::from_secs(60)),
                "{i}"
            );
        }
        assert!(!limiter.allow("k", Bucket::Auth, 5, Duration::from_secs(60)));
        // Another caller and another bucket each have their own window.
        assert!(limiter.allow("other", Bucket::Auth, 5, Duration::from_secs(60)));
        assert!(limiter.allow("k", Bucket::Fetch, 5, Duration::from_secs(60)));
    }

    #[test]
    fn a_window_resets_after_its_span() {
        let limiter = Limiter::default();
        let span = Duration::from_millis(20);
        for _ in 0..2 {
            assert!(limiter.allow("k", Bucket::Auth, 2, span));
        }
        assert!(!limiter.allow("k", Bucket::Auth, 2, span));
        std::thread::sleep(span + Duration::from_millis(5));
        assert!(
            limiter.allow("k", Bucket::Auth, 2, span),
            "the window rolled over"
        );
    }
}
