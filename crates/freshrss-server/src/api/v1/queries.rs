//! Saved searches: the rows behind `S:`/`search:` references and the sidebar's query list.
//!
//! What is stored is the query *text*, re-parsed on every use, so a saved search keeps working
//! across grammar changes. What is checked on save is that the text parses — a saved search that
//! cannot be parsed would surface as an error inside whatever filter action or listing references
//! it, far from the box it was typed into.

use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use freshrss_search::parser::parse;

use crate::api::dto::serialize_id;
use crate::api::error::{ApiError, ApiResult};
use crate::api::extract::CurrentUser;
use crate::db::dao::user_query as query_dao;
use crate::domain::ids::now_unix;
use crate::domain::user_query::SavedQuery;
use crate::error::Error;
use crate::router::AppState;
use sqlx::SqliteConnection;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", get(show).put(update).delete(remove))
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedQueryDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub search: String,
    pub position: i64,
}

impl From<&SavedQuery> for SavedQueryDto {
    fn from(query: &SavedQuery) -> Self {
        Self {
            id: query.id,
            name: query.name.clone(),
            search: query.search.clone(),
            position: query.position,
        }
    }
}

#[derive(Deserialize)]
pub struct CreateQueryRequest {
    pub name: String,
    pub search: String,
}

#[derive(Deserialize)]
pub struct UpdateQueryRequest {
    pub name: Option<String>,
    pub search: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<SavedQueryDto>>> {
    let id_user = user.id;
    let dtos = state
        .db
        .with(move |mut conn| async move {
            Ok(query_dao::list(&mut conn, id_user)
                .await?
                .iter()
                .map(SavedQueryDto::from)
                .collect())
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dtos))
}

pub async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(req): Json<CreateQueryRequest>,
) -> ApiResult<impl IntoResponse> {
    let name = clean_name(&req.name)?;
    check_search(&req.search)?;

    let id_user = user.id;
    let dto = state
        .db
        .with(move |mut conn| async move {
            if query_dao::find_by_name(&mut conn, id_user, &name)
                .await?
                .is_some()
            {
                return Err(Error::Conflict(format!(
                    "a saved search named {name:?} exists"
                )));
            }
            let id =
                query_dao::insert(&mut conn, &SavedQuery::new(id_user, name, req.search)).await?;
            Ok(SavedQueryDto::from(&require(&mut conn, id, id_user).await?))
        })
        .await
        .map_err(ApiError)?;
    Ok((StatusCode::CREATED, Json(dto)))
}

pub async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<Json<SavedQueryDto>> {
    let id_user = user.id;
    let dto = state
        .db
        .with(move |mut conn| async move {
            Ok(SavedQueryDto::from(&require(&mut conn, id, id_user).await?))
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dto))
}

pub async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
    Json(req): Json<UpdateQueryRequest>,
) -> ApiResult<Json<SavedQueryDto>> {
    let name = req.name.as_deref().map(clean_name).transpose()?;
    if let Some(search) = req.search.as_deref() {
        check_search(search)?;
    }

    let id_user = user.id;
    let dto = state
        .db
        .with(move |mut conn| async move {
            let mut query = require(&mut conn, id, id_user).await?;
            if let Some(name) = name {
                if name != query.name
                    && query_dao::find_by_name(&mut conn, id_user, &name)
                        .await?
                        .is_some()
                {
                    return Err(Error::Conflict(format!(
                        "a saved search named {name:?} exists"
                    )));
                }
                query.name = name;
            }
            if let Some(search) = req.search {
                query.search = search;
            }
            query_dao::update(&mut conn, &query).await?;
            Ok(SavedQueryDto::from(&require(&mut conn, id, id_user).await?))
        })
        .await
        .map_err(ApiError)?;
    Ok(Json(dto))
}

/// Deletes a saved search. References to it (`S:<id>`) become errors where they are used, which is
/// the honest outcome: silently rewriting them into what they once meant would freeze a query the
/// user can no longer see or edit.
pub async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    PathParam(id): PathParam<i64>,
) -> ApiResult<impl IntoResponse> {
    let id_user = user.id;
    state
        .db
        .with(move |mut conn| async move {
            require(&mut conn, id, id_user).await?;
            query_dao::delete(&mut conn, id, id_user).await
        })
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn require(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
) -> crate::error::Result<SavedQuery> {
    query_dao::find_by_id(conn, id, id_user)
        .await?
        .ok_or_else(|| Error::NotFound(format!("saved search {id}")))
}

fn clean_name(raw: &str) -> Result<String, Error> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(Error::Validation("a saved search needs a name".into()));
    }
    Ok(name.to_owned())
}

/// Rejects a search that will not parse, at the point it is typed rather than at every later use.
fn check_search(text: &str) -> Result<(), Error> {
    if text.trim().is_empty() {
        return Err(Error::Validation(
            "a saved search needs a query, or it would match everything".into(),
        ));
    }
    parse(text, now_unix()).map_err(|err| Error::Validation(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_search_is_refused_rather_than_saved_as_match_everything() {
        for blank in ["", "   "] {
            assert!(
                matches!(check_search(blank), Err(Error::Validation(_))),
                "{blank:?}"
            );
        }
    }

    #[test]
    fn a_search_that_does_not_parse_is_refused_with_the_reason() {
        match check_search("intitle:a OR") {
            Err(Error::Validation(message)) => assert!(!message.is_empty(), "{message}"),
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn a_parseable_search_is_accepted() {
        assert!(check_search("intitle:rust -author:bot date:P1W").is_ok());
    }
}
