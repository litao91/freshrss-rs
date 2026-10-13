//! Request extractors.

use axum::extract::FromRef;
use axum::{extract::FromRequestParts, http::request::Parts};

use crate::api::error::ApiError;
use crate::domain::user::UserConfig;
use crate::error::Error;
use crate::router::AppState;
use crate::service::auth;

/// An authenticated caller. Rejects with 401 when the session cookie is missing, expired or
/// revoked.
///
/// Carries the user's config so handlers can resolve per-user settings (`ttl_default`, sort order,
/// page size) without a second query.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub created_at: i64,
    pub config: UserConfig,
    /// The session this request arrived on, for "sign out this device".
    pub session_id: String,
}

impl CurrentUser {
    pub fn display_initial(&self) -> String {
        self.username
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_else(|| "?".into())
    }
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app = AppState::from_ref(state);
        let (user, session_id) = resolve(&app, parts).await?.ok_or(Error::Unauthorized)?;
        Ok(Self {
            id: user.id,
            username: user.username,
            email: user.email,
            is_admin: user.is_admin,
            created_at: user.created_at,
            config: user.config,
            session_id,
        })
    }
}

/// Like [`CurrentUser`], but yields `None` instead of rejecting. For endpoints that render
/// differently for a signed-out visitor — the bootstrap call, chiefly.
#[derive(Debug, Clone)]
pub struct MaybeUser(pub Option<CurrentUser>);

impl<S> FromRequestParts<S> for MaybeUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app = AppState::from_ref(state);
        Ok(Self(match resolve(&app, parts).await? {
            Some((user, session_id)) => Some(CurrentUser {
                id: user.id,
                username: user.username,
                email: user.email,
                is_admin: user.is_admin,
                created_at: user.created_at,
                config: user.config,
                session_id,
            }),
            None => None,
        }))
    }
}

/// Reads the session cookie and resolves it to a user.
///
/// Shared by both extractors so they cannot drift on what counts as a valid session.
async fn resolve(
    app: &AppState,
    parts: &Parts,
) -> Result<Option<(crate::domain::user::User, String)>, ApiError> {
    let cookie_name = app.config.session.cookie_name.clone();
    let header = match parts.headers.get(axum::http::header::COOKIE) {
        Some(value) => value.to_str().unwrap_or_default().to_string(),
        None => return Ok(None),
    };
    let Some(session_id) = crate::api::cookie::parse(&header, &cookie_name) else {
        return Ok(None);
    };
    if session_id.is_empty() {
        return Ok(None);
    }

    let user = auth::resolve_session(&app.db, &session_id).await?;
    Ok(user.map(|u| (u, session_id)))
}

/// True when the original request arrived over TLS.
///
/// Behind a reverse proxy the socket is plain HTTP and the real scheme is in `X-Forwarded-Proto`,
/// so that header takes precedence. Getting this wrong means either a `Secure` cookie the browser
/// refuses to store (behind a TLS-terminating proxy) or a session cookie sent over plain HTTP.
///
/// Takes the headers and URI separately rather than `Parts` so handlers that only have a
/// `HeaderMap` and a `Uri` in scope can use the same rule as the extractors — one definition, no
/// chance of the two drifting.
pub fn is_secure_request(headers: &axum::http::HeaderMap, uri: &axum::http::Uri) -> bool {
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        // A chain of proxies lists the client-facing one first.
        return proto
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("https");
    }
    uri.scheme().is_some_and(|s| s.as_str() == "https")
}

/// Convenience wrapper for callers that have the whole request parts.
pub fn request_is_secure(parts: &Parts) -> bool {
    is_secure_request(&parts.headers, &parts.uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Request, Uri};

    fn parts_with_cookie(value: Option<&str>, proto: Option<&str>) -> Parts {
        let mut builder = Request::builder().uri(Uri::from_static("https://example.com/api/v1/me"));
        if let Some(v) = value {
            builder = builder.header(
                axum::http::header::COOKIE,
                HeaderValue::from_str(v).unwrap(),
            );
        }
        if let Some(p) = proto {
            builder = builder.header("x-forwarded-proto", HeaderValue::from_str(p).unwrap());
        }
        builder.body(()).expect("request").into_parts().0
    }

    #[tokio::test]
    async fn a_missing_cookie_is_anonymous_not_an_error() {
        let app = AppState::new(
            crate::db::Db::in_memory().await.expect("db"),
            crate::config::AppConfig::default(),
        );
        let parts = parts_with_cookie(None, None);
        let resolved = resolve(&app, &parts).await.expect("resolve");
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn an_unknown_session_id_is_anonymous() {
        let app = AppState::new(
            crate::db::Db::in_memory().await.expect("db"),
            crate::config::AppConfig::default(),
        );
        let parts = parts_with_cookie(Some("frss_sid=doesnotexist"), None);
        assert!(resolve(&app, &parts).await.expect("resolve").is_none());
    }

    #[tokio::test]
    async fn an_empty_cookie_value_is_anonymous() {
        let app = AppState::new(
            crate::db::Db::in_memory().await.expect("db"),
            crate::config::AppConfig::default(),
        );
        let parts = parts_with_cookie(Some("frss_sid="), None);
        assert!(resolve(&app, &parts).await.expect("resolve").is_none());
    }

    #[tokio::test]
    async fn a_real_session_resolves_to_its_user() {
        use crate::service::auth::{ClientInfo, LoginRequest};
        let app = AppState::new(
            crate::db::Db::in_memory().await.expect("db"),
            crate::config::AppConfig::default(),
        );

        // The policy-free path: this test is about session resolution, not registration.
        auth::create_user(&app.db, "alice", None, "correct horse", false)
            .await
            .expect("user");
        let (_, session) = auth::login(
            &app.db,
            &app.config,
            LoginRequest {
                username: "alice".into(),
                password: "correct horse".into(),
            },
            ClientInfo::default(),
        )
        .await
        .expect("login");

        let parts = parts_with_cookie(Some(&format!("frss_sid={}", session.id)), None);
        let (user, sid) = resolve(&app, &parts)
            .await
            .expect("resolve")
            .expect("authenticated");
        assert_eq!(user.username, "alice");
        assert_eq!(sid, session.id);
    }

    #[test]
    fn secure_detection_prefers_the_proxy_header() {
        // Behind a TLS-terminating proxy the socket is plain HTTP.
        assert!(request_is_secure(&parts_with_cookie(None, Some("https"))));
        assert!(!request_is_secure(&parts_with_cookie(None, Some("http"))));
        // A chain of proxies: the client-facing one is first.
        assert!(request_is_secure(&parts_with_cookie(
            None,
            Some("https, http")
        )));
        assert!(!request_is_secure(&parts_with_cookie(
            None,
            Some("http, https")
        )));
    }

    #[test]
    fn secure_detection_falls_back_to_the_request_scheme() {
        assert!(
            request_is_secure(&parts_with_cookie(None, None)),
            "uri is https"
        );

        let mut parts = parts_with_cookie(None, None);
        parts.uri = Uri::from_static("http://example.com/api/v1/me");
        assert!(!request_is_secure(&parts));
    }

    #[test]
    fn display_initial_matches_the_account_menu_avatar() {
        let user = CurrentUser {
            id: 1,
            username: "lovelace".into(),
            email: None,
            is_admin: false,
            created_at: 0,
            config: UserConfig::default(),
            session_id: String::new(),
        };
        assert_eq!(user.display_initial(), "L");
    }
}
