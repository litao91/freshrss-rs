//! The `/api/v1` route table.

use axum::{
    routing::{delete, get},
    Router,
};

use crate::router::AppState;

pub mod auth;
pub mod categories;
pub mod entries;
pub mod feeds;
pub mod io;
pub mod me;
pub mod packages;
pub mod queries;
pub mod refresh;
pub mod search;
pub mod tree;

/// All versioned API routes, mounted at `/api/v1`.
///
/// Kept in one place so the surface is reviewable at a glance: anything reachable by the SPA is
/// listed here, and anything not listed here is not reachable.
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/auth", auth::router())
        .route("/me", get(me::bootstrap))
        .route("/me/sessions", get(auth::list_sessions))
        .route("/me/sessions/{id}", delete(auth::revoke_session))
        .route("/tree", get(tree::tree))
        .route("/counts", get(tree::counts))
        .route("/problems", get(refresh::problems))
        .nest("/categories", categories::router())
        .nest("/feeds", feeds::router())
        .nest("/entries", entries::router())
        .nest("/queries", queries::router())
        .nest("/refresh", refresh::router())
        .nest("/search", search::router())
        .nest("/packages", packages::router())
        // OPML export/import live at the top level (`/export/opml`, `/import/opml`) rather than under a
        // prefix, so they are merged in rather than nested.
        .merge(io::router())
}
