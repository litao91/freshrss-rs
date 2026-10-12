//! The uniform JSON error envelope and the mapping from [`Error`] to an HTTP status.
//!
//! Every failure leaves the API in the same shape, so the frontend has one place that turns a
//! response into a `.rd-toast` message:
//!
//! ```json
//! { "error": { "code": "bad_search", "message": "unbalanced parenthesis at offset 12" } }
//! ```

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::error::Error;

/// Wraps [`Error`] so it can be returned from a handler.
///
/// Handlers return `Result<T, ApiError>`; the `From` impls below let `?` do the conversion, which
/// is the only reason this newtype exists.
#[derive(Debug)]
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        Self(err)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> Self {
        Self(Error::from(err))
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(err: tokio::task::JoinError) -> Self {
        Self(Error::from(err))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        Self(Error::Io(err))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        Self(Error::Json(err))
    }
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match &self.0 {
            Error::Unauthorized | Error::BadCredentials => StatusCode::UNAUTHORIZED,
            Error::Validation(_) | Error::Search(_) | Error::BadUrl { .. } => {
                StatusCode::BAD_REQUEST
            }
            Error::RegistrationClosed | Error::AccountDisabled => StatusCode::FORBIDDEN,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            // The feed server, not us, failed. 502 keeps that distinction visible to operators and
            // stops a monitoring system from paging about someone else's 404.
            Error::Feed(_) | Error::Http { .. } => StatusCode::BAD_GATEWAY,
            Error::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// True when the message is safe to show the user verbatim.
    ///
    /// Server-side failures carry details (SQL, paths, stack context) that are useful in a log and
    /// harmful in a response body, so those are replaced with a generic string and correlated by
    /// the `code`.
    fn is_message_public(&self) -> bool {
        self.0.is_client_error() || matches!(self.0, Error::Feed(_) | Error::Http { .. })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.0.code();

        if status.is_server_error() {
            // Log the real detail; the client only gets the code.
            tracing::error!(error = %self.0, code, %status, "request failed");
        } else {
            tracing::debug!(error = %self.0, code, %status, "request rejected");
        }

        let message = if self.is_message_public() {
            self.0.to_string()
        } else {
            "an internal error occurred".to_string()
        };

        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

/// Convenience alias for handler signatures.
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header;

    async fn envelope(err: Error) -> (StatusCode, serde_json::Value) {
        let res = ApiError(err).into_response();
        let status = res.status();
        let body = to_bytes(res.into_body(), usize::MAX).await.expect("body");
        (status, serde_json::from_slice(&body).expect("json"))
    }

    #[tokio::test]
    async fn client_errors_keep_their_status_and_message() {
        let cases = [
            (
                Error::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (
                Error::BadCredentials,
                StatusCode::UNAUTHORIZED,
                "bad_credentials",
            ),
            (
                Error::Validation("username must be between 2 and 64 characters".into()),
                StatusCode::BAD_REQUEST,
                "validation",
            ),
            (
                Error::RegistrationClosed,
                StatusCode::FORBIDDEN,
                "registration_closed",
            ),
            (
                Error::AccountDisabled,
                StatusCode::FORBIDDEN,
                "account_disabled",
            ),
            (
                Error::NotFound("feed 7".into()),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                Error::Conflict("that username is already taken".into()),
                StatusCode::CONFLICT,
                "conflict",
            ),
            (
                Error::BadUrl {
                    url: "x".into(),
                    reason: "no scheme".into(),
                },
                StatusCode::BAD_REQUEST,
                "bad_url",
            ),
        ];
        for (err, want_status, want_code) in cases {
            let (status, body) = envelope(err).await;
            assert_eq!(status, want_status, "{want_code}");
            assert_eq!(body["error"]["code"], want_code);
            assert!(
                !body["error"]["message"].as_str().unwrap().is_empty(),
                "{want_code} must carry a message the UI can show"
            );
        }
    }

    #[tokio::test]
    async fn upstream_feed_failures_are_502_with_a_readable_reason() {
        let err = Error::Http {
            status: 404,
            message: "the feed address was not found".into(),
        };
        let (status, body) = envelope(err).await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "the feed server failed, not us"
        );
        assert_eq!(body["error"]["code"], "http");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not found"),
            "the reader's problems dialog shows this text"
        );
    }

    #[tokio::test]
    async fn server_errors_hide_their_detail() {
        let (status, body) =
            envelope(Error::Internal("connection string with a password".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "internal");
        let message = body["error"]["message"].as_str().unwrap();
        assert_eq!(message, "an internal error occurred");
        assert!(
            !message.contains("password"),
            "internal detail must not reach the client"
        );
    }

    #[tokio::test]
    async fn sqlite_errors_are_reported_as_internal() {
        let (status, body) = envelope(Error::from(rusqlite::Error::QueryReturnedNoRows)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "db");
        assert_eq!(body["error"]["message"], "an internal error occurred");
    }

    #[tokio::test]
    async fn the_envelope_shape_is_stable() {
        let (_, body) = envelope(Error::NotFound("entry 1".into())).await;
        let keys: Vec<&str> = body["error"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["code", "message"]);
        assert_eq!(
            body.as_object().unwrap().len(),
            1,
            "only an `error` key at the top level"
        );
    }

    #[tokio::test]
    async fn responses_are_json() {
        let res = ApiError(Error::Unauthorized).into_response();
        assert_eq!(
            res.headers()
                .get(header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("application/json")
        );
    }
}
