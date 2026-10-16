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

use std::net::SocketAddr;
use std::time::Duration;

use futures::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, REFERER, USER_AGENT};
use reqwest::{Client, Method, StatusCode};

use super::guard::Guard;
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
    /// The network policy both clients obey.
    guard: Guard,
}

impl ReqwestFetcher {
    /// Builds a fetcher with a shared connection pool and **no** network guard.
    ///
    /// That is the right default for a tool run by an operator on their own machine — the CLI's
    /// `parse` command, where `http://localhost:8080/feed.xml` is a perfectly reasonable thing to
    /// want to look at — and the wrong default for a server that fetches URLs its users supply. A
    /// server should build its fetcher with [`ReqwestFetcher::with_guard`].
    ///
    /// # Errors
    ///
    /// Only if `reqwest` cannot build a client at all, which in practice means a TLS backend
    /// problem — fatal at startup, not something to retry per feed.
    pub fn new() -> Result<Self, FeedError> {
        Self::with_guard(Guard::default())
    }

    /// Builds a fetcher that refuses to contact the addresses `guard` excludes.
    ///
    /// # Errors
    ///
    /// As [`ReqwestFetcher::new`].
    pub fn with_guard(guard: Guard) -> Result<Self, FeedError> {
        Ok(Self {
            verified: base_client(true, &guard)?,
            unverified: once_cell::sync::OnceCell::new(),
            guard,
        })
    }

    /// The policy this fetcher applies. Callers that validate a URL before handing it over — the
    /// discovery dialog does, so it can say *why* a candidate was not tried — need it.
    pub fn guard(&self) -> &Guard {
        &self.guard
    }
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new().expect("reqwest client construction does not perform IO")
    }
}

fn base_client(ssl_verify: bool, guard: &Guard) -> Result<Client, FeedError> {
    let mut builder = Client::builder()
        // Redirects are handled in `fetch`, so the client must not follow them itself.
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(!ssl_verify)
        .tcp_nodelay(true)
        .pool_max_idle_per_host(4);
    if guard.block_private {
        // Installed on the client rather than checked per request, so that every connection the
        // client makes goes through it — including the ones a redirect chain starts, which the
        // caller cannot enumerate in advance.
        builder = builder.dns_resolver(GuardedResolver {
            guard: guard.clone(),
        });
    }
    builder
        .build()
        .map_err(|e| FeedError::BadConfig(e.to_string()))
}

/// Resolves names and drops the addresses the [`Guard`] excludes.
///
/// Only installed when the guard blocks anything at all; with it off, `reqwest`'s own resolver is
/// used unchanged.
struct GuardedResolver {
    guard: Guard,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let guard = self.guard.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((&*host, 0))
                .await
                .map_err(boxed)?
                .collect();
            let permitted = guard.permitted(&host, resolved.iter().map(SocketAddr::ip));
            if permitted.is_empty() {
                return Err(boxed(FeedError::NonRemote(format!(
                    "{host} resolves only to addresses this server is configured not to contact"
                ))));
            }
            // Port 0 stands for "the conventional port for the scheme", which `reqwest` fills in
            // from the URL — the resolver is not told which scheme is being used.
            let addrs = permitted.into_iter().map(|ip| SocketAddr::new(ip, 0));
            let addrs: reqwest::dns::Addrs = Box::new(addrs);
            Ok(addrs)
        })
    }
}

/// Boxes an error the way `reqwest`'s resolver trait wants it.
fn boxed(
    err: impl std::error::Error + Send + Sync + 'static,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(err)
}

#[async_trait::async_trait]
impl super::Fetcher for ReqwestFetcher {
    async fn fetch(&self, request: &FetchRequest) -> Result<FetchOutcome, FeedError> {
        let mut url = request.url.clone();
        let mut permanently_moved = false;

        for _ in 0..=MAX_REDIRECTS {
            validate_url(&url, &self.guard)?;
            let response = send(self, request, &url).await?;
            let status = response.status();

            // 304 is a 3xx, so `is_redirection()` is true for it — but it carries no `Location` and
            // must not be followed. It is the *answer* to the conditional GET we sent, and treating
            // it as a malformed redirect would turn every quiet feed into a permanently failing one
            // on its second refresh, which is the first refresh that replays a validator.
            if status == StatusCode::NOT_MODIFIED {
                return Ok(FetchOutcome::NotModified);
            }

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
    if let Some(referer) = &request.referer {
        if let Ok(v) = HeaderValue::from_str(referer) {
            builder = builder.header(REFERER, v);
        }
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
    let guard = fetcher.guard.clone();
    fetcher
        .unverified
        .get_or_try_init(move || base_client(false, &guard))
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
    // A refusal from the network guard arrives wrapped several layers deep, because it is reported
    // by the resolver and `reqwest` turns any resolver failure into a connect error. Recovering it
    // matters: "this server will not contact that address" and "the connection failed" send the user
    // to completely different fixes, and only the first one is not worth retrying.
    let mut cause = std::error::Error::source(&err);
    while let Some(inner) = cause {
        if let Some(FeedError::NonRemote(reason)) = inner.downcast_ref::<FeedError>() {
            return FeedError::NonRemote(reason.clone());
        }
        cause = inner.source();
    }
    FeedError::Transport(err)
}

/// Consumes a response that is neither a redirect nor a 304 into a [`FetchOutcome`].
///
/// Both of those are handled by the loop in [`Fetcher::fetch`]: a redirect has to be followed rather
/// than read, and a 304 has to be recognised as a 3xx that is *not* a redirect.
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

/// Refuses a URL that is not an absolute `http`/`https` one, or whose host is an address the guard
/// excludes.
///
/// Checked on every hop, not just the first. A feed author who can make us follow a redirect can
/// otherwise point us at `file:///etc/passwd` or at the cloud metadata endpoint, and the response
/// body would be parsed and stored as an article — readable by anyone who can open that feed.
///
/// Only a host written *as an address* is checked here; a hostname is checked when it is resolved,
/// by [`GuardedResolver`], because that is the only point at which its address is known.
fn validate_url(url: &str, guard: &Guard) -> Result<(), FeedError> {
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
    let host = match parsed.host() {
        Some(host @ (url::Host::Ipv4(_) | url::Host::Ipv6(_))) => host,
        Some(url::Host::Domain(domain)) if !domain.is_empty() => return Ok(()),
        _ => {
            return Err(FeedError::BadUrl {
                url: url.to_owned(),
                reason: "no host".to_owned(),
            })
        }
    };
    // An address literal never reaches the resolver, so this is the only chance to refuse it. It
    // also covers the forms that do not look like one: `url` parses `http://2130706433/` and
    // `http://0x7f.1/` into `127.0.0.1` before we ever see them.
    let ip = match host {
        url::Host::Ipv4(v4) => std::net::IpAddr::V4(v4),
        url::Host::Ipv6(v6) => std::net::IpAddr::V6(v6),
        url::Host::Domain(_) => unreachable!("domains returned above"),
    };
    if guard.allows_host(&ip.to_string()) || guard.allows_addr(&ip) {
        return Ok(());
    }
    Err(FeedError::NonRemote(format!(
        "{url} ({ip} is not an address this server is configured to contact)"
    )))
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
    use tokio::sync::oneshot;

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

    /// Validation with no network policy, which is what the scheme and host rules amount to.
    fn unguarded(url: &str) -> Result<(), FeedError> {
        validate_url(url, &Guard::default())
    }

    /// Validation with the policy a server builds from its config.
    fn guarded(url: &str) -> Result<(), FeedError> {
        validate_url(url, &Guard::new(true, &[]))
    }

    #[test]
    fn only_absolute_http_and_https_are_accepted() {
        for ok in [
            "http://example.com/f",
            "https://example.com/f",
            "https://example.com:8443/f?x=1",
        ] {
            assert!(unguarded(ok).is_ok(), "{ok}");
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
            assert!(unguarded(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_url_with_no_host_is_refused() {
        assert!(unguarded("http://").is_err());
        assert!(unguarded("http:///").is_err());
        // `http:///path` is *not* hostless: the URL parser reads `path` as the host, which is a
        // valid (if odd) target and none of this function's business.
        assert!(unguarded("http:///path").is_ok());
    }

    /// The address forms a guard has to catch. Each of these is `127.0.0.1` or `169.254.169.254`
    /// once the URL parser is done with it, which is exactly why the check is on the parsed host and
    /// not on the string.
    #[test]
    fn an_address_literal_is_checked_against_the_guard() {
        for bad in [
            "http://127.0.0.1/feed.xml",
            "http://127.1/feed.xml",
            "http://2130706433/feed.xml",
            "http://0x7f000001/feed.xml",
            "http://[::1]/feed.xml",
            "http://[::ffff:127.0.0.1]/feed.xml",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/feed.xml",
            "http://192.168.1.1/admin",
        ] {
            assert!(
                matches!(guarded(bad), Err(FeedError::NonRemote(_))),
                "{bad} must be refused as non-remote, not merely as a bad url: {:?}",
                guarded(bad)
            );
            assert!(unguarded(bad).is_ok(), "{bad} is fine with the guard off");
        }
        for ok in [
            "http://8.8.8.8/feed.xml",
            "http://93.184.216.34/feed.xml",
            "http://[2606:2800:220:1:248:1893:25c8:1946]/feed.xml",
        ] {
            assert!(guarded(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn an_allowlisted_address_may_be_fetched() {
        let guard = Guard::new(true, &["nas.lan".to_owned(), "192.168.1.50".to_owned()]);
        assert!(
            validate_url("http://192.168.1.50/feed.xml", &guard).is_ok(),
            "the operator said this one is fine"
        );
        assert!(validate_url("http://192.168.1.51/feed.xml", &guard).is_err());
        // A hostname is not resolved here, so it is neither refused nor cleared: the resolver is
        // what decides, and it has the same allowlist.
        assert!(validate_url("http://nas.lan/feed.xml", &guard).is_ok());
        assert!(validate_url("http://other.lan/feed.xml", &guard).is_ok());
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

    // --- a real socket ------------------------------------------------------
    //
    // `FixtureFetcher` answers a conditional GET itself, so it never exercises the status handling
    // in `fetch`. That is exactly how a 304 came to be read as a malformed redirect: the fixture
    // returned `NotModified` and the code that would have mishandled a real one was never reached.
    // These tests talk to a listener on loopback instead, which is still no network.

    /// Serves `replies` in order, one per connection, and hands back the base URL plus the request
    /// head each one received.
    ///
    /// Every reply is sent with `Connection: close`, so the client cannot reuse a socket the task
    /// has already stopped reading from.
    async fn serve(replies: Vec<String>) -> (String, oneshot::Receiver<Vec<String>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = listener.local_addr().expect("an address");
        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in replies {
                let (mut socket, _) = listener.accept().await.expect("a connection");
                let mut buf = vec![0u8; 4096];
                let read = socket.read(&mut buf).await.unwrap_or(0);
                seen.push(String::from_utf8_lossy(&buf[..read]).into_owned());
                socket
                    .write_all(reply.as_bytes())
                    .await
                    .expect("the reply was written");
                socket.shutdown().await.ok();
            }
            tx.send(seen).ok();
        });

        (format!("http://{addr}/feed.xml"), rx)
    }

    /// A raw HTTP/1.1 response, with `Connection: close` added.
    fn raw(status_line: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\n{headers}\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn a_real_304_is_not_read_as_a_redirect() {
        let (url, requests) = serve(vec![raw(
            "304 Not Modified",
            "ETag: \"v1\"\r\nLast-Modified: Thu, 01 Oct 2026 08:00:00 GMT",
            "",
        )])
        .await;

        let fetcher = ReqwestFetcher::new().expect("a client");
        let mut request = FetchRequest::new(url.clone());
        request.conditional.etag = Some("\"v1\"".to_owned());
        request.conditional.last_modified = Some("Thu, 01 Oct 2026 08:00:00 GMT".to_owned());

        let outcome = fetcher.fetch(&request).await.expect("fetched");
        assert_eq!(
            outcome,
            FetchOutcome::NotModified,
            "304 is a 3xx that is not a redirect; it has no Location and must not be followed"
        );

        let sent = requests.await.expect("the server reported");
        assert!(
            sent[0].contains("if-none-match: \"v1\""),
            "and the validator really was replayed: {}",
            sent[0]
        );
    }

    #[tokio::test]
    async fn a_real_301_is_followed_and_reported_as_permanent() {
        let body =
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>M</title></channel></rss>";
        let (url, requests) = serve(vec![
            raw("301 Moved Permanently", "Location: /moved.xml", ""),
            raw("200 OK", "Content-Type: application/rss+xml", body),
        ])
        .await;

        let fetcher = ReqwestFetcher::new().expect("a client");
        let found = match fetcher.fetch(&FetchRequest::new(url.clone())).await {
            Ok(FetchOutcome::Found(found)) => found,
            other => panic!("expected the redirect to be followed, got {other:?}"),
        };
        assert!(found.permanently_moved, "a 301 has to stay distinguishable");
        assert!(
            found.effective_url.ends_with("/moved.xml"),
            "{}",
            found.effective_url
        );
        assert_eq!(found.body, body.as_bytes());

        let sent = requests.await.expect("the server reported");
        assert_eq!(sent.len(), 2, "one request per hop");
        assert!(sent[1].starts_with("GET /moved.xml"), "{}", sent[1]);
    }

    #[tokio::test]
    async fn a_redirect_with_no_location_is_a_transport_failure_not_a_panic() {
        let (url, _requests) = serve(vec![raw("302 Found", "", "")]).await;
        let fetcher = ReqwestFetcher::new().expect("a client");
        let err = fetcher
            .fetch(&FetchRequest::new(url))
            .await
            .expect_err("there is nowhere to go");
        assert!(
            matches!(&err, FeedError::Http { status: 0, message } if message.contains("Location")),
            "{err:?}"
        );
        assert!(err.is_transient(), "so the next tick tries again");
    }

    #[tokio::test]
    async fn the_referer_reaches_the_wire_verbatim() {
        // Image hosts that accept only their own site as referer are the reason the field exists;
        // a header that is dropped or rewritten on the way out would look identical to a missing
        // one from the caller's side, so the raw request is what gets asserted.
        let (url, requests) = serve(vec![raw("200 OK", "Content-Type: image/webp", "xx")]).await;

        let fetcher = ReqwestFetcher::new().expect("a client");
        let mut request = FetchRequest::new(url.clone());
        request.referer = Some("https://www.example/article/".to_owned());
        fetcher.fetch(&request).await.expect("fetched");

        let sent = requests.await.expect("the server reported");
        assert!(
            sent[0].contains("referer: https://www.example/article/"),
            "{:?}",
            sent[0]
        );
    }
}
