//! Proxy environment variables must reach the wire.
//!
//! reqwest reads `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` itself when a client is
//! built, so there is no proxy plumbing of our own to test — what these tests pin down is that the
//! feature set we compile reqwest with actually *drives* each kind of proxy (a SOCKS proxy without
//! the `socks` feature is silently misdriven as an HTTP one), and that a fetch really goes through
//! the proxy rather than direct.
//!
//! Env vars are process-global, so everything runs sequentially in one test, in this dedicated
//! binary: the unit tests build clients too, and must not see these variables.

use std::time::Duration;

use freshrss_feed::http::{FetchOutcome, FetchRequest, Fetcher as _, ReqwestFetcher};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const BODY: &str =
    "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>P</title></channel></rss>";

/// Sets (or clears) the proxy variables in both cases, so nothing leaks in from the environment
/// `cargo test` was started with.
fn proxy_env(all: Option<&str>, no: Option<&str>) {
    for (name, value) in [
        ("ALL_PROXY", all),
        ("HTTP_PROXY", None),
        ("HTTPS_PROXY", None),
        ("NO_PROXY", no),
    ] {
        for key in [name.to_owned(), name.to_ascii_lowercase()] {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
    }
}

async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    let addr = listener.local_addr().expect("an address");
    (listener, addr.to_string())
}

fn raw_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn request(url: &str) -> FetchRequest {
    FetchRequest {
        timeout: Duration::from_secs(5),
        ..FetchRequest::new(url)
    }
}

fn body_of(outcome: FetchOutcome) -> Vec<u8> {
    match outcome {
        FetchOutcome::Found(f) => f.body,
        FetchOutcome::NotModified => panic!("expected a body"),
    }
}

#[tokio::test]
async fn the_proxy_environment_variables_are_honoured() {
    // --- ALL_PROXY as an HTTP proxy: the request arrives in absolute form -------------------
    let (proxy, addr) = listener().await;
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = proxy.accept().await.expect("a connection");
        let mut buf = vec![0u8; 4096];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..read]).into_owned();
        socket.write_all(raw_ok(BODY).as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
        tx.send(head).ok();
    });
    proxy_env(Some(&format!("http://{addr}")), None);

    let fetcher = ReqwestFetcher::new().expect("a client");
    let outcome = fetcher
        .fetch(&request("http://example.com/feed.xml"))
        .await
        .expect("fetched through the proxy");
    assert_eq!(body_of(outcome), BODY.as_bytes());
    let head = rx.await.expect("the proxy reported");
    assert!(
        head.starts_with("GET http://example.com/feed.xml"),
        "an HTTP proxy is asked in absolute form, which is how we know the fetch did not go \
         direct: {head:?}"
    );

    // --- ALL_PROXY as SOCKS5: the handshake names the target, the body arrives tunneled -----
    let (proxy, addr) = listener().await;
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = proxy.accept().await.expect("a connection");
        // Greeting: version, method count, methods. Answer "no authentication".
        let mut greeting = [0u8; 2];
        socket.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5, "SOCKS5");
        let mut methods = vec![0u8; greeting[1] as usize];
        socket.read_exact(&mut methods).await.unwrap();
        socket.write_all(&[5, 0]).await.unwrap();
        // CONNECT request. `socks5h` sends the hostname for the proxy to resolve, which is what
        // keeps this test off the network: nothing here resolves example.com locally.
        let mut head = [0u8; 4];
        socket.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..3], &[5, 1, 0], "CONNECT");
        assert_eq!(head[3], 3, "a domain address");
        let mut len = [0u8; 1];
        socket.read_exact(&mut len).await.unwrap();
        let mut domain = vec![0u8; len[0] as usize];
        socket.read_exact(&mut domain).await.unwrap();
        let mut port = [0u8; 2];
        socket.read_exact(&mut port).await.unwrap();
        let target = format!(
            "{}:{}",
            String::from_utf8(domain).unwrap(),
            u16::from_be_bytes(port)
        );
        socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
        // The tunnel is up: the real request arrives in origin form.
        let mut buf = vec![0u8; 4096];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        let tunneled = String::from_utf8_lossy(&buf[..read]).into_owned();
        socket.write_all(raw_ok(BODY).as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
        tx.send((target, tunneled)).ok();
    });
    proxy_env(Some(&format!("socks5h://{addr}")), None);

    let fetcher = ReqwestFetcher::new().expect("a client");
    let outcome = fetcher
        .fetch(&request("http://example.com/feed.xml"))
        .await
        .expect("fetched through the SOCKS proxy");
    assert_eq!(body_of(outcome), BODY.as_bytes());
    let (target, tunneled) = rx.await.expect("the proxy reported");
    assert_eq!(target, "example.com:80");
    assert!(
        tunneled.starts_with("GET /feed.xml"),
        "through a tunnel the request is origin-form: {tunneled:?}"
    );

    // --- NO_PROXY: a listed destination goes direct even with a proxy set -------------------
    let (origin, addr) = listener().await;
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.expect("a connection");
        let mut buf = vec![0u8; 4096];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..read]).into_owned();
        socket.write_all(raw_ok(BODY).as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
        tx.send(head).ok();
    });
    // Nothing listens on this port: if the fetch tried the proxy it would fail, not succeed.
    proxy_env(Some("http://127.0.0.1:1"), Some("127.0.0.1"));

    let fetcher = ReqwestFetcher::new().expect("a client");
    let outcome = fetcher
        .fetch(&request(&format!("http://{addr}/feed.xml")))
        .await
        .expect("NO_PROXY destination fetched direct");
    assert_eq!(body_of(outcome), BODY.as_bytes());
    let head = rx.await.expect("the origin reported");
    assert!(
        head.starts_with("GET /feed.xml"),
        "a direct request is origin-form: {head:?}"
    );

    proxy_env(None, None);
}