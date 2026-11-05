//! The Google Reader compatible API, driven through the real router.
//!
//! The contract here is what third-party clients parse, so the assertions are about wire shapes:
//! the ClientLogin body, the hex-versus-decimal item ids, the continuation token that must neither
//! skip nor repeat a row, and the status codes and headers that tell a client "wrong token" from
//! "server broken".

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use freshrss_feed::http::{FixtureFetcher, FixtureResponse};
use freshrss_feed::Fetcher;
use freshrss_server::config::AppConfig;
use freshrss_server::db::Db;
use freshrss_server::router::{router, AppState};
use freshrss_server::service::{auth, subscription};

const FEED_URL: &str = "https://e.example/feed.xml";

const FEED_BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://e.example/</link>
<item><title>One</title><link>https://e.example/1</link><guid>g1</guid><description>Body one</description></item>
<item><title>Two</title><link>https://e.example/2</link><guid>g2</guid><description>Body two</description></item>
<item><title>Three</title><link>https://e.example/3</link><guid>g3</guid><description>Body three</description></item>
</channel></rss>"#;

struct Reply {
    status: StatusCode,
    body: Value,
    text: String,
    headers: Vec<(String, String)>,
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    auth: Option<&str>,
    body: Option<String>,
    content_type: Option<&str>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "reader.example");
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let (payload, builder) = match body {
        Some(body) => (
            Body::from(body),
            builder.header(
                header::CONTENT_TYPE,
                content_type.unwrap_or("application/x-www-form-urlencoded"),
            ),
        ),
        None => (Body::empty(), builder),
    };
    let response = app
        .clone()
        .oneshot(builder.body(payload).expect("request"))
        .await
        .expect("served");
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_str(&text).unwrap_or(Value::Null);
    Reply {
        status,
        body,
        text,
        headers,
    }
}

fn header<'a>(reply: &'a Reply, name: &str) -> Option<&'a str> {
    reply
        .headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// A server with one user, an API password, and one subscribed fixture feed.
async fn signed_in() -> (Router, String) {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(FEED_URL, rss(FEED_BODY)));
    let config = AppConfig::default();
    let app = router(AppState::with_fetcher(db.clone(), config, fetcher.clone()));

    auth::create_user(&db, "alice", None, "correct-horse-battery", false)
        .await
        .expect("user");
    auth::set_api_password(&db, "alice", "api-password-1")
        .await
        .expect("api password");
    let user = auth::client_login(&db, "alice", "api-password-1")
        .await
        .expect("client login");
    let token = auth::greader_token(&app_config(&db).await, &user).expect("token");

    let spec = subscription::NewSubscription {
        id_user: user.id,
        url: FEED_URL.to_owned(),
        name: None,
        category: None,
        kind: freshrss_server::domain::kind::FeedKind::Rss,
        priority: freshrss_server::domain::priority::Priority::MAIN_STREAM,
        ttl: freshrss_server::domain::ttl::Ttl::default(),
    };
    subscription::add(
        &db,
        &*fetcher,
        &freshrss_server::service::refresh::Settings::default(),
        &spec,
        freshrss_server::domain::ids::now_unix(),
    )
    .await
    .expect("subscribed");

    (app, format!("GoogleLogin auth=alice/{token}"))
}

async fn app_config(db: &Db) -> AppConfig {
    // The token salt is the session secret; the default config is what the router was built with.
    let _ = db;
    AppConfig::default()
}

fn rss(body: &str) -> FixtureResponse {
    FixtureResponse::Body {
        status: 200,
        body: body.as_bytes().to_vec(),
        etag: None,
        last_modified: None,
        content_type: Some("application/rss+xml".to_owned()),
    }
}

#[tokio::test]
async fn client_login_answers_in_the_three_line_form() {
    let (app, _) = signed_in().await;
    let reply = call(
        &app,
        Method::POST,
        "/api/greader.php/accounts/ClientLogin",
        None,
        Some("Email=alice&Passwd=api-password-1".to_owned()),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.text.starts_with("SID=alice/"), "{}", reply.text);
    assert!(reply.text.contains("LSID=null"), "{}", reply.text);
    let auth = reply
        .text
        .lines()
        .find(|l| l.starts_with("Auth="))
        .expect("an Auth line");
    let token = auth.trim_start_matches("Auth=alice/");
    assert_eq!(token.len(), 40, "a sha1 hex: {token}");

    let wrong = call(
        &app,
        Method::POST,
        "/api/greader.php/accounts/ClientLogin",
        None,
        Some("Email=alice&Passwd=nope".to_owned()),
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(header(&wrong, "google-bad-token"), Some("true"));

    let unknown = call(
        &app,
        Method::POST,
        "/api/greader.php/accounts/ClientLogin",
        None,
        Some("Email=nobody&Passwd=x".to_owned()),
        None,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn requests_without_the_header_are_unauthorized() {
    let (app, _) = signed_in().await;
    let reply = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/user-info",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(header(&reply, "google-bad-token"), Some("true"));
}

#[tokio::test]
async fn user_info_and_the_compat_probes() {
    let (app, auth) = signed_in().await;
    let info = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/user-info",
        Some(&auth),
        None,
        None,
    )
    .await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.body["userName"], "alice");

    let check = call(
        &app,
        Method::GET,
        "/api/greader.php/check/compatibility",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(check.text, "PASS");

    let options = call(
        &app,
        Method::OPTIONS,
        "/api/greader.php/reader/api/0/user-info",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(options.status, StatusCode::NO_CONTENT);
    assert_eq!(header(&options, "access-control-allow-origin"), Some("*"));
}

#[tokio::test]
async fn stream_contents_pages_without_skipping_or_repeating() {
    let (app, auth) = signed_in().await;
    let first = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/stream/contents/user/-/state/com.google/reading-list?n=2",
        Some(&auth),
        None,
        None,
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let items = first.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    let continuation = first.body["continuation"]
        .as_str()
        .expect("a full page carries a continuation")
        .to_owned();
    // Item ids are the hex tag URIs; timestampUsec is the decimal id as a string.
    let id = items[0]["id"].as_str().expect("id");
    assert!(id.starts_with("tag:google.com,2005:reader/item/"), "{id}");
    assert_eq!(id.len(), "tag:google.com,2005:reader/item/".len() + 16);
    let usec = items[0]["timestampUsec"].as_str().expect("usec");
    assert!(usec.chars().all(|c| c.is_ascii_digit()), "{usec}");
    assert!(
        items[0]["categories"]
            .as_array()
            .expect("categories")
            .iter()
            .any(|c| c == "user/-/state/com.google/reading-list"),
        "{:?}",
        items[0]["categories"]
    );

    let second = call(
        &app,
        Method::GET,
        &format!(
            "/api/greader.php/reader/api/0/stream/contents/user/-/state/com.google/reading-list?n=2&c={continuation}"
        ),
        Some(&auth),
        None,
        None,
    )
    .await;
    let page2 = second.body["items"].as_array().expect("items");
    assert_eq!(page2.len(), 1, "three articles, two on the first page");
    let seen: Vec<&str> = items
        .iter()
        .chain(page2.iter())
        .map(|i| i["id"].as_str().expect("id"))
        .collect();
    let mut deduped = seen.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(seen.len(), deduped.len(), "no row appears on two pages");
    assert!(
        second.body.get("continuation").is_none(),
        "a short page ends the stream"
    );
}

#[tokio::test]
async fn item_ids_are_decimal_and_items_contents_accepts_both_spellings() {
    let (app, auth) = signed_in().await;
    let ids = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/stream/items/ids?s=user/-/state/com.google/reading-list&n=5",
        Some(&auth),
        None,
        None,
    )
    .await;
    let refs = ids.body["itemRefs"].as_array().expect("refs");
    assert_eq!(refs.len(), 3);
    assert!(refs[0]["id"]
        .as_str()
        .expect("id")
        .chars()
        .all(|c| c.is_ascii_digit()));

    let hex = format!(
        "tag:google.com,2005:reader/item/{:016x}",
        refs[0]["id"].as_str().unwrap().parse::<i64>().unwrap()
    );
    let body = format!(
        "i={}&i={}",
        hex,
        refs[1]["id"].as_str().expect("decimal id")
    );
    let contents = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/stream/items/contents",
        Some(&auth),
        Some(body),
        None,
    )
    .await;
    assert_eq!(contents.status, StatusCode::OK);
    assert_eq!(contents.body["items"].as_array().expect("items").len(), 2);
}

#[tokio::test]
async fn the_three_listings_require_output_json() {
    let (app, auth) = signed_in().await;
    for path in [
        "/reader/api/0/tag/list",
        "/reader/api/0/subscription/list",
        "/reader/api/0/unread-count",
    ] {
        let refused = call(
            &app,
            Method::GET,
            &format!("/api/greader.php{path}"),
            Some(&auth),
            None,
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::NOT_IMPLEMENTED, "{path}");
        let ok = call(
            &app,
            Method::GET,
            &format!("/api/greader.php{path}?output=json"),
            Some(&auth),
            None,
            None,
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn subscription_and_unread_shapes() {
    let (app, auth) = signed_in().await;
    let subs = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/subscription/list?output=json",
        Some(&auth),
        None,
        None,
    )
    .await;
    let sub = &subs.body["subscriptions"][0];
    assert_eq!(sub["id"], "feed/1");
    assert_eq!(sub["frss:priority"], "main");
    assert!(sub["iconUrl"]
        .as_str()
        .expect("icon")
        .contains("/api/v1/feeds/1/icon"));

    let counts = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/unread-count?output=json",
        Some(&auth),
        None,
        None,
    )
    .await;
    let feed_count = counts.body["unreadcounts"]
        .as_array()
        .expect("counts")
        .iter()
        .find(|c| c["id"] == "feed/1")
        .expect("the feed");
    assert_eq!(feed_count["count"], 3);
    assert!(feed_count["newestItemTimestampUsec"].as_str().is_some());
}

#[tokio::test]
async fn edit_tag_marks_read_stars_and_labels_with_the_token_holes() {
    let (app, auth) = signed_in().await;
    let ids = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/stream/items/ids?s=user/-/state/com.google/reading-list",
        Some(&auth),
        None,
        None,
    )
    .await;
    let first = ids.body["itemRefs"][0]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // FeedMe sends no T at all; the mutation must still apply.
    let marked = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/edit-tag",
        Some(&auth),
        Some(format!("i={first}&a=user/-/state/com.google/read")),
        None,
    )
    .await;
    assert_eq!(marked.status, StatusCode::OK);
    let counts = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/unread-count?output=json",
        Some(&auth),
        None,
        None,
    )
    .await;
    let feed = counts.body["unreadcounts"]
        .as_array()
        .expect("counts")
        .iter()
        .find(|c| c["id"] == "feed/1")
        .expect("feed");
    assert_eq!(feed["count"], 2, "one of three is read now");

    let starred = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/edit-tag",
        Some(&auth),
        Some(format!("i={first}&a=user/-/state/com.google/starred&T=x")),
        None,
    )
    .await;
    assert_eq!(starred.status, StatusCode::OK);

    let labelled = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/edit-tag",
        Some(&auth),
        Some(format!("i={first}&a=user/-/label/todo")),
        None,
    )
    .await;
    assert_eq!(labelled.status, StatusCode::OK);
    let tags = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/tag/list?output=json",
        Some(&auth),
        None,
        None,
    )
    .await;
    let todo = tags.body["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .find(|t| t["id"] == "user/-/label/todo")
        .expect("the new label");
    assert_eq!(todo["type"], "tag");
    assert_eq!(
        todo["unread_count"], 0,
        "the labelled article was read first"
    );

    // A wrong T is the one thing that must be refused.
    let token = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/token",
        Some(&auth),
        None,
        None,
    )
    .await;
    assert_eq!(token.text.trim_end().len(), 57, "{}", token.text);
    let bad = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/edit-tag",
        Some(&auth),
        Some(format!("i={first}&a=user/-/state/com.google/read&T=wrong")),
        None,
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mark_all_as_read_honours_the_ts_bound() {
    let (app, auth) = signed_in().await;
    let ids = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/stream/items/ids?s=user/-/state/com.google/reading-list&r=o",
        Some(&auth),
        None,
        None,
    )
    .await;
    let refs: Vec<i64> = ids.body["itemRefs"]
        .as_array()
        .expect("refs")
        .iter()
        .map(|r| r["id"].as_str().expect("id").parse().expect("decimal"))
        .collect();
    let bound = refs[0];

    let reply = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/mark-all-as-read",
        Some(&auth),
        Some(format!(
            "s=user/-/state/com.google/reading-list&ts={bound}&T=x"
        )),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    let counts = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/unread-count?output=json",
        Some(&auth),
        None,
        None,
    )
    .await;
    let feed = counts.body["unreadcounts"]
        .as_array()
        .expect("counts")
        .iter()
        .find(|c| c["id"] == "feed/1")
        .expect("feed");
    assert_eq!(
        feed["count"], 2,
        "only the oldest article was within the bound"
    );

    let bad = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/mark-all-as-read",
        Some(&auth),
        Some("s=user/-/state/com.google/reading-list&ts=notdigits".to_owned()),
        None,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn quickadd_reports_failure_with_a_200_and_edit_unsubscribes() {
    let (app, auth) = signed_in().await;
    let failed = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/subscription/quickadd?quickadd=https://e.example/not-a-feed",
        Some(&auth),
        None,
        None,
    )
    .await;
    assert_eq!(failed.status, StatusCode::OK);
    assert_eq!(failed.body["numResults"], 0);
    assert!(failed.body["error"].as_str().is_some());

    let removed = call(
        &app,
        Method::POST,
        "/api/greader.php/reader/api/0/subscription/edit",
        Some(&auth),
        Some("ac=unsubscribe&s=feed/1".to_owned()),
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK);
    let subs = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/subscription/list?output=json",
        Some(&auth),
        None,
        None,
    )
    .await;
    assert_eq!(
        subs.body["subscriptions"].as_array().expect("subs").len(),
        0
    );
}

#[tokio::test]
async fn unknown_paths_are_bad_requests_and_disabled_api_is_503() {
    let (app, auth) = signed_in().await;
    let unknown = call(
        &app,
        Method::GET,
        "/api/greader.php/reader/api/0/nonsense",
        Some(&auth),
        None,
        None,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.text, "Bad Request!");
}
