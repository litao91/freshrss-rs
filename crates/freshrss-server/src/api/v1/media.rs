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
use freshrss_feed::http::{FetchOutcome, FetchRequest};

/// A week: article images do not change, and re-fetching them on every read would turn the reader
/// into a mirror that hammers the hosts it mirrors.
const CACHE_CONTROL: &str = "public, max-age=604800";

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
    if let Some(ip) = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .and_then(|h| h.parse::<std::net::IpAddr>().ok())
    {
        if !guard.allows_addr(&ip) {
            return Err(ApiError(Error::Validation(format!(
                "{ip} is not an address this server is configured to contact"
            ))));
        }
    }

    let settings = Settings::from_config(&state.config);
    let mut request = FetchRequest::new(url.clone());
    request.user_agent = settings.user_agent;
    request.timeout = settings.timeout;
    request.max_bytes = settings.max_bytes;
    request.accept = "image/*,*/*;q=0.8".to_owned();
    request.referer = query.via.as_deref().and_then(article_origin);

    let body = match state.fetcher.fetch(&request).await {
        Ok(FetchOutcome::Found(body)) => body,
        // A conditional request is never made here, so a 304 means the host misbehaved.
        Ok(FetchOutcome::NotModified) => {
            return Err(ApiError(Error::Http {
                status: 502,
                message: "the image host answered 304 to an unconditional request".into(),
            }))
        }
        Err(err) => {
            return Err(ApiError(Error::Http {
                status: 502,
                message: format!("the image host refused: {err}"),
            }))
        }
    };

    let mime = body
        .content_type
        .as_deref()
        .map(|c| c.split(';').next().unwrap_or(c).trim())
        .unwrap_or("")
        .to_owned();
    // Without this check the endpoint would proxy anything anyone points it at, which is the
    // open proxy this handler exists to not be.
    if !mime.starts_with("image/") {
        return Err(ApiError(Error::Http {
            status: 502,
            message: format!("the address is not an image (it answered {mime:?})"),
        }));
    }

    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, CACHE_CONTROL.to_owned()),
        ],
        body.body,
    )
        .into_response())
}

/// The origin of the article an image was found in, for use as `Referer`.
fn article_origin(via: &str) -> Option<String> {
    let url = url::Url::parse(via).ok()?;
    let scheme = url.scheme();
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{scheme}://{host}{port}/"))
}
