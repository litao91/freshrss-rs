//! The bootstrap payload.
//!
//! `GET /api/v1/me` returns everything the SPA needs to render its first frame — user, config, the
//! whole sidebar tree and the version — so mounting does not depend on a waterfall of follow-up
//! requests. On a slow connection that is the difference between a reader that appears instantly
//! and one that visibly assembles itself.

use axum::{extract::State, Json};

use crate::api::dto::{BootstrapDto, UserConfigDto, UserDto, VersionDto};
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::meta;
use crate::db::migrate;
use crate::router::AppState;
use crate::service::tree;

pub async fn bootstrap(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<BootstrapDto>> {
    let id_user = user.id;
    let config = user.config.clone();
    let user_dto = UserDto {
        id: user.id,
        username: user.username.clone(),
        email: user.email.clone(),
        is_admin: user.is_admin,
        created_at: user.created_at,
        initial: user.display_initial(),
    };

    let (tree, schema) = state
        .db
        .with(move |conn| {
            let schema = meta::get(conn, "schema_version")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(migrate::SCHEMA_VERSION);
            Ok((tree::build(conn, id_user)?, schema))
        })
        .await
        .map_err(ApiError)?;

    Ok(Json(BootstrapDto {
        user: user_dto,
        config: UserConfigDto::from(&config),
        tree,
        version: VersionDto {
            freshrss_rs: env!("CARGO_PKG_VERSION").to_string(),
            schema,
        },
    }))
}
