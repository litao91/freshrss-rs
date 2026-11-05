//! The Fever compatible API, driven through the real router.
//!
//! Fever's contract is an envelope rather than status codes: a bad credential is HTTP 200 with
//! `auth: 0`, and every payload key appears only when the request asked for it. The assertions
//! follow that shape, including the quirks clients parse around — string item ids, `favicon_id`
//! equal to the feed id, and the hardcoded 50-item page.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
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

async fn call(app: &Router, query: &str, key: Option<&str>) -> (StatusCode, Value) {
    let body = match key {
        Some(key) => format!("api_key={key}&{query}"),
        None => query.to_owned(),
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/fever.php")
                .header("host", "reader.example")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("served");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// A server with one user, an API password, one subscribed fixture feed, and the Fever key.
async fn ready() -> (Router, String) {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(FEED_URL, rss(FEED_BODY)));
    let app = router(AppState::with_fetcher(
        db.clone(),
        AppConfig::default(),
        fetcher.clone(),
    ));
    auth::create_user(&db, "alice", None, "correct-horse-battery", false)
        .await
        .expect("user");
    let user = auth::set_api_password(&db, "alice", "api-password-1")
        .await
        .expect("api password");
    let key = user.config.fever_key.clone().expect("a fever key");

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
    (app, key)
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
async fn a_wrong_key_is_a_200_with_auth_zero() {
    let (app, _) = ready().await;
    let (status, body) = call(&app, "groups", Some("deadbeef")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "api_version": 4, "auth": 0 }));

    let (status, body) = call(&app, "groups", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "api_version": 4, "auth": 0 }));
}

#[tokio::test]
async fn refresh_is_refused_before_authentication() {
    let (app, _) = ready().await;
    let (status, _) = call(&app, "refresh", None).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn groups_feeds_and_their_mapping() {
    let (app, key) = ready().await;
    let (_, body) = call(&app, "groups&feeds", Some(&key)).await;
    assert_eq!(body["auth"], 1);
    assert!(body["last_refreshed_on_time"].is_number());
    assert_eq!(body["groups"][0]["id"], 1);
    let feed = &body["feeds"][0];
    assert_eq!(feed["id"], 1);
    assert_eq!(
        feed["favicon_id"], 1,
        "the favicon id space is the feed id space"
    );
    assert_eq!(feed["is_spark"], 0);
    assert_eq!(feed["title"], "Example");
    assert_eq!(body["feeds_groups"][0]["group_id"], 1);
    assert_eq!(body["feeds_groups"][0]["feed_ids"], "1");
}

#[tokio::test]
async fn items_page_by_exclusive_cursors() {
    let (app, key) = ready().await;
    let (_, body) = call(&app, "items", Some(&key)).await;
    assert_eq!(body["total_items"], 3);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3);
    assert!(items[0]["id"].is_string(), "fever item ids are strings");
    let first: i64 = items[0]["id"].as_str().unwrap().parse().unwrap();
    let last: i64 = items[2]["id"].as_str().unwrap().parse().unwrap();

    let (_, after) = call(&app, &format!("items&since_id={first}"), Some(&key)).await;
    let paged = after["items"].as_array().expect("items");
    assert_eq!(paged.len(), 2, "since_id is exclusive");
    let (_, before) = call(&app, &format!("items&max_id={last}"), Some(&key)).await;
    let back = before["items"].as_array().expect("items");
    assert_eq!(back.len(), 2, "max_id is exclusive");
}

#[tokio::test]
async fn marking_an_item_read_moves_it_between_the_id_lists() {
    let (app, key) = ready().await;
    let (_, body) = call(&app, "unread_item_ids", Some(&key)).await;
    let unread = body["unread_item_ids"].as_str().expect("a string");
    assert_eq!(unread.split(',').count(), 3);
    let first = unread.split(',').next().unwrap().to_owned();

    let (_, marked) = call(&app, &format!("mark=item&as=read&id={first}"), Some(&key)).await;
    let after = marked["unread_item_ids"].as_str().expect("a string");
    assert_eq!(after.split(',').count(), 2, "{after}");
    assert!(!after.split(',').any(|id| id == first));

    let (_, saved) = call(&app, &format!("mark=item&as=saved&id={first}"), Some(&key)).await;
    assert_eq!(saved["saved_item_ids"].as_str().expect("ids"), first);
}

#[tokio::test]
async fn marking_group_zero_reads_everything() {
    let (app, key) = ready().await;
    let (_, marked) = call(&app, "mark=group&as=read&id=0&before=0", Some(&key)).await;
    assert_eq!(marked["unread_item_ids"], "");
    let (_, body) = call(&app, "unread_item_ids", Some(&key)).await;
    assert_eq!(body["unread_item_ids"], "");
}

#[tokio::test]
async fn links_are_always_empty_and_unknown_keys_add_nothing() {
    let (app, key) = ready().await;
    let (_, body) = call(&app, "links", Some(&key)).await;
    assert_eq!(body["links"], json!([]));
    let (_, bare) = call(&app, "", Some(&key)).await;
    assert_eq!(bare["auth"], 1);
    assert!(bare.get("items").is_none());
    assert!(bare.get("groups").is_none());
}
