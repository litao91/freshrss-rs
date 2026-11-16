//! `GET /api/v1/media`: fetching article images on the reader's behalf.
//!
//! Image hosts police the `Referer` header in both directions and a browser cannot satisfy either
//! policy from inside the reader: some refuse every referer but their own site's, and the only
//! referer a browser can send is the reader's own origin; others refuse foreign referers outright.
//! A page rendered from `http://127.0.0.1:8099` is foreign to all of them. Routing the image
//! through here lets the request carry the *article's* address as referer — what those hosts
//! actually check — and keeps the reader's address out of third-party logs besides.
//!
//! The endpoint is authenticated on purpose: an image proxy anyone on the internet may use is an
//! open proxy, and the SSRF guard applies to it exactly as it does to feed fetches.

use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::error::Error;
use crate::router::AppState;
use crate::service::refresh::Settings;
use freshrss_feed::http::{FetchOutcome, FetchRequest, FetchedBody};

/// A week: article images do not change, and re-fetching them on every read would turn the reader
/// into a mirror that hammers the hosts it mirrors.
const CACHE_CONTROL: &str = "public, max-age=604800";

/// Image CDNs and WAFs routinely refuse non-browser agents, and the feed bot UA is exactly what
/// they block. For images the reader is acting as the reader's browser, so present as one.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    pub url: String,
    /// The article the image came from; its origin is replayed as `Referer`.
    #[serde(default)]
    pub via: Option<String>,
}

pub async fn proxy(
    State(state): State<AppState>,
    _user: CurrentUser,
    Query(query): Query<MediaQuery>,
) -> ApiResult<Response> {
    let url = query.url.trim().to_owned();
    let guard = crate::router::guard_from(&state.config.http);
    // The same refusal a feed fetch gets: a media URL is still a URL this server was asked to go
    // and fetch, and the guard is what stands between that and the internal network.
    let parsed = url::Url::parse(&url).ok();
    let scheme_ok = parsed
        .as_ref()
        .is_some_and(|u| u.scheme() == "http" || u.scheme() == "https");
    if !scheme_ok {
        return Err(ApiError(Error::Validation(
            "only http and https addresses can be proxied".into(),
        )));
    }
    // An address written as a literal is checked here, because there is no resolution step left to
    // catch it; a name is checked where every other fetch is checked, in the guarded resolver.
    // `allows_host` first: the guard keeps a bare-address allowlist entry as a host entry, so
    // `internal_host_allowlist = ["192.168.1.50"]` exempts the literal by name and an image on a
    // LAN host the operator allowlisted is not refused here while its feed is fetched happily.
    let literal_host = parsed.as_ref().and_then(|u| u.host_str());
    if let Some(ip) = literal_host.and_then(|h| h.parse::<std::net::IpAddr>().ok()) {
        let allowlisted = literal_host.is_some_and(|h| guard.allows_host(h));
        if !allowlisted && !guard.allows_addr(&ip) {
            return Err(ApiError(Error::Validation(format!(
                "{ip} is not an address this server is configured to contact"
            ))));
        }
    }

    let cache_root = state.config.media.cache_dir.clone();
    let max_bytes = state.config.media.max_mb.saturating_mul(1024 * 1024);

    // A copy fetched once is served forever: article images do not change in place, and serving
    // the local copy is what keeps a slow, rate-limiting or briefly down host from blanking a
    // picture the reader has already seen.
    if let Some(cached) = crate::service::media::get(&cache_root, &url) {
        return Ok(serve(cached.mime, cached.body));
    }

    let settings = Settings::from_config(&state.config);
    let mut request = FetchRequest::new(url.clone());
    request.user_agent = BROWSER_UA.to_owned();
    request.timeout = settings.timeout;
    request.max_bytes = settings.max_bytes;
    request.accept = "image/*,*/*;q=0.8".to_owned();

    let body = fetch_with_fallbacks(&state, &mut request, query.via.as_deref(), &url)
        .await
        .map_err(ApiError)?;

    let declared = body
        .content_type
        .as_deref()
        .map(|c| c.split(';').next().unwrap_or(c).trim())
        .unwrap_or("")
        .to_owned();
    // Hosts mislabel images as octet-stream or omit the type entirely; trust the bytes over the
    // header, but only for signatures recognised as raster images, so this stays an image proxy
    // rather than the open proxy the handler exists to not be.
    let mime = if declared.starts_with("image/") {
        declared
    } else {
        match crate::service::media::sniff_image_mime(&body.body) {
            Some(sniffed) => sniffed.to_owned(),
            None => {
                return Err(ApiError(Error::Http {
                    status: 502,
                    message: format!("the address is not an image (it answered {declared:?})"),
                }));
            }
        }
    };

    crate::service::media::put(&cache_root, &url, &body.body, &mime, max_bytes);
    Ok(serve(mime, body.body))
}

/// The two headers an image is served with, cached copy or fresh fetch alike.
fn serve(mime: String, body: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, CACHE_CONTROL.to_owned()),
        ],
        body,
    )
        .into_response()
}

/// Fetches the image, varying the `Referer` across attempts rather than repeating one refusal.
///
/// Image hosts blip — a timeout, a 502 of their own, a rate limiter having a moment — and on a
/// cache miss there is no older copy to fall back on, so the reader's image is whatever this
/// returns. But the more common blank is a *policy* refusal, not a blip: hotlink protection that
/// rejects a foreign `Referer` (the article's origin is foreign to the image CDN) while accepting
/// none, or one that wants the image's own host. Repeating the identical request teaches it
/// nothing; walking the referer candidates does. The article origin goes first because it is what
/// same-site checks want and it keeps the reader's address out of third-party logs.
async fn fetch_with_fallbacks(
    state: &AppState,
    request: &mut FetchRequest,
    via: Option<&str>,
    url: &str,
) -> Result<FetchedBody, Error> {
    let mut candidates: Vec<Option<String>> = Vec::new();
    for candidate in [via.and_then(article_origin), None, article_origin(url)] {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    let mut last = None;
    for referer in candidates {
        request.referer = referer;
        match state.fetcher.fetch(request).await {
            Ok(FetchOutcome::Found(body)) => return Ok(body),
            // A conditional request is never made here, so a 304 means the host misbehaved, and
            // another referer is not going to teach it better manners.
            Ok(FetchOutcome::NotModified) => {
                return Err(Error::Http {
                    status: 502,
                    message: "the image host answered 304 to an unconditional request".into(),
                })
            }
            Err(err) => last = Some(err),
        }
    }
    Err(Error::Http {
        status: 502,
        message: format!(
            "the image host refused: {}",
            last.expect("at least one referer candidate, all of them failures")
        ),
    })
}

/// The origin of the article an image was found in, for use as `Referer`.
fn article_origin(via: &str) -> Option<String> {
    let url = url::Url::parse(via).ok()?;
    let scheme = url.scheme();
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{scheme}://{host}{port}/"))
}
