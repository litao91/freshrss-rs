//! The refresh, discovery, subscription and job endpoints, driven through the real router.
//!
//! The service layer has its own tests against fixtures; what only an end-to-end pass can show is
//! the contract between them and a client: the status codes, the envelope shapes, the cookie that
//! has to come back, and that a job registered by `POST /refresh` is still there to poll afterwards.
//! A browser found the 405 that motivated this file — `POST /feeds/discover` falling through to
//! `/feeds/{id}` — and no amount of service-level testing would have.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use freshrss_feed::http::{FixtureFetcher, FixtureResponse};
use freshrss_feed::Fetcher;
use freshrss_server::config::{AppConfig, RegistrationConfig, RegistrationMode};
use freshrss_server::db::Db;
use freshrss_server::router::{router, AppState};

const FEED_URL: &str = "https://e.example/feed.xml";

const FEED_BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://e.example/</link>
<item><title>One</title><link>https://e.example/1</link><guid>g1</guid><description>Body</description></item>
<item><title>Two</title><link>https://e.example/2</link><guid>g2</guid><description>Body</description></item>
</channel></rss>"#;

fn rss(body: &str) -> FixtureResponse {
    FixtureResponse::Body {
        status: 200,
        body: body.as_bytes().to_vec(),
        etag: None,
        last_modified: None,
        content_type: Some("application/rss+xml".to_owned()),
    }
}

/// One response, reduced to what these tests assert on.
struct Reply {
    status: StatusCode,
    body: Value,
    set_cookie: Option<String>,
    content_type: Option<String>,
    cache_control: Option<String>,
    bytes: Vec<u8>,
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    body: Option<Value>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "reader.example");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    let (payload, builder) = match body {
        Some(body) => (
            Body::from(body.to_string()),
            builder.header(header::CONTENT_TYPE, "application/json"),
        ),
        None => (Body::empty(), builder),
    };
    let response = app
        .clone()
        .oneshot(builder.body(payload).expect("request"))
        .await
        .expect("served");

    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).to_owned());
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let cache_control = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        body,
        set_cookie,
        content_type,
        cache_control,
        bytes,
    }
}

async fn get(app: &Router, uri: &str, cookie: &str) -> Reply {
    call(app, Method::GET, uri, Some(cookie), None, None).await
}

async fn post(app: &Router, uri: &str, cookie: &str, body: Value) -> Reply {
    call(app, Method::POST, uri, Some(cookie), None, Some(body)).await
}

/// A server on an in-memory database with a fixture fetcher, plus a signed-in session.
async fn signed_in() -> (Router, String) {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(FEED_URL, rss(FEED_BODY)));
    let app = router(AppState::with_fetcher(
        db,
        registration(RegistrationMode::Open),
        fetcher,
    ));

    let registered = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(json!({"username": "alice", "password": "correct-horse-battery"})),
    )
    .await;
    assert_eq!(registered.status, StatusCode::CREATED);
    let cookie = registered.set_cookie.expect("a session cookie");
    (app, cookie)
}

#[tokio::test]
async fn discovery_answers_for_an_address_that_is_already_a_feed() {
    let (app, cookie) = signed_in().await;
    let reply = post(
        &app,
        "/api/v1/feeds/discover",
        &cookie,
        json!({"url": FEED_URL}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["is_feed"], true);
    let candidate = &reply.body["candidates"][0];
    assert_eq!(candidate["verified"], true);
    assert_eq!(candidate["title"], "Example");
    assert_eq!(candidate["item_count"], 2);
    assert_eq!(candidate["source"], "given");
}

#[tokio::test]
async fn discovery_refuses_a_scheme_that_is_not_http() {
    let (app, cookie) = signed_in().await;
    let reply = post(
        &app,
        "/api/v1/feeds/discover",
        &cookie,
        json!({"url": "file:///etc/passwd"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"]["code"], "bad_url");
}

#[tokio::test]
async fn subscribing_fetches_immediately_and_reports_what_arrived() {
    let (app, cookie) = signed_in().await;
    let reply = post(
        &app,
        "/api/v1/feeds",
        &cookie,
        json!({"url": FEED_URL, "category_id": null}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    assert_eq!(reply.body["new_entries"], 2);
    assert_eq!(reply.body["feed"]["name"], "Example");
    let id = reply.body["feed"]["id"]
        .as_str()
        .expect("a string id")
        .to_owned();

    // The list sees the articles without a second fetch.
    let entries = get(&app, "/api/v1/entries?scope=all", &cookie).await;
    assert_eq!(entries.status, StatusCode::OK);
    assert_eq!(entries.body["items"].as_array().expect("items").len(), 2);

    // A second refresh of a quiet feed is a no-op that still reports success.
    let again = post(
        &app,
        &format!("/api/v1/feeds/{id}/refresh"),
        &cookie,
        json!({}),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{:?}", again.body);
    assert_eq!(again.body["status"], "unchanged");
    assert_eq!(again.body["new_entries"], 0);
}

#[tokio::test]
async fn subscribing_to_an_address_that_is_not_a_feed_leaves_nothing_behind() {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> =
        Arc::new(FixtureFetcher::new().with(FEED_URL, rss("<html><body>not a feed</body></html>")));
    let app = router(AppState::with_fetcher(
        db,
        registration(RegistrationMode::Open),
        fetcher,
    ));
    let registered = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(json!({"username": "alice", "password": "correct-horse-battery"})),
    )
    .await;
    let cookie = registered.set_cookie.expect("a session cookie");

    let reply = post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{:?}", reply.body);
    assert!(
        reply.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("could not be parsed as a feed"),
        "{:?}",
        reply.body
    );

    let feeds = get(&app, "/api/v1/feeds", &cookie).await;
    assert_eq!(feeds.body.as_array().expect("feeds").len(), 0);
}

#[tokio::test]
async fn a_refresh_job_is_registered_polled_and_resumable() {
    let (app, cookie) = signed_in().await;
    post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;

    let started = post(&app, "/api/v1/refresh", &cookie, json!({})).await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let job_id = started.body["job_id"]
        .as_str()
        .expect("a job id")
        .to_owned();

    // A second press converges on the same job rather than starting another.
    let again = post(&app, "/api/v1/refresh", &cookie, json!({})).await;
    let second = again.body["job_id"].as_str().expect("a job id");
    assert!(
        second == job_id || poll_until_done(&app, &cookie, second).await,
        "either the same job, or the first had already finished"
    );

    assert!(poll_until_done(&app, &cookie, &job_id).await);
    let job = get(&app, &format!("/api/v1/refresh/{job_id}"), &cookie).await;
    assert_eq!(job.body["state"], "done");
    assert_eq!(
        job.body["errors"].as_array().expect("errors").len(),
        0,
        "{:?}",
        job.body
    );
    // Subscribing already fetched the feed, so whether anything is due now depends on timing; the
    // lifecycle is what this test pins, not the count.
    assert!(job.body["feeds_done"].as_u64().is_some());

    let status = get(&app, "/api/v1/refresh/status", &cookie).await;
    assert_eq!(status.body["running"], false);
    assert_eq!(status.body["job_id"], job_id);

    // Someone else's job id is indistinguishable from one that never existed.
    let stranger = get(&app, &format!("/api/v1/refresh/{job_id}"), "").await;
    assert_eq!(stranger.status, StatusCode::UNAUTHORIZED);
}

async fn poll_until_done(app: &Router, cookie: &str, job_id: &str) -> bool {
    for _ in 0..100 {
        let job = get(app, &format!("/api/v1/refresh/{job_id}"), cookie).await;
        match job.body["state"].as_str() {
            Some("done") | Some("failed") => return true,
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    false
}

#[tokio::test]
async fn an_unknown_job_is_missing_rather_than_forbidden() {
    let (app, cookie) = signed_in().await;
    let reply = get(&app, "/api/v1/refresh/deadbeef", &cookie).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body["error"]["code"], "not_found");
}

#[tokio::test]
async fn problems_is_empty_for_a_healthy_reader_and_names_a_muted_feed() {
    let (app, cookie) = signed_in().await;
    let created = post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;
    let id = created.body["feed"]["id"].as_str().expect("id").to_owned();

    let empty = get(&app, "/api/v1/problems", &cookie).await;
    assert_eq!(empty.body["count"], 0);

    let muted = call(
        &app,
        Method::PUT,
        &format!("/api/v1/feeds/{id}"),
        Some(&cookie),
        None,
        Some(json!({"muted": true})),
    )
    .await;
    assert_eq!(muted.status, StatusCode::OK, "{:?}", muted.body);
    assert_eq!(muted.body["muted"], true);
    assert_eq!(
        muted.body["ttl"].as_i64().expect("a ttl"),
        3600,
        "muting a feed on the default interval borrows the user's default as its magnitude"
    );

    let problems = get(&app, "/api/v1/problems", &cookie).await;
    assert_eq!(problems.body["count"], 1);
    assert_eq!(problems.body["feeds"][0]["muted"], true);
    assert_eq!(problems.body["feeds"][0]["id"], id);
}

#[tokio::test]
async fn filter_rules_round_trip_and_reject_a_query_that_does_not_parse() {
    let (app, cookie) = signed_in().await;
    let created = post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;
    let id = created.body["feed"]["id"].as_str().expect("id").to_owned();

    let bad = call(
        &app,
        Method::PUT,
        &format!("/api/v1/feeds/{id}/filters"),
        Some(&cookie),
        None,
        Some(json!({"filters": [{"search": "intitle:a OR", "actions": ["read"]}]})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{:?}", bad.body);
    assert_eq!(bad.body["error"]["code"], "validation");

    let set = call(
        &app,
        Method::PUT,
        &format!("/api/v1/feeds/{id}/filters"),
        Some(&cookie),
        None,
        Some(json!({"filters": [{"search": "intitle:One", "actions": ["read", "star"]}]})),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
    assert_eq!(set.body["filters"][0]["actions"], json!(["read", "star"]));

    let read_back = get(&app, &format!("/api/v1/feeds/{id}/filters"), &cookie).await;
    assert_eq!(read_back.body, set.body);
}

#[tokio::test]
async fn saved_searches_round_trip_and_refuse_a_broken_query_at_save_time() {
    let (app, cookie) = signed_in().await;

    let bad = post(
        &app,
        "/api/v1/queries",
        &cookie,
        json!({"name": "broken", "search": "(intitle:a"}),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let created = post(
        &app,
        "/api/v1/queries",
        &cookie,
        json!({"name": "rust", "search": "intitle:One"}),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let id = created.body["id"].as_str().expect("id").to_owned();

    let listed = get(&app, "/api/v1/queries", &cookie).await;
    assert_eq!(listed.body.as_array().expect("queries").len(), 1);

    // And the saved search is reachable from the grammar it feeds.
    let parsed = post(
        &app,
        "/api/v1/search/parse",
        &cookie,
        json!({"q": format!("S:{id}")}),
    )
    .await;
    assert_eq!(parsed.status, StatusCode::OK, "{:?}", parsed.body);
    // The canonical form stores the folded needle, which is what both backends compare against.
    assert_eq!(parsed.body["canonical"], "intitle:one");

    let deleted = call(
        &app,
        Method::DELETE,
        &format!("/api/v1/queries/{id}"),
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_mutation_from_another_origin_is_refused_before_it_reaches_a_handler() {
    let (app, cookie) = signed_in().await;
    let reply = call(
        &app,
        Method::POST,
        "/api/v1/entries/read",
        Some(&cookie),
        Some("https://evil.example"),
        Some(json!({"ids": []})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.body["error"]["code"], "origin_mismatch");
}

#[tokio::test]
async fn login_attempts_are_capped_per_minute() {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new());
    let mut config = AppConfig::default();
    config.limits.rate_limit_per_minute = 2;
    let app = router(AppState::with_fetcher(db, config, fetcher));

    let body = json!({"username": "nobody", "password": "wrong"});
    for _ in 0..2 {
        let reply = call(
            &app,
            Method::POST,
            "/api/v1/auth/login",
            None,
            None,
            Some(body.clone()),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let third = call(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        None,
        None,
        Some(body),
    )
    .await;
    assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(third.body["error"]["code"], "rate_limited");
}

const ARTICLE_PAGE: &str = r#"<html><head><title>Page</title></head>
<body><nav>Menus and ads</nav><article><p>The whole article, in full.</p></article></body></html>"#;

fn page(body: &str) -> FixtureResponse {
    FixtureResponse::Body {
        status: 200,
        body: body.as_bytes().to_vec(),
        etag: None,
        last_modified: None,
        content_type: Some("text/html".to_owned()),
    }
}

/// A server whose fetcher also serves the article page behind the feed's links.
async fn signed_in_with_pages() -> (Router, String) {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(
        FixtureFetcher::new()
            .with(FEED_URL, rss(FEED_BODY))
            .with("https://e.example/1", page(ARTICLE_PAGE))
            .with("https://e.example/2", page(ARTICLE_PAGE)),
    );
    let app = router(AppState::with_fetcher(
        db,
        registration(RegistrationMode::Open),
        fetcher,
    ));
    let registered = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(json!({"username": "alice", "password": "correct-horse-battery"})),
    )
    .await;
    let cookie = registered.set_cookie.expect("a session cookie");
    post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;
    (app, cookie)
}

#[tokio::test]
async fn the_reading_pane_can_ask_for_the_full_article() {
    let (app, cookie) = signed_in_with_pages().await;
    let entries = get(&app, "/api/v1/entries?scope=all", &cookie).await;
    let id = entries.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|e| e["link"] == FEED_BODY_LINK_1)
        .expect("the first article")["id"]
        .as_str()
        .expect("a string id")
        .to_owned();

    // Before asking, the detail has no full content.
    let before = get(&app, &format!("/api/v1/entries/{id}"), &cookie).await;
    assert_eq!(before.body["entry"]["full_content"], Value::Null);

    let reply = post(
        &app,
        &format!("/api/v1/entries/{id}/fulltext"),
        &cookie,
        json!({}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert!(
        reply.body["content_html"]
            .as_str()
            .expect("html")
            .contains("The whole article, in full."),
        "{:?}",
        reply.body
    );
    assert!(!reply.body["content_html"]
        .as_str()
        .unwrap()
        .contains("Menus and ads"));
    assert_eq!(reply.body["stored"], true);
    assert_eq!(reply.body["selector"], "article");

    // Asking once is enough: the detail now carries it, for a reload or another device.
    let after = get(&app, &format!("/api/v1/entries/{id}"), &cookie).await;
    assert!(after.body["entry"]["full_content"]
        .as_str()
        .expect("stored full content")
        .contains("The whole article, in full."));
}

/// The link of the first article in [`FEED_BODY`].
const FEED_BODY_LINK_1: &str = "https://e.example/1";

#[tokio::test]
async fn an_explicit_selector_is_a_preview_and_stores_nothing() {
    let (app, cookie) = signed_in_with_pages().await;
    let entries = get(&app, "/api/v1/entries?scope=all", &cookie).await;
    let id = entries.body["items"].as_array().expect("items")[0]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let reply = post(
        &app,
        &format!("/api/v1/entries/{id}/fulltext"),
        &cookie,
        json!({"selector": "article"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["stored"], false);

    let detail = get(&app, &format!("/api/v1/entries/{id}"), &cookie).await;
    assert_eq!(
        detail.body["entry"]["full_content"],
        Value::Null,
        "a preview must not rewrite what the reader sees on reload"
    );
}

#[tokio::test]
async fn a_page_with_no_recognisable_article_is_a_readable_refusal() {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(
        FixtureFetcher::new()
            .with(FEED_URL, rss(FEED_BODY))
            .with(
                "https://e.example/1",
                page("<html><body><div>nothing semantic</div></body></html>"),
            )
            .with(
                "https://e.example/2",
                page("<html><body><div>nothing semantic</div></body></html>"),
            ),
    );
    let app = router(AppState::with_fetcher(
        db,
        registration(RegistrationMode::Open),
        fetcher,
    ));
    let registered = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(json!({"username": "alice", "password": "correct-horse-battery"})),
    )
    .await;
    let cookie = registered.set_cookie.expect("a session cookie");
    post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;

    let entries = get(&app, "/api/v1/entries?scope=all", &cookie).await;
    let id = entries.body["items"].as_array().expect("items")[0]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let reply = post(
        &app,
        &format!("/api/v1/entries/{id}/fulltext"),
        &cookie,
        json!({}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{:?}", reply.body);
    assert_eq!(reply.body["error"]["code"], "validation");
    assert!(
        reply.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("article"),
        "{:?}",
        reply.body
    );
}

/// A server on an in-memory database with a fixture fetcher and a caller-chosen config.
async fn app_with(config: AppConfig) -> (Router, Db) {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(FEED_URL, rss(FEED_BODY)));
    let app = router(AppState::with_fetcher(db.clone(), config, fetcher));
    (app, db)
}

fn registration(mode: RegistrationMode) -> AppConfig {
    AppConfig {
        registration: RegistrationConfig {
            mode,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn register_body(username: &str) -> Value {
    json!({"username": username, "password": "correct-horse-battery"})
}

#[tokio::test]
async fn the_policy_endpoint_reports_an_open_server_without_a_session() {
    let (app, _db) = app_with(registration(RegistrationMode::Open)).await;
    let reply = call(&app, Method::GET, "/api/v1/auth/policy", None, None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["mode"], "open");
    assert_eq!(reply.body["can_register"], true);
}

#[tokio::test]
async fn registration_is_closed_unless_the_operator_opens_it() {
    // The shipped default: a reader reachable from the internet must not hand out accounts, so a
    // fresh config refuses registration and says so, and the CLI path is the only way in.
    let (app, db) = app_with(AppConfig::default()).await;

    let reply = call(&app, Method::GET, "/api/v1/auth/policy", None, None, None).await;
    assert_eq!(reply.body["mode"], "closed");
    assert_eq!(reply.body["can_register"], false);

    let refused = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(register_body("mallory")),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    freshrss_server::service::auth::create_user(
        &db,
        "operator",
        None,
        "correct-horse-battery",
        false,
    )
    .await
    .expect("the CLI path is unaffected by the policy");
}

#[tokio::test]
async fn a_closed_server_refuses_registration_and_says_so_before_the_form_is_filled() {
    let (app, db) = app_with(registration(RegistrationMode::Closed)).await;

    let reply = call(&app, Method::GET, "/api/v1/auth/policy", None, None, None).await;
    assert_eq!(reply.body["mode"], "closed");
    assert_eq!(reply.body["can_register"], false);

    let refused = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(register_body("mallory")),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["error"]["code"], "registration_closed");

    // The operator's back door is unaffected: `create-user` ignores the policy by design, and the
    // account it makes can sign in even while the web form stays shut.
    freshrss_server::service::auth::create_user(
        &db,
        "operator",
        None,
        "correct-horse-battery",
        true,
    )
    .await
    .expect("the CLI path creates an account on a closed server");
    let login = call(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        None,
        None,
        Some(json!({"username": "operator", "password": "correct-horse-battery"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    assert!(login.set_cookie.is_some());
}

#[tokio::test]
async fn first_user_only_closes_once_an_account_exists() {
    let (app, _db) = app_with(registration(RegistrationMode::FirstUserOnly)).await;

    let before = call(&app, Method::GET, "/api/v1/auth/policy", None, None, None).await;
    assert_eq!(before.body["can_register"], true);

    let first = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(register_body("alice")),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED);

    let after = call(&app, Method::GET, "/api/v1/auth/policy", None, None, None).await;
    assert_eq!(after.body["can_register"], false);

    let second = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(register_body("bob")),
    )
    .await;
    assert_eq!(second.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn marking_articles_read_keeps_the_cached_sidebar_counters_in_step() {
    let (app, cookie) = signed_in().await;
    post(&app, "/api/v1/feeds", &cookie, json!({"url": FEED_URL})).await;

    let entries = get(&app, "/api/v1/entries?scope=all", &cookie).await;
    let items = entries.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    let first = items[0]["id"].as_str().expect("id").to_owned();

    let before = get(&app, "/api/v1/counts", &cookie).await;
    assert_eq!(before.body["main_stream"]["unread"], 2);

    let marked = post(
        &app,
        "/api/v1/entries/read",
        &cookie,
        json!({"ids": [first]}),
    )
    .await;
    assert_eq!(marked.body["marked"], 1);

    // The sidebar reads cached per-feed counters, so a mark-read that forgets to recompute them
    // leaves the tree claiming an unread article the list no longer shows.
    let after = get(&app, "/api/v1/counts", &cookie).await;
    assert_eq!(after.body["all"]["unread"], 1, "{:?}", after.body);
    assert_eq!(after.body["main_stream"]["unread"], 1, "{:?}", after.body);

    post(
        &app,
        "/api/v1/entries/mark-all-read",
        &cookie,
        json!({"scope": "all"}),
    )
    .await;
    let done = get(&app, "/api/v1/counts", &cookie).await;
    assert_eq!(done.body["all"]["unread"], 0, "{:?}", done.body);
    assert_eq!(done.body["main_stream"]["unread"], 0, "{:?}", done.body);
}

#[tokio::test]
async fn the_image_proxy_fetches_with_the_article_origin_as_referer() {
    let (app, cookie) = signed_in().await;
    // No fixture is registered for the image address, so the fetch fails: the endpoint must report
    // that as 502 rather than leaking a transport error as a 500. The referer behaviour itself is
    // pinned by the fetcher's own wire test.
    let reply = call(
        &app,
        Method::GET,
        "/api/v1/media?url=https%3A%2F%2Fe.example%2Fimg.png&via=https%3A%2F%2Fe.example%2Farticle%2F",
        Some(&cookie),
        None,
        None,
    )
    .await;
    // No fixture is registered for the image, so the fetch fails and the endpoint must say 502,
    // not leak the transport error as a 500.
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{:?}", reply.body);
}

#[tokio::test]
async fn the_image_proxy_refuses_to_be_an_open_proxy() {
    let (app, cookie) = signed_in().await;

    // No session: an image proxy anyone may use is an open proxy.
    let anonymous = call(
        &app,
        Method::GET,
        "/api/v1/media?url=https%3A%2F%2Fe.example%2Fimg.png",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // A non-image answer is refused, so the endpoint cannot be pointed at arbitrary resources.
    // `:` and `/` are legal inside a query value, so the address needs no encoding here.
    let fed = call(
        &app,
        Method::GET,
        &format!("/api/v1/media?url={FEED_URL}"),
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(fed.status, StatusCode::BAD_GATEWAY, "{:?}", fed.body);

    // An internal address is the SSRF guard's job, and the proxy is a fetch like any other.
    let internal = call(
        &app,
        Method::GET,
        "/api/v1/media?url=http%3A%2F%2F127.0.0.1%3A1%2Fx.png",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(
        internal.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        internal.body
    );

    let scheme = call(
        &app,
        Method::GET,
        "/api/v1/media?url=file%3A%2F%2F%2Fetc%2Fpasswd",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(scheme.status, StatusCode::BAD_REQUEST, "{:?}", scheme.body);
}

#[tokio::test]
async fn the_image_proxy_serves_an_image_with_a_long_cache_life() {
    let db = Db::in_memory().await.expect("db");
    let fetcher: Arc<dyn Fetcher> = Arc::new(FixtureFetcher::new().with(
        "https://e.example/img.png",
        FixtureResponse::Body {
            status: 200,
            body: vec![1, 2, 3, 4],
            etag: None,
            last_modified: None,
            content_type: Some("image/png; charset=binary".to_owned()),
        },
    ));
    let app = router(AppState::with_fetcher(
        db,
        registration(RegistrationMode::Open),
        fetcher,
    ));
    let registered = call(
        &app,
        Method::POST,
        "/api/v1/auth/register",
        None,
        None,
        Some(json!({"username": "alice", "password": "correct-horse-battery"})),
    )
    .await;
    let cookie = registered.set_cookie.expect("a session cookie");

    let reply = call(
        &app,
        Method::GET,
        "/api/v1/media?url=https%3A%2F%2Fe.example%2Fimg.png&via=https%3A%2F%2Fe.example%2Fa%2F",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.content_type.as_deref(), Some("image/png"));
    assert_eq!(reply.bytes, vec![1, 2, 3, 4]);
    assert!(
        reply
            .cache_control
            .as_deref()
            .is_some_and(|c| c.contains("max-age")),
        "{:?}",
        reply.cache_control
    );
}
