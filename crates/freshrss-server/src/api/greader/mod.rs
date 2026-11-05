//! The Google Reader compatible API, at `/api/greader.php` — the one FreshRSS exposes and the one
//! client apps (FeedMe, ReadYou, Unread, News+, EasyRSS) speak when pointed at a FreshRSS server.
//!
//! Ported from `p/api/greader.php` in the PHP source, quirks included where clients depend on them:
//! continuation tokens that are inclusive decimal microsecond ids with a fetch-one-more-and-drop-
//! first compensation, item ids as 16-digit hex tag URIs in one endpoint and plain decimal in the
//! next, an action token that FeedMe sends empty and Reeder sends as `x`, and `output=json` being
//! mandatory on exactly three endpoints because no XML output was ever implemented upstream.
//!
//! Authentication is stateless and separate from the web session: `POST /accounts/ClientLogin`
//! exchanges the per-user API password for `Auth=<user>/<sha1>`, and every later request replays
//! that pair in the `Authorization` header. There is deliberately no session row and no expiry —
//! the token is recomputed from the stored hash on each request, which is also the only revocation
//! the scheme has (changing the API password invalidates every token derived from it).

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde_json::json;

use crate::api::error::{ApiError, ApiResult};
use crate::config::AppConfig;
use crate::domain::user::User;
use crate::error::Error;
use crate::router::AppState;
use crate::service::auth;

pub mod dto;
pub mod escape;
pub mod mutate;
pub mod streams;

/// `Authorization: GoogleLogin auth=<user>/<token>`, the only credential path.
const GOOGLE_LOGIN: &str = "GoogleLogin ";

/// The username grammar ClientLogin accepts, verbatim from the PHP.
const USERNAME_RE: &str = r"^([0-9a-zA-Z_][0-9a-zA-Z_.@\-]{1,38}|[0-9a-zA-Z])$";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/accounts/ClientLogin", get(client_login).post(client_login))
        .route("/reader/api/0/token", get(action_token))
        .route("/check/compatibility", get(compatibility))
        .fallback(dispatch)
        .layer(axum::middleware::from_fn(compat_headers))
}

/// The CORS headers every response carries, and the `OPTIONS` short-circuit.
///
/// Client apps are native and web wrappers exist, so the PHP answered every request with a
/// permissive CORS set; reproducing it is cheaper than discovering which client needed it.
async fn compat_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        apply_cors(&mut res);
        return res;
    }
    let mut res = next.run(request).await;
    apply_cors(&mut res);
    res
}

fn apply_cors(res: &mut Response) {
    let headers = res.headers_mut();
    headers.insert("access-control-allow-headers", "Authorization".parse().unwrap());
    headers.insert("access-control-allow-methods", "GET, POST".parse().unwrap());
    headers.insert("access-control-allow-origin", "*".parse().unwrap());
    headers.insert("access-control-max-age", "600".parse().unwrap());
}

/// A text/plain answer, which is what every non-JSON response in this API is.
pub fn plain(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
        body,
    )
        .into_response()
}

/// 401 with the header clients use to tell "wrong token" from a network fault.
pub fn unauthorized() -> Response {
    let mut res = plain(StatusCode::UNAUTHORIZED, "Unauthorized!");
    res.headers_mut()
        .insert("google-bad-token", "true".parse().unwrap());
    res
}

/// JSON the way this API writes it: unescaped slashes and unicode, terminated by a newline.
pub fn json_body(value: serde_json::Value) -> Response {
    let mut text = value.to_string();
    text.push('\n');
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=UTF-8")],
        text,
    )
        .into_response()
}

fn api_disabled(state: &AppState) -> bool {
    !state.config.api.enabled
}

/// Splits `GoogleLogin auth=<user>/<token>` into its halves.
///
/// `parse_str` on the header value is what the PHP does, which is why the space after the scheme
/// becomes an underscore-separated key; matching on the prefix directly is the same grammar with
/// one less surprise.
fn header_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = value.strip_prefix(GOOGLE_LOGIN)?.trim();
    let rest = rest.strip_prefix("auth=").unwrap_or(rest);
    let (user, token) = rest.split_once('/')?;
    if user.is_empty() || token.is_empty() {
        return None;
    }
    Some((user.to_owned(), token.to_owned()))
}

/// Resolves the request's user from the Authorization header, or `None`.
async fn request_user(state: &AppState, headers: &HeaderMap) -> crate::error::Result<Option<User>> {
    let Some((username, token)) = header_credentials(headers) else {
        return Ok(None);
    };
    auth::greader_user(&state.db, &state.config, &username, &token).await
}

async fn client_login(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if api_disabled(&state) {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable!");
    }
    // POST form body first, query string as the deprecated fallback the PHP still accepts.
    let form = form_pairs(&body);
    let email = form
        .get("Email")
        .or_else(|| query.get("Email"))
        .cloned()
        .unwrap_or_default();
    let passwd = form
        .get("Passwd")
        .or_else(|| query.get("Passwd"))
        .cloned()
        .unwrap_or_default();

    let re = regex::Regex::new(USERNAME_RE).expect("static pattern");
    if !re.is_match(email.trim()) {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    match auth::client_login(&state.db, &email, &passwd).await {
        Ok(user) => {
            let Some(token) = auth::greader_token(&state.config, &user) else {
                return unauthorized();
            };
            let body = format!("SID={email}/{token}\nLSID=null\nAuth={email}/{token}\n");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
                body,
            )
                .into_response()
        }
        Err(Error::NotFound(_)) => plain(StatusCode::BAD_REQUEST, "Bad Request!"),
        Err(_) => unauthorized(),
    }
}

async fn action_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if api_disabled(&state) {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable!");
    }
    let user = match request_user(&state, &headers).await {
        Ok(Some(user)) => user,
        Ok(None) => return unauthorized(),
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    match auth::greader_action_token(&state.config, &user) {
        Some(token) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
            format!("{token}\n"),
        )
            .into_response(),
        None => unauthorized(),
    }
}

async fn compatibility() -> Response {
    plain(StatusCode::OK, "PASS")
}

/// Everything under `/reader/api/0/…`, dispatched on the path the way the PHP's `pathInfos` does.
async fn dispatch(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    Query(query): Query<std::collections::HashMap<String, String>>,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    if api_disabled(&state) {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable!");
    }
    let path = uri.path().to_owned();
    let path = strip_base(&path);
    if path.is_empty() && uri.query().is_none() {
        return plain(StatusCode::OK, "OK");
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }

    let user = match request_user(&state, &headers).await {
        Ok(Some(user)) => user,
        Ok(None) => return unauthorized(),
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };

    if segments[0] != "reader" || segments[1] != "api" || segments[2] != "0" {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    let rest = &segments[3..];
    let ctx = Ctx {
        state: &state,
        user: &user,
        method: &method,
        query: &query,
        body: &body,
        headers: &headers,
    };
    match rest {
        ["user-info"] => json_body(json!({
            "userId": user.username,
            "userName": user.username,
            "userProfileId": user.username,
            "userEmail": user.email.clone().unwrap_or_else(|| user.username.clone()),
        })),
        ["token"] => action_token(State(state), headers).await,
        ["tag", "list"] => streams::tag_list(ctx).await,
        ["subscription", "list"] => streams::subscription_list(ctx).await,
        ["subscription", "export"] => streams::subscription_export(ctx).await,
        ["subscription", "import"] => mutate::subscription_import(ctx).await,
        ["subscription", "edit"] => mutate::subscription_edit(ctx).await,
        ["subscription", "quickadd"] => mutate::subscription_quickadd(ctx).await,
        ["unread-count"] => streams::unread_count(ctx).await,
        ["stream", "contents", rest @ ..] => streams::contents(ctx, rest).await,
        ["stream", "items", "ids"] => streams::item_ids(ctx).await,
        ["stream", "items", "contents"] => streams::items_contents(ctx).await,
        ["edit-tag"] => mutate::edit_tag(ctx).await,
        ["mark-all-as-read"] => mutate::mark_all_as_read(ctx).await,
        ["rename-tag"] => mutate::rename_tag(ctx).await,
        ["disable-tag"] => mutate::disable_tag(ctx).await,
        _ => plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    }
}

/// Removes the mount prefix, including a duplicated one the client may have sent.
fn strip_base(path: &str) -> String {
    let mut p = path.to_owned();
    for _ in 0..2 {
        if let Some(stripped) = p.strip_prefix("/api/greader.php") {
            p = stripped.to_owned();
        } else if let Some(stripped) = p.strip_prefix("/api/greader") {
            p = stripped.to_owned();
        }
    }
    percent_decode_path(&p)
}

/// PATH_INFO arrives raw-url-decoded in PHP; axum gives us the encoded form.
fn percent_decode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex: String = bytes.by_ref().take(2).map(char::from).collect();
            match u8::from_str_radix(&hex, 16) {
                Ok(v) => out.push(v as char),
                Err(_) => {
                    out.push('%');
                    out.push_str(&hex);
                }
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

/// What every endpoint handler needs. Borrowed rather than extracted, because the dispatch has
/// already paid for parsing the body and the headers once.
pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub user: &'a User,
    pub method: &'a Method,
    pub query: &'a std::collections::HashMap<String, String>,
    pub body: &'a Bytes,
    pub headers: &'a HeaderMap,
}

impl Ctx<'_> {
    pub fn config(&self) -> &AppConfig {
        &self.state.config
    }

    /// A query or form parameter, query first: the PHP reads most of them from `$_REQUEST`.
    pub fn param(&self, name: &str) -> Option<String> {
        self.query.get(name).cloned().or_else(|| self.form().get(name).cloned())
    }

    /// The form-decoded POST body, parsed once per call site that needs it.
    pub fn form(&self) -> std::collections::HashMap<String, String> {
        form_pairs(self.body)
    }

    /// Repeated form keys (`i`, `a`, `r`, `s`, `t`), which a map cannot hold: the PHP re-parses the
    /// raw body for exactly this reason, and so does this.
    pub fn repeated(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name}=");
        let mut out = Vec::new();
        let text = String::from_utf8_lossy(self.body);
        for chunk in text.split('&') {
            if let Some(value) = chunk.strip_prefix(&prefix) {
                out.push(decode_component(value));
            }
        }
        if out.is_empty() {
            if let Some(one) = self.param(name) {
                out.push(one);
            }
        }
        out
    }

    /// The `T` action-token check, with the two holes clients depend on: FeedMe sends nothing and
    /// Reeder sends `x`, and both have to work or those apps silently lose every mutation.
    pub fn check_token(&self) -> bool {
        let given = self.form().get("T").cloned().unwrap_or_default();
        let given = given.trim();
        if given.is_empty() || given == "x" {
            return true;
        }
        match auth::greader_action_token(self.config(), self.user) {
            Some(expected) => expected == given,
            None => false,
        }
    }
}

/// A form-urlencoded body as a map; repeated keys keep the last value, and the repeated-key
/// readers use [`Ctx::repeated`] instead, which is the whole reason this API re-parses raw bodies.
fn form_pairs(body: &[u8]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let text = String::from_utf8_lossy(body);
    for chunk in text.split('&').filter(|c| !c.is_empty()) {
        let (key, value) = match chunk.split_once('=') {
            Some((k, v)) => (k, v),
            None => (chunk, ""),
        };
        out.insert(decode_component(key), decode_component(value));
    }
    out
}

/// `application/x-www-form-urlencoded` component decoding, `+` included.
fn decode_component(value: &str) -> String {
    let replaced = value.replace('+', " ");
    let mut out = String::with_capacity(replaced.len());
    let mut bytes = replaced.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex: String = bytes.by_ref().take(2).map(char::from).collect();
            match u8::from_str_radix(&hex, 16) {
                Ok(v) => out.push(v as char),
                Err(_) => {
                    out.push('%');
                    out.push_str(&hex);
                }
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

/// The 401/503 guards as an `ApiResult`, for the handlers that prefer `?` over matching.
pub fn unauthorized_error() -> ApiResult<Response> {
    Err(ApiError(Error::Unauthorized))
}
