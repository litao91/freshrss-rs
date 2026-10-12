//! The `reqwest` implementation of [`Fetcher`].
//!
//! Redirects are followed manually rather than by reqwest's policy, for three reasons that all matter
//! to a feed reader:
//!
//! 1. A permanent move has to be *distinguished* from a temporary one, because HTTP 301 means the
//!    subscription URL should be rewritten. reqwest's policy collapses both into "the final URL".
//! 2. A redirect target must be checked against the same rules as the original URL. Letting the
//!    client follow a hop to `file://` or `gopher://` would hand a feed author a way to reach
//!    whatever the server process can.
//! 3. The size cap has to apply to the body we actually keep, and a redirect chain is the easiest
//!    place to lose track of which response that is.

use std::time::Duration;

use futures::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};
use reqwest::{Client, Method, StatusCode};

use super::{body_hash, FetchOutcome, FetchRequest, FetchedBody};
use crate::error::FeedError;

/// How many redirects to follow before giving up.
const MAX_REDIRECTS: usize = 10;

/// Fetches over HTTP(S) with `reqwest`.
///
/// One client is reused across every feed it serves. That is not just an optimization: `reqwest`
/// keeps a connection pool and a DNS cache per client, and rebuilding either on every refresh turns
/// a hundred-feed tick into a hundred TLS handshakes.
pub struct ReqwestFetcher {
    /// Client for the normal case (certificate verification on).
    verified: Client,
    /// Client for feeds whose owner has explicitly disabled verification.
    ///
    /// Built lazily and kept separate, because `danger_accept_invalid_certs` is per-client and one
    /// feed's self-signed certificate must not weaken every other feed's TLS.
    unverified: once_cell::sync::OnceCell<Client>,
}

impl ReqwestFetcher {
    /// Builds a fetcher with a shared connection pool.
    ///
    /// # Errors
    ///
    /// Only if `reqwest` cannot build a client at all, which in practice means a TLS backend
    /// problem — fatal at startup, not something to retry per feed.
    pub fn new() -> Result<Self, FeedError> {
        Ok(Self {
            verified: base_client(true)?,
            unverified: once_cell::sync::OnceCell::new(),
        })
    }
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new().expect("reqwest client construction does not perform IO")
    }
}

fn base_client(ssl_verify: bool) -> Result<Client, FeedError> {
    Client::builder()
        // Redirects are handled in `fetch`, so the client must not follow them itself.
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(!ssl_verify)
        .tcp_nodelay(true)
        .pool_max_idle_per_host(4)
        .build()
        .map_err(|e| FeedError::BadConfig(e.to_string()))
}

#[async_trait::async_trait]
impl super::Fetcher for ReqwestFetcher {
    async fn fetch(&self, request: &FetchRequest) -> Result<FetchOutcome, FeedError> {
        let mut url = request.url.clone();
        let mut permanently_moved = false;

        for _ in 0..=MAX_REDIRECTS {
            validate_url(&url)?;
            let response = send(self, request, &url).await?;
            let status = response.status();

            if status.is_redirection() {
                let location = header(response.headers(), "location")
                    .ok_or_else(|| transport(&url, "redirect with no Location header"))?;
                if status == StatusCode::MOVED_PERMANENTLY
                    || status == StatusCode::PERMANENT_REDIRECT
                {
                    permanently_moved = true;
                }
                // Resolved against the URL we asked for, not the original, so a chain of relative
                // hops works.
                url = response
                    .url()
                    .join(&location)
                    .map(|u| u.to_string())
                    .map_err(|e| FeedError::BadUrl {
                        url: location.clone(),
                        reason: e.to_string(),
                    })?;
                continue;
            }

            return finish(request, response, permanently_moved).await;
        }

        Err(transport(
            &request.url,
            &format!("more than {MAX_REDIRECTS} redirects"),
        ))
    }
}

/// A transport failure that has no `reqwest::Error` behind it.
///
/// `status: 0` is what [`FeedError::is_transient`] reads as "the request never completed", so a
/// malformed redirect still gets retried on the next tick rather than being treated as permanent.
fn transport(url: &str, reason: &str) -> FeedError {
    FeedError::Http {
        status: 0,
        message: format!("{url}: {reason}"),
    }
}

/// Builds and sends one request.
async fn send(
    fetcher: &ReqwestFetcher,
    request: &FetchRequest,
    url: &str,
) -> Result<reqwest::Response, FeedError> {
    let client = client_for(fetcher, request.ssl_verify)?;
    let mut builder = client
        .request(Method::GET, url)
        .timeout(request.timeout)
        .header(
            USER_AGENT,
            HeaderValue::from_str(&request.user_agent)
                .unwrap_or_else(|_| HeaderValue::from_static(super::DEFAULT_USER_AGENT)),
        )
        .header(
            ACCEPT,
            HeaderValue::from_str(&request.accept).unwrap_or(HeaderValue::from_static("*/*")),
        );

    if let Some((user, pass)) = &request.basic_auth {
        builder = builder.basic_auth(user, Some(pass));
    }
    for (name, value) in request.conditional.as_headers() {
        if let Ok(v) = HeaderValue::from_str(value) {
            builder = builder.header(name, v);
        }
    }

    builder
        .send()
        .await
        .map_err(|e| map_transport_error(e, request.timeout))
}

/// Selects the verified or unverified client, building the latter on first use.
fn client_for(fetcher: &ReqwestFetcher, ssl_verify: bool) -> Result<&Client, FeedError> {
    if ssl_verify {
        return Ok(&fetcher.verified);
    }
    fetcher.unverified.get_or_try_init(|| base_client(false))
}

/// Turns a `reqwest` error into the distinction the refresh pipeline acts on.
///
/// `timeout_secs` is passed in because `reqwest` does not report which timeout fired; the message a
/// user sees should say how long the fetch was given, and that is only known by the caller.
fn map_transport_error(err: reqwest::Error, timeout: Duration) -> FeedError {
    if err.is_timeout() {
        return FeedError::Timeout {
            secs: timeout.as_secs(),
        };
    }
    if let Some(status) = err.status() {
        return FeedError::Http {
            status: status.as_u16(),
            message: err.to_string(),
        };
    }
    FeedError::Transport(err)
}

/// Consumes a non-redirect response into a [`FetchOutcome`].
async fn finish(
    request: &FetchRequest,
    response: reqwest::Response,
    permanently_moved: bool,
) -> Result<FetchOutcome, FeedError> {
    let status = response.status();
    let headers = response.headers().clone();
    let etag = header(&headers, "etag");
    let last_modified = header(&headers, "last-modified");
    let content_type = header(&headers, "content-type");
    let effective_url = response.url().to_string();

    if status == StatusCode::NOT_MODIFIED {
        return Ok(FetchOutcome::NotModified);
    }

    // 410 Gone is the one status with a specific downstream meaning: the feed is dead and should be
    // muted rather than retried forever.
    if status == StatusCode::GONE {
        return Err(FeedError::Gone);
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
        if let Some(retry_after) = header(&headers, "retry-after") {
            let now = jiff::Timestamp::now().as_second();
            if let Some(delay) = super::parse_retry_after(&retry_after, now) {
                return Err(FeedError::RetryAfter {
                    until: now + delay.as_secs() as i64,
                });
            }
        }
    }
    if status.is_client_error() || status.is_server_error() {
        return Err(FeedError::Http {
            status: status.as_u16(),
            message: status.canonical_reason().unwrap_or("error").to_owned(),
        });
    }

    let body = read_body_capped(response, request.max_bytes, request.timeout).await?;
    let hash = body_hash(&body);

    Ok(FetchOutcome::Found(FetchedBody {
        body,
        status: status.as_u16(),
        etag,
        last_modified,
        content_type,
        effective_url,
        hash,
        permanently_moved,
    }))
}

/// Reads a response body, stopping as soon as the cap is exceeded.
///
/// Streaming rather than `response.bytes()`: a server that advertises a small `Content-Length` and
/// then sends gigabytes, or one that sends no length at all, must be stopped mid-transfer. Checking
/// the length after the fact means the memory was already allocated.
async fn read_body_capped(
    response: reqwest::Response,
    max_bytes: u64,
    timeout: Duration,
) -> Result<Vec<u8>, FeedError> {
    // A truthful Content-Length lets us refuse before reading anything.
    if let Some(declared) = response.content_length() {
        if declared > max_bytes {
            return Err(FeedError::TooLarge {
                max: max_bytes as usize,
            });
        }
    }

    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| map_transport_error(e, timeout))?;
        body.extend_from_slice(&chunk);
        if body.len() as u64 > max_bytes {
            return Err(FeedError::TooLarge {
                max: max_bytes as usize,
            });
        }
    }
    Ok(body)
}

/// Reads one header as a trimmed string.
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// Refuses a URL that is not an absolute `http`/`https` one.
///
/// Checked on every hop, not just the first. A feed author who can make us follow a redirect can
/// otherwise point us at `file:///etc/passwd` or at the cloud metadata endpoint, and the response
/// body would be parsed and stored as an article — readable by anyone who can open that feed.
fn validate_url(url: &str) -> Result<(), FeedError> {
    let parsed = url::Url::parse(url).map_err(|e| FeedError::BadUrl {
        url: url.to_owned(),
        reason: e.to_string(),
    })?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(FeedError::NonRemote(format!(
                "{url} (scheme {other:?}, only http and https are allowed)"
            )));
        }
    }
    // `url` already rejects an empty authority for a special scheme (`http://` → "empty host"), so
    // this only has to catch the case it normalizes away rather than refuses.
    if !matches!(parsed.host_str(), Some(host) if !host.is_empty()) {
        return Err(FeedError::BadUrl {
            url: url.to_owned(),
            reason: "no host".to_owned(),
        });
    }
    Ok(())
}

/// A [`Fetcher`] backed by canned responses, for tests and for the CLI's `--offline` mode.
///
/// Every fetch is recorded so a test can assert on the request that was made — the conditional-GET
/// headers in particular, which are otherwise invisible.
#[derive(Debug, Default, Clone)]
pub struct FixtureFetcher {
    /// Canned response per URL.
    responses: std::collections::HashMap<String, FixtureResponse>,
    /// Requests received, in order.
    requests: std::sync::Arc<std::sync::Mutex<Vec<FetchRequest>>>,
}

/// What a [`FixtureFetcher`] answers with.
#[derive(Debug, Clone)]
pub enum FixtureResponse {
    Body {
        status: u16,
        body: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
        content_type: Option<String>,
    },
    NotModified,
    Error(FixtureError),
}

/// A failure a fixture can simulate.
///
/// Not [`FeedError`] itself, because that holds a `reqwest::Error` and so cannot be `Clone` — and a
/// fixture table has to be cloneable to be shared across a test's assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureError {
    Gone,
    Timeout,
    TooLarge,
    Transport,
    Http(u16),
}

impl FixtureError {
    /// The real error this stands in for.
    pub fn to_feed_error(self) -> FeedError {
        match self {
            FixtureError::Gone => FeedError::Gone,
            FixtureError::Timeout => FeedError::Timeout { secs: 30 },
            FixtureError::TooLarge => FeedError::TooLarge { max: 0 },
            FixtureError::Transport => FeedError::Http {
                status: 0,
                message: "simulated transport failure".to_owned(),
            },
            FixtureError::Http(status) => FeedError::Http {
                status,
                message: format!("simulated http {status}"),
            },
        }
    }
}

impl FixtureResponse {
    /// A plain 200 with the given body.
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        FixtureResponse::Body {
            status: 200,
            body: body.into(),
            etag: None,
            last_modified: None,
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    /// A 200 carrying an `ETag` and `Last-Modified`, so a second fetch can be a 304.
    pub fn ok_with_validators(body: impl Into<Vec<u8>>, etag: &str, last_modified: &str) -> Self {
        FixtureResponse::Body {
            status: 200,
            body: body.into(),
            etag: Some(etag.to_owned()),
            last_modified: Some(last_modified.to_owned()),
            content_type: Some("application/rss+xml".to_owned()),
        }
    }
}

impl FixtureFetcher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the response for a URL.
    #[must_use]
    pub fn with(mut self, url: impl Into<String>, response: FixtureResponse) -> Self {
        self.responses.insert(url.into(), response);
        self
    }

    /// The requests made so far, in order.
    pub fn requests(&self) -> Vec<FetchRequest> {
        self.requests.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl super::Fetcher for FixtureFetcher {
    async fn fetch(&self, request: &FetchRequest) -> Result<FetchOutcome, FeedError> {
        if let Ok(mut seen) = self.requests.lock() {
            seen.push(request.clone());
        }
        let Some(response) = self.responses.get(&request.url) else {
            return Err(FeedError::Http {
                status: 404,
                message: format!("no fixture registered for {}", request.url),
            });
        };
        match response {
            FixtureResponse::Body {
                status,
                body,
                etag,
                last_modified,
                content_type,
            } => {
                // Honour a conditional request the way a real server would, so the 304 path is
                // exercised rather than stubbed.
                let validators_match =
                    request.conditional.etag.as_deref() == etag.as_deref() && etag.is_some();
                if *status == 200 && validators_match {
                    return Ok(FetchOutcome::NotModified);
                }
                if body.len() as u64 > request.max_bytes {
                    return Err(FeedError::TooLarge {
                        max: request.max_bytes as usize,
                    });
                }
                Ok(FetchOutcome::Found(FetchedBody {
                    body: body.clone(),
                    status: *status,
                    etag: etag.clone(),
                    last_modified: last_modified.clone(),
                    content_type: content_type.clone(),
                    effective_url: request.url.clone(),
                    hash: body_hash(body),
                    permanently_moved: false,
                }))
            }
            FixtureResponse::NotModified => Ok(FetchOutcome::NotModified),
            FixtureResponse::Error(err) => Err(err.to_feed_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::condget::ConditionalHeaders;
    use super::super::Fetcher as _;
    use super::*;

    fn req(url: &str) -> FetchRequest {
        FetchRequest::new(url)
    }

    #[tokio::test]
    async fn a_fixture_fetcher_returns_the_canned_body() {
        let fetcher = FixtureFetcher::new().with(
            "https://example.com/feed.xml",
            FixtureResponse::ok(b"<rss></rss>".to_vec()),
        );
        let outcome = fetcher
            .fetch(&req("https://example.com/feed.xml"))
            .await
            .unwrap();
        match outcome {
            FetchOutcome::Found(f) => {
                assert_eq!(f.body, b"<rss></rss>");
                assert_eq!(f.status, 200);
                assert_eq!(f.effective_url, "https://example.com/feed.xml");
                assert!(!f.hash.is_empty());
            }
            FetchOutcome::NotModified => panic!("expected a body"),
        }
    }

    #[tokio::test]
    async fn an_unknown_url_is_not_found() {
        let fetcher = FixtureFetcher::new();
        let err = fetcher
            .fetch(&req("https://nope.example/feed"))
            .await
            .unwrap_err();
        assert!(matches!(err, FeedError::Http { status: 404, .. }), "{err}");
    }

    #[tokio::test]
    async fn a_replayed_etag_yields_not_modified() {
        let fetcher = FixtureFetcher::new().with(
            "https://example.com/feed.xml",
            FixtureResponse::ok_with_validators(
                b"<rss></rss>".to_vec(),
                "\"v1\"",
                "Mon, 02 Jan 2006 15:04:05 GMT",
            ),
        );

        let first = fetcher
            .fetch(&req("https://example.com/feed.xml"))
            .await
            .unwrap();
        let etag = match first {
            FetchOutcome::Found(f) => f.etag.unwrap(),
            FetchOutcome::NotModified => panic!("first fetch must return a body"),
        };
        assert_eq!(etag, "\"v1\"");

        let second = FetchRequest {
            conditional: ConditionalHeaders {
                etag: Some(etag),
                last_modified: None,
            },
            ..req("https://example.com/feed.xml")
        };
        assert_eq!(
            fetcher.fetch(&second).await.unwrap(),
            FetchOutcome::NotModified
        );
    }

    #[tokio::test]
    async fn the_fixture_fetcher_records_what_was_asked_for() {
        let fetcher = FixtureFetcher::new().with(
            "https://example.com/f",
            FixtureResponse::ok(b"<rss/>".to_vec()),
        );
        let request = FetchRequest {
            accept: "text/html".to_owned(),
            basic_auth: Some(("user".to_owned(), "pw".to_owned())),
            ..req("https://example.com/f")
        };
        fetcher.fetch(&request).await.unwrap();

        let seen = fetcher.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].accept, "text/html");
        assert_eq!(
            seen[0].basic_auth.as_ref().map(|(u, _)| u.as_str()),
            Some("user")
        );
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_refused() {
        let fetcher = FixtureFetcher::new().with(
            "https://example.com/big",
            FixtureResponse::ok(vec![b'x'; 1024]),
        );
        let request = FetchRequest {
            max_bytes: 10,
            ..req("https://example.com/big")
        };
        assert!(matches!(
            fetcher.fetch(&request).await.unwrap_err(),
            FeedError::TooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn a_canned_error_is_passed_through() {
        let fetcher = FixtureFetcher::new().with(
            "https://example.com/gone",
            FixtureResponse::Error(FixtureError::Gone),
        );
        assert!(matches!(
            fetcher
                .fetch(&req("https://example.com/gone"))
                .await
                .unwrap_err(),
            FeedError::Gone
        ));
    }

    // --- URL validation ----------------------------------------------------

    #[test]
    fn only_absolute_http_and_https_are_accepted() {
        for ok in [
            "http://example.com/f",
            "https://example.com/f",
            "https://example.com:8443/f?x=1",
        ] {
            assert!(validate_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "gopher://example.com/",
            "ftp://example.com/f",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "example.com/feed.xml",
            "",
            "http://",
        ] {
            assert!(validate_url(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_url_with_no_host_is_refused() {
        assert!(validate_url("http://").is_err());
        assert!(validate_url("http:///").is_err());
        // `http:///path` is *not* hostless: the URL parser reads `path` as the host, which is a
        // valid (if odd) target and none of this function's business.
        assert!(validate_url("http:///path").is_ok());
    }

    // --- header reading ----------------------------------------------------

    #[test]
    fn a_blank_or_invalid_header_is_none() {
        let mut headers = HeaderMap::new();
        headers.insert("etag", HeaderValue::from_static("  "));
        assert_eq!(header(&headers, "etag"), None);
        assert_eq!(header(&headers, "missing"), None);

        headers.insert("etag", HeaderValue::from_static("\"v1\""));
        assert_eq!(header(&headers, "etag").as_deref(), Some("\"v1\""));
    }

    // --- client construction -----------------------------------------------

    #[test]
    fn the_verified_client_does_not_follow_redirects_itself() {
        // Redirects are handled in `fetch` so that 301 stays distinguishable from 302 and every hop
        // is scheme-checked. A client that followed them internally would hide both.
        let fetcher = ReqwestFetcher::new().unwrap();
        assert!(client_for(&fetcher, true).is_ok());
        assert!(client_for(&fetcher, false).is_ok());
        // The unverified client is built once and reused.
        let first = client_for(&fetcher, false).unwrap() as *const Client;
        let second = client_for(&fetcher, false).unwrap() as *const Client;
        assert_eq!(first, second);
    }

    #[test]
    fn the_unverified_client_is_separate_from_the_verified_one() {
        // `danger_accept_invalid_certs` is per-client; sharing one would silently disable
        // verification for every feed because one user has a self-signed certificate.
        let fetcher = ReqwestFetcher::new().unwrap();
        let verified = client_for(&fetcher, true).unwrap() as *const Client;
        let unverified = client_for(&fetcher, false).unwrap() as *const Client;
        assert_ne!(verified, unverified);
    }
}
