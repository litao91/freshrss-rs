//! The write half of the Google Reader API: `edit-tag`, `mark-all-as-read`, `rename-tag`,
//! `disable-tag` and the subscription trio.
//!
//! Every mutation refreshes the cached per-feed counters in the same transaction where the PHP
//! decremented them inline; here that is [`crate::service::tree::refresh_counters`], which
//! recomputes rather than adjusts and so cannot drift the way an increment can.

use axum::http::StatusCode;
use axum::response::Response;

use crate::api::greader::streams::{resolve_stream, Stream};
use crate::api::greader::{plain, Ctx};
use crate::db::dao::entry_query::{list, EntryQuery};
use crate::db::dao::{
    category as cat_dao, entry as entry_dao, feed as feed_dao, tag as tag_dao, user as user_dao,
};
use crate::domain::ids::{now_unix, EntryId};
use crate::domain::state::EntryState;
use crate::service::refresh::Settings;
use crate::service::subscription::{self, NewSubscription};
use crate::service::tree;

const READ: &str = "user/-/state/com.google/read";
const STARRED: &str = "user/-/state/com.google/starred";

/// `POST /reader/api/0/edit-tag` — read/unread, star/unstar, label/unlabel, in one call.
pub async fn edit_tag(ctx: Ctx<'_>) -> Response {
    if !ctx.check_token() {
        return crate::api::greader::unauthorized();
    }
    let ids: Vec<EntryId> = ctx
        .repeated("i")
        .iter()
        .map(|raw| EntryId::from_micros(crate::api::greader::dto::parse_item_id(raw)))
        .filter(|id| id.micros() != 0)
        .collect();
    if ids.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    let adds = ctx.repeated("a");
    let removes = ctx.repeated("r");
    let id_user = ctx.user.id;

    let result = ctx
        .state
        .db
        .with_writer(move |mut conn| async move {
            let mut touched: Vec<i64> = Vec::new();
            for state in adds.iter().chain(removes.iter()) {
                let adding = adds.iter().any(|a| a == state);
                match state.as_str() {
                    READ => {
                        let feeds = entry_dao::feed_ids_for(&mut conn, id_user, &ids).await?;
                        entry_dao::mark_read(&mut conn, id_user, &ids, adding).await?;
                        touched.extend(feeds);
                    }
                    STARRED => {
                        entry_dao::mark_favorite(&mut conn, id_user, &ids, adding).await?;
                    }
                    // broadcast/like/tracking-kept-unread are accepted and ignored, as upstream.
                    "user/-/state/com.google/broadcast"
                    | "user/-/state/com.google/like"
                    | "user/-/state/com.google/tracking-kept-unread" => {}
                    other => {
                        let Some(name) = label_name(other, adding) else {
                            continue;
                        };
                        if adding {
                            let tag = tag_dao::find_or_create(&mut conn, id_user, &name).await?;
                            tag_dao::tag_entries(
                                &mut conn,
                                tag,
                                &ids.iter().map(|id| id.micros()).collect::<Vec<_>>(),
                            )
                            .await?;
                        } else {
                            if let Some(tag) =
                                tag_dao::find_by_name(&mut conn, id_user, &name).await?
                            {
                                for id in &ids {
                                    tag_dao::untag_entry(&mut conn, tag.id, id.micros()).await?;
                                }
                            }
                        }
                    }
                }
            }
            touched.sort_unstable();
            touched.dedup();
            tree::refresh_counters(&mut conn, &touched).await?;
            Ok(())
        })
        .await;
    match result {
        Ok(()) => plain(StatusCode::OK, "OK"),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    }
}

/// `user/-/label/<name>` (and the `user/<me>/label/<name>` spelling adds accept) to a bare name.
fn label_name(stream_id: &str, adding: bool) -> Option<String> {
    let marker = "/label/";
    let pos = stream_id.find(marker)?;
    if adding || stream_id.starts_with("user/-/label/") {
        Some(stream_id[pos + marker.len()..].to_owned())
    } else {
        None
    }
}

/// `POST /reader/api/0/mark-all-as-read` — `ts` is the microsecond bound, `0` meaning now.
pub async fn mark_all_as_read(ctx: Ctx<'_>) -> Response {
    if !ctx.check_token() {
        return crate::api::greader::unauthorized();
    }
    let Some(stream_id) = ctx.param("s") else {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    };
    let ts_raw = ctx.param("ts").unwrap_or_else(|| "0".to_owned());
    if !ts_raw.chars().all(|c| c.is_ascii_digit()) {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    let ts: i64 = ts_raw.parse().unwrap_or(0);
    let id_max = EntryId::from_micros(if ts == 0 {
        crate::domain::ids::now_micros()
    } else {
        ts
    });
    let now = now_unix();

    let stream = match resolve_stream(&ctx, &stream_id).await {
        Some(stream) => stream,
        None => return plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    };
    let id_user = ctx.user.id;

    let result = ctx
        .state
        .db
        .with_writer(move |mut conn| async move {
            match stream {
                Stream::Feed(id) if id > 0 => {
                    entry_dao::mark_read_feed(&mut conn, id, id_max, now).await?;
                    tree::refresh_counters(&mut conn, &[id]).await?;
                    return Ok(());
                }
                Stream::Category(id) if id > 0 => {
                    entry_dao::mark_read_category(&mut conn, id_user, id, id_max, now).await?;
                }
                Stream::Label(id) if id > 0 => {
                    let ids = labelled_ids(&mut conn, id_user, id, id_max).await?;
                    if !ids.is_empty() {
                        let feeds = entry_dao::feed_ids_for(&mut conn, id_user, &ids).await?;
                        entry_dao::mark_read(&mut conn, id_user, &ids, true).await?;
                        tree::refresh_counters(&mut conn, &feeds).await?;
                    }
                    return Ok(());
                }
                other => {
                    let (scope, state) = match other {
                        Stream::Starred => {
                            (crate::domain::list_type::ListType::Starred, EntryState::ALL)
                        }
                        Stream::Main => (
                            crate::domain::list_type::ListType::MainStream,
                            EntryState::ALL,
                        ),
                        Stream::Important => (
                            crate::domain::list_type::ListType::Important,
                            EntryState::ALL,
                        ),
                        Stream::ReadState { unread: false } => {
                            // Marking the already-read read is the no-op upstream produces.
                            return Ok(());
                        }
                        _ => (
                            crate::domain::list_type::ListType::AllButHidden,
                            EntryState::ALL,
                        ),
                    };
                    let query = EntryQuery {
                        id_user,
                        scope,
                        scope_id: 0,
                        state,
                        search: None,
                        id_min: None,
                        id_max: Some(id_max),
                        limit: usize::MAX,
                        ..Default::default()
                    };
                    let rows = list(&mut conn, &query).await?;
                    let ids: Vec<EntryId> = rows.iter().map(|e| e.id).collect();
                    if !ids.is_empty() {
                        let feeds = entry_dao::feed_ids_for(&mut conn, id_user, &ids).await?;
                        entry_dao::mark_read(&mut conn, id_user, &ids, true).await?;
                        tree::refresh_counters(&mut conn, &feeds).await?;
                    }
                }
            }
            // Category and whole-tree paths land here: recompute everything the user can see.
            let all = feed_dao::list(&mut conn, id_user).await?;
            let feeds: Vec<i64> = all.into_iter().map(|f| f.id).collect();
            tree::refresh_counters(&mut conn, &feeds).await?;
            Ok(())
        })
        .await;
    match result {
        Ok(()) => plain(StatusCode::OK, "OK"),
        Err(_) => plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    }
}

async fn labelled_ids(
    conn: &mut sqlx::SqliteConnection,
    id_user: i64,
    id_tag: i64,
    id_max: EntryId,
) -> crate::error::Result<Vec<EntryId>> {
    let query = EntryQuery {
        id_user,
        scope: crate::domain::list_type::ListType::Tag,
        scope_id: id_tag,
        state: EntryState::ALL,
        search: None,
        id_min: None,
        id_max: Some(id_max),
        limit: usize::MAX,
        ..Default::default()
    };
    Ok(list(conn, &query)
        .await?
        .into_iter()
        .map(|e| e.id)
        .collect())
}

/// `POST /reader/api/0/rename-tag` — categories first, then labels, as upstream tries them.
pub async fn rename_tag(ctx: Ctx<'_>) -> Response {
    if !ctx.check_token() {
        return crate::api::greader::unauthorized();
    }
    let (Some(source), Some(dest)) = (ctx.param("s"), ctx.param("dest")) else {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    };
    let (Some(from), Some(to)) = (label_name(&source, true), label_name(&dest, true)) else {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    };
    let id_user = ctx.user.id;
    let done = ctx
        .state
        .db
        .with(move |mut conn| async move {
            if let Some(cat) = cat_dao::find_by_name(&mut conn, id_user, &from).await? {
                cat_dao::update_name(&mut conn, cat.id, id_user, &to).await?;
                return Ok(true);
            }
            if let Some(tag) = tag_dao::find_by_name(&mut conn, id_user, &from).await? {
                tag_dao::rename(&mut conn, tag.id, id_user, &to).await?;
                return Ok(true);
            }
            Ok(false)
        })
        .await;
    match done {
        Ok(true) => plain(StatusCode::OK, "OK"),
        Ok(false) | Err(_) => plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    }
}

/// `POST /reader/api/0/disable-tag` — delete the label, or empty and delete the category.
pub async fn disable_tag(ctx: Ctx<'_>) -> Response {
    if !ctx.check_token() {
        return crate::api::greader::unauthorized();
    }
    let names: Vec<String> = ctx
        .repeated("s")
        .iter()
        .filter_map(|s| label_name(s, true))
        .collect();
    if names.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    let id_user = ctx.user.id;
    let done = ctx
        .state
        .db
        .with_writer(move |mut conn| async move {
            for name in &names {
                if let Some(cat) = cat_dao::find_by_name(&mut conn, id_user, name).await? {
                    let default = user_dao::default_category_id(&mut conn, id_user).await?;
                    for feed in feed_dao::list(&mut conn, id_user).await? {
                        if feed.category == Some(cat.id) {
                            feed_dao::update_category(&mut conn, feed.id, id_user, default).await?;
                        }
                    }
                    if cat.id > 1 {
                        cat_dao::delete(&mut conn, cat.id, id_user).await?;
                    }
                    continue;
                }
                if let Some(tag) = tag_dao::find_by_name(&mut conn, id_user, name).await? {
                    tag_dao::delete(&mut conn, tag.id, id_user).await?;
                    continue;
                }
                return Ok(false);
            }
            Ok(true)
        })
        .await;
    match done {
        Ok(true) => plain(StatusCode::OK, "OK"),
        Ok(false) | Err(_) => plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    }
}

/// `subscription/edit` — subscribe, unsubscribe, move between folders, rename.
pub async fn subscription_edit(ctx: Ctx<'_>) -> Response {
    let action = ctx.param("ac").unwrap_or_default();
    let streams = ctx.repeated("s");
    let titles = ctx.repeated("t");
    if streams.is_empty() || !matches!(action.as_str(), "subscribe" | "unsubscribe" | "edit") {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    let add_to = ctx.param("a").and_then(|s| label_name(&s, true));
    let remove_from = ctx.param("r").and_then(|s| label_name(&s, true));

    for (index, stream_id) in streams.iter().enumerate() {
        let title = titles.get(index).cloned();
        let bare = stream_id.strip_prefix("feed/").unwrap_or(stream_id);
        match action.as_str() {
            "subscribe" => {
                // Numeric ids are never a subscribe target: upstream skips them.
                if bare.parse::<i64>().is_ok() {
                    continue;
                }
                let category = match category_for(&ctx, add_to.as_deref()).await {
                    Some(category) => category,
                    None => return plain(StatusCode::BAD_REQUEST, "Bad Request!"),
                };
                let spec = NewSubscription {
                    id_user: ctx.user.id,
                    url: bare.to_owned(),
                    name: title,
                    category,
                    kind: detect_kind(bare),
                    priority: crate::domain::priority::Priority::MAIN_STREAM,
                    ttl: crate::domain::ttl::Ttl::default(),
                };
                let settings = Settings::from_config(ctx.config());
                let now = now_unix();
                if subscription::add(&ctx.state.db, &*ctx.state.fetcher, &settings, &spec, now)
                    .await
                    .is_err()
                {
                    return plain(StatusCode::BAD_REQUEST, "Bad Request!");
                }
            }
            "unsubscribe" => {
                let Some(feed) = feed_ref(&ctx, bare).await else {
                    return plain(StatusCode::BAD_REQUEST, "Bad Request!");
                };
                let id_user = ctx.user.id;
                let _ = ctx
                    .state
                    .db
                    .with_writer(move |mut conn| async move {
                        feed_dao::delete(&mut conn, feed, id_user).await
                    })
                    .await;
            }
            _ => {
                let Some(feed) = feed_ref(&ctx, bare).await else {
                    return plain(StatusCode::BAD_REQUEST, "Bad Request!");
                };
                let id_user = ctx.user.id;
                let category = match (add_to.as_deref(), remove_from.as_deref()) {
                    (Some(name), _) => category_for(&ctx, Some(name)).await.flatten(),
                    (None, Some(_)) => Some(default_category(&ctx).await.unwrap_or(1)),
                    _ => None,
                };
                let title = title.clone();
                let _ = ctx
                    .state
                    .db
                    .with_writer(move |mut conn| async move {
                        if let Some(category) = category {
                            feed_dao::update_category(&mut conn, feed, id_user, Some(category))
                                .await?;
                        }
                        if let Some(title) = title.filter(|t| !t.is_empty()) {
                            feed_dao::update_name(&mut conn, feed, id_user, &title).await?;
                        }
                        Ok(())
                    })
                    .await;
            }
        }
    }
    plain(StatusCode::OK, "OK")
}

/// `subscription/quickadd` — TheOldReader's extension: always 200, `numResults` says whether it worked.
pub async fn subscription_quickadd(ctx: Ctx<'_>) -> Response {
    let Some(url) = ctx.param("quickadd").or_else(|| ctx.param("s")) else {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    };
    let url = url.strip_prefix("feed/").unwrap_or(&url).to_owned();
    let spec = NewSubscription {
        id_user: ctx.user.id,
        url: url.clone(),
        name: None,
        category: None,
        kind: detect_kind(&url),
        priority: crate::domain::priority::Priority::MAIN_STREAM,
        ttl: crate::domain::ttl::Ttl::default(),
    };
    let settings = Settings::from_config(ctx.config());
    let now = now_unix();
    match subscription::add(&ctx.state.db, &*ctx.state.fetcher, &settings, &spec, now).await {
        Ok(added) => crate::api::greader::json_body(serde_json::json!({
            "numResults": 1,
            "query": url,
            "streamId": format!("feed/{}", added.feed.id),
            "streamName": added.feed.name,
        })),
        Err(err) => crate::api::greader::json_body(serde_json::json!({
            "numResults": 0,
            "error": err.to_string(),
        })),
    }
}

/// `POST /reader/api/0/subscription/import` — the raw body is OPML.
pub async fn subscription_import(ctx: Ctx<'_>) -> Response {
    let body = String::from_utf8_lossy(ctx.body).to_string();
    if body.len() > 1024 * 1024 {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    match crate::service::import::opml(&ctx.state.db, ctx.user.id, body.as_bytes(), false, None)
        .await
    {
        Ok(_) => plain(StatusCode::OK, "OK"),
        Err(_) => plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    }
}

/// The folder a subscribe or move should file the feed under; `None` means "leave where it is",
/// `Some(None)` means the user's default folder.
async fn category_for(ctx: &Ctx<'_>, name: Option<&str>) -> Option<Option<i64>> {
    let name = name?;
    if name.is_empty() || name.eq_ignore_ascii_case("Default category") {
        return Some(Some(default_category(ctx).await?));
    }
    let name = name.to_owned();
    let id_user = ctx.user.id;
    let lookup = name.clone();
    let found = ctx
        .state
        .db
        .with(
            move |mut conn| async move { cat_dao::find_by_name(&mut conn, id_user, &lookup).await },
        )
        .await
        .ok()?;
    match found {
        Some(cat) => Some(Some(cat.id)),
        None => {
            let id_user = ctx.user.id;
            let name = name.clone();
            let created = ctx
                .state
                .db
                .with(move |mut conn| async move {
                    let position = cat_dao::next_position(&mut conn, id_user).await?;
                    cat_dao::insert(
                        &mut conn,
                        &cat_dao::NewCategory {
                            id_user,
                            name: &name,
                            kind: crate::domain::category::CategoryKind::Normal,
                            position,
                            attributes: crate::domain::attributes::Attributes::new(),
                        },
                    )
                    .await
                })
                .await
                .ok()?;
            Some(Some(created))
        }
    }
}

async fn default_category(ctx: &Ctx<'_>) -> Option<i64> {
    let id_user = ctx.user.id;
    ctx.state
        .db
        .with(
            move |mut conn| async move { user_dao::default_category_id(&mut conn, id_user).await },
        )
        .await
        .ok()
        .flatten()
}

async fn feed_ref(ctx: &Ctx<'_>, bare: &str) -> Option<i64> {
    if let Ok(id) = bare.parse::<i64>() {
        return Some(id);
    }
    let url = bare.to_owned();
    let id_user = ctx.user.id;
    ctx.state
        .db
        .with(move |mut conn| async move { feed_dao::find_by_url(&mut conn, id_user, &url).await })
        .await
        .ok()
        .flatten()
        .map(|f| f.id)
}

/// The PHP's `detectFeedKind`: a URL that mentions json is a JSON Feed, everything else RSS.
fn detect_kind(url: &str) -> crate::domain::kind::FeedKind {
    if url.contains("json") {
        crate::domain::kind::FeedKind::JsonFeed
    } else {
        crate::domain::kind::FeedKind::Rss
    }
}
