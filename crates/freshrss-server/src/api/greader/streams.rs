//! The read half of the Google Reader API: stream contents, item ids, item contents by id, and the
//! three aggregate listings (`tag/list`, `subscription/list`, `unread-count`).
//!
//! The paging contract is the part clients break on, so it is reproduced exactly: `c` is the
//! decimal microsecond id of the last item of the previous page, the bound is inclusive, one extra
//! row is fetched and the first is dropped, and `continuation` is emitted only when a full page
//! came back. Getting any of that subtly wrong shows up as skipped or duplicated articles in every
//! client that pages.

use std::collections::HashMap;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use freshrss_search::ast::{DateField, DateRange, Expr, Op, Pred};

use crate::api::greader::dto;
use crate::api::greader::escape::{decode_url, escape_to_unicode_alternative};
use crate::api::greader::{json_body, plain, Ctx};
use crate::db::dao::{category as cat_dao, entry as entry_dao, feed as feed_dao, tag as tag_dao};
use crate::db::dao::entry_query::{list, EntryQuery};
use crate::domain::ids::EntryId;
use crate::domain::list_type::ListType;
use crate::domain::state::EntryState;

/// What a stream id named, once resolved against this user's subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    All,
    Main,
    Important,
    Starred,
    Hidden,
    Feed(i64),
    Category(i64),
    Label(i64),
    /// `com.google/read` and `…/unread` as a *scope*: the reading list restricted by state.
    ReadState { unread: bool },
}

/// Resolves a stream id. `None` means the grammar did not match at all; a stream whose subject does
/// not exist resolves to an id of `-1`, which lists nothing, exactly as the PHP's "search returned
/// nothing" path does.
pub async fn resolve_stream(ctx: &Ctx<'_>, raw: &str) -> Option<Stream> {
    let id_user = ctx.user.id;
    if let Some(rest) = raw.strip_prefix("feed/") {
        let id = match rest.parse::<i64>() {
            Ok(id) => id,
            Err(_) => {
                let url = rest.to_owned();
                ctx.state
                    .db
                    .with(move |mut conn| async move {
                        feed_dao::find_by_url(&mut conn, id_user, &url).await
                    })
                    .await
                    .ok()
                    .flatten()
                    .map(|f| f.id)
                    .unwrap_or(-1)
            }
        };
        return Some(Stream::Feed(id));
    }
    if let Some(pos) = raw.find("/label/") {
        let name = &raw[pos + "/label/".len()..];
        let name = name.to_owned();
        return ctx
            .state
            .db
            .with(move |mut conn| async move {
                if let Some(cat) = cat_dao::find_by_name(&mut conn, id_user, &name).await? {
                    return Ok(Some(Stream::Category(cat.id)));
                }
                Ok(tag_dao::find_by_name(&mut conn, id_user, &name)
                    .await?
                    .map(|tag| Stream::Label(tag.id)))
            })
            .await
            .ok()
            .flatten()
            .or(Some(Stream::Category(-1)));
    }
    if raw.contains("/state/com.google/reading-list") {
        return Some(Stream::All);
    }
    if raw.contains("/state/com.google/starred") {
        return Some(Stream::Starred);
    }
    if raw.contains("/state/com.google/read") {
        return Some(Stream::ReadState { unread: false });
    }
    if raw.contains("/state/com.google/unread") {
        return Some(Stream::ReadState { unread: true });
    }
    if raw.contains("/state/org.freshrss/main") {
        return Some(Stream::Main);
    }
    if raw.contains("/state/org.freshrss/important") {
        return Some(Stream::Important);
    }
    if raw.contains("/state/org.freshrss/hidden") {
        return Some(Stream::Hidden);
    }
    None
}

/// The stream id from the path suffix, or from `?s=` when the path carries none (BazQux clients).
pub fn stream_id_from(ctx: &Ctx<'_>, rest: &[&str]) -> String {
    if rest.is_empty() {
        if let Some(s) = ctx.query.get("s") {
            return s.clone();
        }
        return "user/-/state/com.google/reading-list".to_owned();
    }
    rest.join("/")
}

/// `it` and `xt` become one state mask, over a base that constrains nothing.
///
/// The base is ALL | FAVORITE | NOT_FAVORITE rather than ALL alone because the masks combine by
/// intersection: starting from 3, "exclude starred" would intersect to 0 and lose the exclusion.
fn state_mask(ctx: &Ctx<'_>) -> i32 {
    use crate::domain::state::bits;
    let mut mask: i32 = bits::ALL | bits::FAVORITE | bits::NOT_FAVORITE;
    let it = ctx.param("it").unwrap_or_default();
    let xt = ctx.param("xt").unwrap_or_default();
    let read_state = "user/-/state/com.google/read";
    let unread_state = "user/-/state/com.google/unread";
    let starred_state = "user/-/state/com.google/starred";
    match it.as_str() {
        s if s == read_state => mask &= bits::READ,
        s if s == unread_state => mask &= bits::NOT_READ,
        s if s == starred_state => mask &= bits::FAVORITE,
        _ => {}
    }
    match xt.as_str() {
        s if s == read_state => mask &= bits::NOT_READ,
        s if s == unread_state => mask &= bits::READ,
        s if s == starred_state => mask &= bits::NOT_FAVORITE,
        _ => {}
    }
    mask
}

/// `ot` and `nt` as a search tree: ingestion time (the id) OR modification time for the lower
/// bound, both AND-ed for the upper, which is how the PHP's two `Search` objects combine.
fn time_window(ctx: &Ctx<'_>) -> Option<Expr> {
    let ot: i64 = ctx.param("ot").and_then(|v| v.parse().ok()).unwrap_or(0);
    let nt: i64 = ctx.param("nt").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut parts = Vec::new();
    if ot != 0 {
        parts.push(Expr::Node {
            op: Op::Or,
            children: vec![
                Expr::Leaf(vec![Pred::Date {
                    field: DateField::Ingestion,
                    range: DateRange { min: Some(ot), max: None },
                    negated: false,
                }]),
                Expr::Leaf(vec![Pred::Date {
                    field: DateField::Modified,
                    range: DateRange { min: Some(ot), max: None },
                    negated: false,
                }]),
            ],
        });
    }
    if nt != 0 {
        parts.push(Expr::Leaf(vec![
            Pred::Date {
                field: DateField::Ingestion,
                range: DateRange { min: None, max: Some(nt) },
                negated: false,
            },
            Pred::Date {
                field: DateField::Modified,
                range: DateRange { min: None, max: Some(nt) },
                negated: false,
            },
        ]));
    }
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Expr::Node { op: Op::And, children: parts }),
    }
}

/// The shared query for one stream, built but not run, so `contents` and `ids` share it.
pub async fn build_query(ctx: &Ctx<'_>, stream: Stream) -> EntryQuery {
    use crate::domain::state::bits;
    let (scope, scope_id) = match stream {
        Stream::All | Stream::ReadState { .. } => (ListType::AllButHidden, 0),
        Stream::Main => (ListType::MainStream, 0),
        Stream::Important => (ListType::Important, 0),
        Stream::Starred => (ListType::Starred, 0),
        Stream::Hidden => (ListType::IncludingHidden, 0),
        Stream::Feed(id) => (ListType::Feed, id),
        Stream::Category(id) => (ListType::Category, id),
        Stream::Label(id) => (ListType::Tag, id),
    };
    let mut mask = state_mask(ctx);
    if let Stream::ReadState { unread } = stream {
        mask &= if unread { bits::NOT_READ } else { bits::READ };
    }
    let state = EntryState::from_bits(mask);

    let n: usize = ctx.param("n").and_then(|v| v.parse().ok()).unwrap_or(20);
    let continuation: i64 = ctx
        .param("c")
        .map(|v| v.trim().to_owned())
        .filter(|v| v.chars().all(|c| c.is_ascii_digit()))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let ascending = ctx.param("r").as_deref() == Some("o");

    let mut query = EntryQuery {
        id_user: ctx.user.id,
        scope,
        scope_id,
        state,
        search: time_window(ctx),
        id_min: if ascending && continuation != 0 {
            Some(EntryId::from_micros(continuation))
        } else {
            None
        },
        id_max: if !ascending && continuation != 0 {
            Some(EntryId::from_micros(continuation))
        } else {
            None
        },
        limit: if continuation != 0 { n + 1 } else { n },
        ..Default::default()
    };
    query.sort = crate::domain::sort::Sort::Id;
    query.order = if ascending {
        crate::domain::sort::Order::Asc
    } else {
        crate::domain::sort::Order::Desc
    };
    query
}

/// Runs the query and applies the inclusive-continuation compensation.
async fn page(ctx: &Ctx<'_>, query: EntryQuery) -> crate::error::Result<(Vec<crate::domain::entry::Entry>, Option<String>)> {
    let n = query.limit.saturating_sub(1).max(1);
    let compensated = ctx.param("c").is_some_and(|v| {
        let t = v.trim();
        !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) && t != "0"
    });
    let wanted = if compensated { n } else { query.limit };
    let id_user = ctx.user.id;
    let mut rows = ctx
        .state
        .db
        .with(move |mut conn| async move { list(&mut conn, &query).await })
        .await?;
    if compensated && !rows.is_empty() {
        // The bound is inclusive, so the first row is the last row of the previous page.
        rows.remove(0);
    }
    let continuation = if rows.len() >= wanted {
        rows.last().map(|e| e.id.micros().to_string())
    } else {
        None
    };
    let _ = id_user;
    Ok((rows, continuation))
}

/// The per-item joins: feed, category name, label names. Feed keywords travel on the entry itself.
async fn enrich(
    ctx: &Ctx<'_>,
    rows: &[crate::domain::entry::Entry],
) -> crate::error::Result<Vec<Value>> {
    let id_user = ctx.user.id;
    let ids: Vec<i64> = rows.iter().map(|e| e.id.micros()).collect();
    let (feeds, categories, labels) = ctx
        .state
        .db
        .with(move |mut conn| async move {
            let feeds = feed_dao::list(&mut conn, id_user).await?;
            let categories = cat_dao::list(&mut conn, id_user).await?;
            let labels = tag_dao::labels_for_entries(&mut conn, &ids).await?;
            Ok((feeds, categories, labels))
        })
        .await?;
    let feeds: HashMap<i64, _> = feeds.into_iter().map(|f| (f.id, f)).collect();
    let categories: HashMap<i64, String> = categories.into_iter().map(|c| (c.id, c.name)).collect();

    let mut items = Vec::with_capacity(rows.len());
    for entry in rows {
        // An entry whose feed vanished between the listing and this join is skipped, as the PHP
        // skips it: half an item with no origin is worse than no item.
        let Some(feed) = feeds.get(&entry.id_feed) else {
            continue;
        };
        let category_name = feed.category.and_then(|id| categories.get(&id));
        let label_names: Vec<String> = labels
            .get(&entry.id.micros())
            .map(|tags| tags.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();
        items.push(dto::item_json(
            entry,
            feed,
            category_name.map(String::as_str),
            &label_names,
            &entry.tags,
        ));
    }
    Ok(items)
}

/// `GET /reader/api/0/stream/contents[…]`
pub async fn contents(ctx: Ctx<'_>, rest: &[&str]) -> Response {
    let raw = stream_id_from(&ctx, rest);
    let stream = match resolve_stream(&ctx, &raw).await {
        Some(stream) => stream,
        None => return plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    };
    let query = build_query(&ctx, stream).await;
    let (rows, continuation) = match page(&ctx, query).await {
        Ok(paged) => paged,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let items = match enrich(&ctx, &rows).await {
        Ok(items) => items,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let mut envelope = json!({
        // Hardcoded to the reading list upstream, whatever was asked for; clients read `items`.
        "id": "user/-/state/com.google/reading-list",
        "updated": now_secs(),
        "items": items,
    });
    if let Some(continuation) = continuation {
        envelope["continuation"] = Value::String(continuation);
    }
    json_body(envelope)
}

/// `GET /reader/api/0/stream/items/ids` — decimal ids, not the hex tag URIs of `contents`.
pub async fn item_ids(ctx: Ctx<'_>) -> Response {
    let Some(raw) = ctx.query.get("s").cloned() else {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    };
    let stream = match resolve_stream(&ctx, &raw).await {
        Some(stream) => stream,
        None => return plain(StatusCode::BAD_REQUEST, "Bad Request!"),
    };
    let query = build_query(&ctx, stream).await;
    let (rows, continuation) = match page(&ctx, query).await {
        Ok(paged) => paged,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let refs: Vec<Value> = rows
        .iter()
        .map(|e| json!({ "id": e.id.micros().to_string() }))
        .collect();
    let mut envelope = json!({ "itemRefs": refs });
    if let Some(continuation) = continuation {
        envelope["continuation"] = Value::String(continuation);
    }
    if refs.is_empty() && ctx.param("client").as_deref() == Some("newsplus") {
        // News+ treats an empty array as a protocol error; a single zero id reads as "nothing".
        envelope["itemRefs"] = json!([{ "id": "0" }]);
    }
    json_body(envelope)
}

/// `POST /reader/api/0/stream/items/contents` — full items for an explicit id list.
pub async fn items_contents(ctx: Ctx<'_>) -> Response {
    let ids: Vec<EntryId> = ctx
        .repeated("i")
        .iter()
        .map(|raw| EntryId::from_micros(dto::parse_item_id(raw)))
        .filter(|id| id.micros() != 0)
        .collect();
    if ids.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "Bad Request!");
    }
    let ascending = ctx.param("r").as_deref() == Some("o");
    let id_user = ctx.user.id;
    let rows = match ctx
        .state
        .db
        .with(move |mut conn| async move { entry_dao::list_by_ids(&mut conn, id_user, &ids).await })
        .await
    {
        Ok(mut rows) => {
            rows.sort_by_key(|e| e.id.micros());
            if !ascending {
                rows.reverse();
            }
            rows
        }
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let items = match enrich(&ctx, &rows).await {
        Ok(items) => items,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    json_body(json!({
        "id": "user/-/state/com.google/reading-list",
        "updated": now_secs(),
        "items": items,
    }))
}

/// `GET /reader/api/0/tag/list?output=json`
pub async fn tag_list(ctx: Ctx<'_>) -> Response {
    if ctx.param("output").as_deref() != Some("json") {
        return plain(StatusCode::NOT_IMPLEMENTED, "Not Implemented!");
    }
    let id_user = ctx.user.id;
    let (categories, tags) = match ctx
        .state
        .db
        .with(move |mut conn| async move {
            let categories = cat_dao::list(&mut conn, id_user).await?;
            let tags = tag_dao::list_with_counts(&mut conn, id_user).await?;
            Ok((categories, tags))
        })
        .await
    {
        Ok(both) => both,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let mut list = vec![
        json!({ "id": "user/-/state/com.google/starred" }),
        json!({ "id": "user/-/state/com.google/reading-list" }),
        json!({ "id": "user/-/state/org.freshrss/main" }),
        json!({ "id": "user/-/state/org.freshrss/important" }),
    ];
    for category in categories {
        list.push(json!({
            "id": format!("user/-/label/{}", escape_to_unicode_alternative(&category.name, false)),
            "type": "folder",
        }));
    }
    for (tag, _total, unread) in tags {
        list.push(json!({
            "id": format!("user/-/label/{}", escape_to_unicode_alternative(&tag.name, false)),
            "type": "tag",
            "unread_count": unread,
        }));
    }
    json_body(json!({ "tags": list }))
}

/// `GET /reader/api/0/subscription/list?output=json`
pub async fn subscription_list(ctx: Ctx<'_>) -> Response {
    if ctx.param("output").as_deref() != Some("json") {
        return plain(StatusCode::NOT_IMPLEMENTED, "Not Implemented!");
    }
    let id_user = ctx.user.id;
    let (feeds, categories) = match ctx
        .state
        .db
        .with(move |mut conn| async move {
            let feeds = feed_dao::list(&mut conn, id_user).await?;
            let categories = cat_dao::list(&mut conn, id_user).await?;
            Ok((feeds, categories))
        })
        .await
    {
        Ok(both) => both,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let categories: HashMap<i64, String> = categories.into_iter().map(|c| (c.id, c.name)).collect();
    let base = origin_base(&ctx);

    let subscriptions: Vec<Value> = feeds
        .iter()
        .filter(|f| f.priority.get() > crate::domain::priority::Priority::HIDDEN.get())
        .map(|feed| {
            let cats: Vec<Value> = feed
                .category
                .and_then(|id| categories.get(&id))
                .map(|name| {
                    json!({
                        "id": format!("user/-/label/{}", escape_to_unicode_alternative(name, false)),
                        "label": name,
                    })
                })
                .into_iter()
                .collect();
            json!({
                "id": format!("feed/{}", feed.id),
                "title": escape_to_unicode_alternative(&feed.name, true),
                "categories": cats,
                "url": decode_url(&feed.url),
                "htmlUrl": decode_url(&feed.website),
                "iconUrl": format!("{base}/api/v1/feeds/{}/icon", feed.id),
                "frss:priority": priority_name(feed.priority.get()),
            })
        })
        .collect();
    json_body(json!({ "subscriptions": subscriptions }))
}

/// `GET /reader/api/0/subscription/export` — the OPML, as an attachment.
pub async fn subscription_export(ctx: Ctx<'_>) -> Response {
    let id_user = ctx.user.id;
    let doc = match crate::service::export::opml(&ctx.state.db, id_user).await {
        Ok(doc) => doc,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };
    let body = freshrss_feed::opml::write(&doc);
    let stamp = today_stamp();
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/xml; charset=UTF-8".to_owned()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"feeds_{stamp}.opml.xml\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// `GET /reader/api/0/unread-count?output=json`
pub async fn unread_count(ctx: Ctx<'_>) -> Response {
    if ctx.param("output").as_deref() != Some("json") {
        return plain(StatusCode::NOT_IMPLEMENTED, "Not Implemented!");
    }
    let id_user = ctx.user.id;
    let (feeds, tags, newest_feeds, newest_tags) = match ctx
        .state
        .db
        .with(move |mut conn| async move {
            let feeds = feed_dao::list(&mut conn, id_user).await?;
            let tags = tag_dao::list_with_counts(&mut conn, id_user).await?;
            let newest_feeds = entry_dao::newest_item_per_feed(&mut conn, id_user).await?;
            let newest_tags = tag_dao::newest_item_per_tag(&mut conn, id_user).await?;
            Ok((feeds, tags, newest_feeds, newest_tags))
        })
        .await
    {
        Ok(all) => all,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!"),
    };

    let mut counts: Vec<Value> = Vec::new();
    let mut total: i64 = 0;
    let mut overall_newest: i64 = 0;
    for feed in &feeds {
        if feed.priority.get() <= crate::domain::priority::Priority::HIDDEN.get() {
            continue;
        }
        total += feed.cache_nb_unreads;
        let newest = newest_feeds.get(&feed.id).copied().unwrap_or(0);
        overall_newest = overall_newest.max(newest);
        counts.push(json!({
            "id": format!("feed/{}", feed.id),
            "count": feed.cache_nb_unreads,
            "newestItemTimestampUsec": newest.to_string(),
        }));
    }
    for (tag, _total, unread) in &tags {
        let newest = newest_tags.get(&tag.id).copied().unwrap_or(0);
        counts.push(json!({
            "id": format!("user/-/label/{}", escape_to_unicode_alternative(&tag.name, false)),
            "count": unread,
            "newestItemTimestampUsec": newest.to_string(),
        }));
    }
    counts.push(json!({
        "id": "user/-/state/com.google/reading-list",
        "count": total,
        "newestItemTimestampUsec": overall_newest.to_string(),
    }));
    json_body(json!({ "max": total, "unreadcounts": counts }))
}

fn priority_name(priority: i8) -> &'static str {
    match priority {
        20 => "important",
        10 => "main",
        0 => "category",
        _ => "feed",
    }
}

/// `scheme://host` of this request, for absolute icon URLs.
fn origin_base(ctx: &Ctx<'_>) -> String {
    let host = ctx
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = ctx
        .headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

fn now_secs() -> i64 {
    crate::domain::ids::now_unix()
}

/// `feeds_YYYY-MM-DD.opml.xml`, in UTC, matching the filename upstream generates.
fn today_stamp() -> String {
    jiff::Timestamp::from_second(now_secs())
        .expect("current time is in range")
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .to_string()
}
