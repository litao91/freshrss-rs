//! The Fever compatible API, at `/api/fever.php`.
//!
//! Ported from `p/api/fever.php`. Fever is a much smaller protocol than Google Reader's: one
//! endpoint, dispatch on which keys are present, one JSON envelope. The port keeps the shapes
//! clients parse — `api_version` 4 (the docs say 3, the code says 4), item ids as decimal strings,
//! `favicon_id` equal to the feed id, `is_spark` hardcoded 0, a hardcoded 50-item page — and
//! deliberately does not keep one upstream bug: a disabled user whose key file existed was served
//! anyway, because `isAuthenticatedApiUser` returned "a user config is loaded" rather than "the
//! credential checked out". Here a disabled account gets `auth: 0` like any wrong key.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::Engine as _;
use serde_json::{json, Value};

use crate::api::greader::escape::{decode_url, escape_to_unicode_alternative};
use crate::db::dao::entry_query::{list, EntryQuery};
use crate::db::dao::{
    category as cat_dao, entry as entry_dao, favicon as favicon_dao, feed as feed_dao,
};
use crate::domain::ids::EntryId;
use crate::domain::list_type::ListType;
use crate::domain::state::EntryState;
use crate::router::AppState;
use crate::service::auth;
use crate::service::tree;

const API_LEVEL: i64 = 4;
const PAGE: usize = 50;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(fever).post(fever))
}

fn envelope(auth: i64, payload: Value, last_refreshed: Option<i64>) -> Response {
    let mut body = json!({ "api_version": API_LEVEL, "auth": auth });
    if auth == 1 {
        body["last_refreshed_on_time"] = json!(last_refreshed.unwrap_or(0));
        if let Value::Object(payload) = payload {
            for (key, value) in payload {
                body[key] = value;
            }
        }
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=UTF-8")],
        body.to_string(),
    )
        .into_response()
}

fn denied() -> Response {
    envelope(0, Value::Null, None)
}

async fn fever(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !state.config.api.enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
            "Service Unavailable!",
        )
            .into_response();
    }
    let form = form(&body);
    // `refresh` is refused before authentication, exactly as upstream refuses it.
    if query.contains_key("refresh") || form.contains_key("refresh") {
        return (
            StatusCode::NOT_IMPLEMENTED,
            [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
            "Not Implemented!",
        )
            .into_response();
    }

    // The key travels in the POST body only: a query-string key never authenticated upstream and
    // never does here, which also keeps it out of proxy and server logs.
    let key = form.get("api_key").cloned().unwrap_or_default();
    let key: String = key.trim().chars().take(128).collect();
    let hex = !key.is_empty() && key.chars().all(|c| c.is_ascii_hexdigit());
    let user = if hex {
        auth::fever_user(&state.db, &key.to_lowercase())
            .await
            .ok()
            .flatten()
            .filter(|u| u.enabled)
    } else {
        None
    };
    let Some(user) = user else {
        return denied();
    };

    // Every other parameter accepts GET or POST, which is what `$_REQUEST` gave the PHP.
    let param = |name: &str| query.get(name).cloned().or_else(|| form.get(name).cloned());
    let has = |name: &str| query.contains_key(name) || form.contains_key(name);

    let id_user = user.id;
    let last_refreshed = state
        .db
        .with(move |mut conn| async move {
            feed_dao::least_recently_refreshed(&mut conn, id_user).await
        })
        .await
        .ok()
        .flatten();

    let mut payload = serde_json::Map::new();
    let mut feeds_groups: Option<Value> = None;

    if has("groups") || has("feeds") {
        match subscription_tree(&state, user.id).await {
            Ok((groups, feeds, mapping)) => {
                if has("groups") {
                    payload.insert("groups".to_owned(), groups);
                }
                if has("feeds") {
                    payload.insert("feeds".to_owned(), feeds);
                }
                feeds_groups = Some(mapping);
            }
            Err(_) => return denied(),
        }
    }
    if has("favicons") {
        match favicon_list(&state, user.id).await {
            Ok(list) => {
                payload.insert("favicons".to_owned(), list);
            }
            Err(_) => return denied(),
        }
    }
    if has("items") {
        match items(&state, &param, user.id).await {
            Ok((total, items)) => {
                payload.insert("total_items".to_owned(), json!(total));
                payload.insert("items".to_owned(), items);
            }
            Err(_) => return denied(),
        }
    }
    if has("links") {
        payload.insert("links".to_owned(), json!([]));
    }
    if has("unread_item_ids") {
        let ids = id_list(&state, user.id, EntryState::from_bits(2)).await;
        payload.insert("unread_item_ids".to_owned(), json!(ids));
    }
    if has("saved_item_ids") {
        let ids = id_list(&state, user.id, EntryState::from_bits(4)).await;
        payload.insert("saved_item_ids".to_owned(), json!(ids));
    }
    if let (Some(mark), Some(as_)) = (param("mark"), param("as")) {
        if let Some(key) = apply_mark(&state, user.id, &mark, &as_, &param).await {
            let bits = if key == "unread_item_ids" { 2 } else { 4 };
            let ids = id_list(&state, user.id, EntryState::from_bits(bits)).await;
            payload.insert(key.to_owned(), json!(ids));
        }
    }
    if let Some(mapping) = feeds_groups {
        payload.insert("feeds_groups".to_owned(), mapping);
    }

    envelope(1, Value::Object(payload), last_refreshed)
}

/// `application/x-www-form-urlencoded` body as a map.
fn form(body: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let text = String::from_utf8_lossy(body);
    for chunk in text.split('&').filter(|c| !c.is_empty()) {
        let (key, value) = chunk.split_once('=').unwrap_or((chunk, ""));
        out.insert(decode(key), decode(value));
    }
    out
}

fn decode(value: &str) -> String {
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

/// groups, feeds and feeds_groups in one pass over the subscription list.
async fn subscription_tree(
    state: &AppState,
    id_user: i64,
) -> crate::error::Result<(Value, Value, Value)> {
    let (feeds, categories) = state
        .db
        .with(move |mut conn| async move {
            let feeds = feed_dao::list(&mut conn, id_user).await?;
            let categories = cat_dao::list(&mut conn, id_user).await?;
            Ok((feeds, categories))
        })
        .await?;
    let visible: Vec<_> = feeds
        .into_iter()
        .filter(|f| f.priority.get() > crate::domain::priority::Priority::HIDDEN.get())
        .collect();

    let groups: Vec<Value> = categories
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "title": escape_to_unicode_alternative(&c.name, true),
            })
        })
        .collect();
    let feeds_json: Vec<Value> = visible
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "favicon_id": f.id,
                "title": escape_to_unicode_alternative(&f.name, true),
                "url": decode_url(&f.url),
                "site_url": decode_url(&f.website),
                "is_spark": 0,
                "last_updated_on_time": f.last_update,
            })
        })
        .collect();
    let mut by_category: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
    for feed in &visible {
        if let Some(category) = feed.category {
            by_category
                .entry(category)
                .or_default()
                .push(feed.id.to_string());
        }
    }
    let mapping: Vec<Value> = by_category
        .into_iter()
        .map(|(group_id, ids)| json!({ "group_id": group_id, "feed_ids": ids.join(",") }))
        .collect();
    Ok((json!(groups), json!(feeds_json), json!(mapping)))
}

/// Cached favicons as `mime;base64,…` payloads, omitting feeds with no cached icon.
async fn favicon_list(state: &AppState, id_user: i64) -> crate::error::Result<Value> {
    let feeds = state
        .db
        .with(move |mut conn| async move { feed_dao::list(&mut conn, id_user).await })
        .await?;
    let mut out = Vec::new();
    for feed in feeds {
        if feed.priority.get() <= crate::domain::priority::Priority::HIDDEN.get() {
            continue;
        }
        let id_feed = feed.id;
        let icon = state
            .db
            .with(move |mut conn| async move { favicon_dao::find(&mut conn, id_feed).await })
            .await?;
        let Some(icon) = icon else { continue };
        let data = format!(
            "{};base64,{}",
            icon.content_type,
            base64::engine::general_purpose::STANDARD.encode(&icon.body)
        );
        out.push(json!({ "id": feed.id, "data": data }));
    }
    Ok(json!(out))
}

/// The 50-item page, with `max_id` > `with_ids` > `since_id` and `feed_ids` > `group_ids`.
async fn items(
    state: &AppState,
    param: &impl Fn(&str) -> Option<String>,
    id_user: i64,
) -> crate::error::Result<(i64, Value)> {
    let digit_list = |name: &str| -> Vec<i64> {
        param(name)
            .map(|raw| {
                raw.split(',')
                    .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
                    .filter_map(|p| p.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    // An all-dropped id list means "no filter" upstream rather than "no rows", which is surprising
    // enough to keep deliberately: a client that sends stale ids gets everything, not nothing.
    let feed_filter: Option<Vec<i64>> = if param("feed_ids").is_some() {
        Some(digit_list("feed_ids"))
    } else if let Some(raw) = param("group_ids") {
        Some(category_feeds(state, id_user, &raw).await?)
    } else {
        None
    };
    let feed_filter = feed_filter.filter(|ids| !ids.is_empty());

    let digits = |name: &str| -> Option<i64> {
        param(name)
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
            .and_then(|v| v.parse().ok())
    };
    let max_id = digits("max_id");
    let with_ids = param("with_ids").map(|_| digit_list("with_ids"));
    let since_id = digits("since_id");

    let rows = match with_ids {
        Some(ids) if !ids.is_empty() => {
            let ids: Vec<EntryId> = ids.into_iter().map(EntryId::from_micros).collect();
            let mut rows = state
                .db
                .with(move |mut conn| async move {
                    entry_dao::list_by_ids(&mut conn, id_user, &ids).await
                })
                .await?;
            rows.sort_by_key(|e| e.id.micros());
            rows
        }
        _ => {
            let (ascending, bound) = if let Some(max) = max_id {
                (false, Some(max))
            } else if let Some(since) = since_id {
                (true, Some(since))
            } else {
                (true, None)
            };
            let query = EntryQuery {
                id_user,
                scope: ListType::IncludingHidden,
                scope_id: 0,
                state: EntryState::ALL,
                search: None,
                // Fever's cursors are exclusive; the builder's bounds are inclusive.
                id_min: bound
                    .filter(|_| ascending)
                    .map(|b| EntryId::from_micros(b + 1)),
                id_max: bound
                    .filter(|_| !ascending)
                    .map(|b| EntryId::from_micros(b - 1)),
                limit: PAGE,
                sort: crate::domain::sort::Sort::Id,
                order: if ascending {
                    crate::domain::sort::Order::Asc
                } else {
                    crate::domain::sort::Order::Desc
                },
                ..Default::default()
            };
            let mut rows = state
                .db
                .with(move |mut conn| async move { list(&mut conn, &query).await })
                .await?;
            if let Some(feeds) = &feed_filter {
                rows.retain(|e| feeds.contains(&e.id_feed));
            }
            rows
        }
    };
    let rows: Vec<_> = rows.into_iter().take(PAGE).collect();

    let total = {
        state
            .db
            .with(move |mut conn| async move { entry_dao::count_states(&mut conn, id_user).await })
            .await?
            .total
    };

    let items: Vec<Value> = rows
        .iter()
        .map(|e| {
            json!({
                "id": e.id.micros().to_string(),
                "feed_id": e.id_feed,
                "title": escape_to_unicode_alternative(&e.title, false),
                "author": escape_to_unicode_alternative(&e.authors.join("; "), false),
                "html": e.content,
                "url": decode_url(&e.link),
                "is_saved": i64::from(e.is_favorite),
                "is_read": i64::from(e.is_read),
                "created_on_time": e.date,
            })
        })
        .collect();
    Ok((total, json!(items)))
}

async fn category_feeds(
    state: &AppState,
    id_user: i64,
    raw: &str,
) -> crate::error::Result<Vec<i64>> {
    let ids: Vec<i64> = raw.split(',').filter_map(|p| p.parse().ok()).collect();
    let feeds = state
        .db
        .with(move |mut conn| async move { feed_dao::list(&mut conn, id_user).await })
        .await?;
    Ok(feeds
        .into_iter()
        .filter(|f| f.priority.get() > crate::domain::priority::Priority::HIDDEN.get())
        .filter(|f| f.category.is_some_and(|c| ids.contains(&c)))
        .map(|f| f.id)
        .collect())
}

/// The comma-separated id strings, over the main stream only, as upstream computes them.
async fn id_list(state: &AppState, id_user: i64, entry_state: EntryState) -> String {
    let query = EntryQuery {
        id_user,
        scope: ListType::MainStream,
        scope_id: 0,
        state: entry_state,
        search: None,
        id_min: None,
        id_max: None,
        limit: usize::MAX,
        sort: crate::domain::sort::Sort::Id,
        order: crate::domain::sort::Order::Asc,
        ..Default::default()
    };
    state
        .db
        .with(move |mut conn| async move { list(&mut conn, &query).await })
        .await
        .map(|rows| {
            rows.iter()
                .map(|e| e.id.micros().to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// The marker mutations. Returns the response key to append, if the combination is one Fever knows.
async fn apply_mark(
    state: &AppState,
    id_user: i64,
    mark: &str,
    as_: &str,
    param: &impl Fn(&str) -> Option<String>,
) -> Option<&'static str> {
    let mark = mark.to_ascii_lowercase();
    let digits = |name: &str| -> Option<i64> {
        param(name)
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
            .and_then(|v| v.parse().ok())
    };
    let id = digits("id");
    let with_ids: Vec<i64> = param("with_ids")
        .map(|raw| {
            raw.split(',')
                .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
                .filter_map(|p| p.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let before_secs = param("before")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    // `before=0` means "up to now", and now must keep its microsecond fraction: truncating to the
    // second would exclude every article ingested later in the very second the marker arrives.
    let before = if before_secs == 0 {
        crate::domain::ids::now_micros()
    } else {
        before_secs * 1_000_000
    };
    let now = crate::domain::ids::now_unix();

    let ids: Vec<EntryId> = match id {
        Some(id) => vec![EntryId::from_micros(id)],
        None => with_ids.into_iter().map(EntryId::from_micros).collect(),
    };

    let work = match (mark.as_str(), as_) {
        ("item", "read") => Work::Entries(ids, true),
        ("item", "unread") => Work::Entries(ids, false),
        ("item", "saved") => Work::Star(ids, true),
        ("item", "unsaved") => Work::Star(ids, false),
        ("feed", "read") => Work::Feed(id.unwrap_or(0), before),
        ("group", "read") => Work::Group(id.unwrap_or(0), before),
        _ => return None,
    };
    let response_key = match as_ {
        "read" | "unread" => "unread_item_ids",
        "saved" | "unsaved" => "saved_item_ids",
        _ => return None,
    };

    let result: crate::error::Result<()> = state
        .db
        .with_writer(move |mut conn| async move {
            match work {
                Work::Entries(ids, read) if !ids.is_empty() => {
                    let feeds = entry_dao::feed_ids_for(&mut conn, id_user, &ids).await?;
                    entry_dao::mark_read(&mut conn, id_user, &ids, read).await?;
                    tree::refresh_counters(&mut conn, &feeds).await?;
                }
                Work::Star(ids, starred) if !ids.is_empty() => {
                    entry_dao::mark_favorite(&mut conn, id_user, &ids, starred).await?;
                }
                Work::Feed(feed_id, bound) if feed_id > 0 => {
                    entry_dao::mark_read_feed(&mut conn, feed_id, EntryId::from_micros(bound), now)
                        .await?;
                    tree::refresh_counters(&mut conn, &[feed_id]).await?;
                }
                Work::Group(0, bound) => {
                    entry_dao::mark_read_all(&mut conn, id_user, EntryId::from_micros(bound), now)
                        .await?;
                    let feeds: Vec<i64> = feed_dao::list(&mut conn, id_user)
                        .await?
                        .into_iter()
                        .map(|f| f.id)
                        .collect();
                    tree::refresh_counters(&mut conn, &feeds).await?;
                }
                Work::Group(group_id, bound) => {
                    entry_dao::mark_read_category(
                        &mut conn,
                        id_user,
                        group_id,
                        EntryId::from_micros(bound),
                        now,
                    )
                    .await?;
                    let feeds: Vec<i64> = feed_dao::list(&mut conn, id_user)
                        .await?
                        .into_iter()
                        .filter(|f| f.category == Some(group_id))
                        .map(|f| f.id)
                        .collect();
                    tree::refresh_counters(&mut conn, &feeds).await?;
                }
                _ => {}
            }
            Ok(())
        })
        .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, "fever marker failed");
    }
    Some(response_key)
}

enum Work {
    Entries(Vec<EntryId>, bool),
    Star(Vec<EntryId>, bool),
    Feed(i64, i64),
    Group(i64, i64),
}
