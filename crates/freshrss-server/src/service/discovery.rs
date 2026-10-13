//! Finding the feed behind an address that is not one.
//!
//! The chain itself lives in [`freshrss_feed::discover`], which knows nothing about users or
//! databases. What is left here is the two things only the server knows: which fetch settings a
//! discovery should obey, and what to do when the answer is "there is no feed here".

use freshrss_feed::http::{FetchRequest, Fetcher};
use freshrss_feed::Discovered;

use crate::error::Result;
use crate::service::refresh::Settings;

/// Looks for a feed at `url`, or behind the page it serves.
///
/// # Errors
///
/// Only when `url` itself cannot be fetched. A page with no feed on it is a successful answer with
/// no [`Discovered::found`], because the add-feed dialog has to be able to show *which* candidates
/// were tried and why each was rejected — "not found" on its own is not actionable.
pub async fn discover(
    fetcher: &(dyn Fetcher + '_),
    settings: &Settings,
    url: &str,
) -> Result<Discovered> {
    Ok(freshrss_feed::discover(fetcher, url, &template(settings, url)).await?)
}

/// The request every candidate in one discovery is fetched with.
///
/// Built from the same [`Settings`] a refresh uses, so discovery obeys the operator's user agent,
/// timeout and size cap. A discovery that ignored them would be a way to make the server fetch
/// something it is configured not to.
pub fn template(settings: &Settings, url: &str) -> FetchRequest {
    let mut request = FetchRequest::new(url.to_owned());
    request.user_agent = settings.user_agent.clone();
    request.timeout = settings.timeout;
    request.max_bytes = settings.max_bytes;
    request
}

#[cfg(test)]
mod tests {
    use freshrss_feed::http::{FixtureFetcher, FixtureResponse};

    use super::*;

    const FEED: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://e.example/</link>
<item><title>One</title><link>https://e.example/1</link><guid>g1</guid></item>
</channel></rss>"#;

    const PAGE: &str = r#"<html><head>
<link rel="alternate" type="application/rss+xml" href="/feed.xml">
</head><body><a href="/feed.xml">RSS</a></body></html>"#;

    fn html(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("text/html".to_owned()),
        }
    }

    fn xml(body: &str) -> FixtureResponse {
        FixtureResponse::Body {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("application/rss+xml".to_owned()),
        }
    }

    #[tokio::test]
    async fn an_address_that_is_already_a_feed_needs_no_discovery() {
        let fetcher = FixtureFetcher::new().with("https://e.example/feed.xml", xml(FEED));
        let found = discover(&fetcher, &Settings::default(), "https://e.example/feed.xml")
            .await
            .expect("discovered");
        assert!(found.is_feed());
        let candidate = found.found().expect("a feed");
        assert_eq!(candidate.url, "https://e.example/feed.xml");
        assert_eq!(
            candidate.probe.as_ref().expect("probed").title.as_deref(),
            Some("Example")
        );
    }

    #[tokio::test]
    async fn a_website_is_followed_to_its_feed() {
        let fetcher = FixtureFetcher::new()
            .with("https://e.example/", html(PAGE))
            .with("https://e.example/feed.xml", xml(FEED));
        let found = discover(&fetcher, &Settings::default(), "https://e.example/")
            .await
            .expect("discovered");
        assert!(!found.is_feed(), "the address given was not itself a feed");
        assert_eq!(
            found.found().expect("a feed").url,
            "https://e.example/feed.xml"
        );
    }

    #[tokio::test]
    async fn a_page_with_no_feed_on_it_is_an_answer_not_an_error() {
        let fetcher = FixtureFetcher::new().with(
            "https://e.example/",
            html("<html><body><p>nothing here</p></body></html>"),
        );
        let found = discover(&fetcher, &Settings::default(), "https://e.example/")
            .await
            .expect("discovered");
        assert!(found.found().is_none());
        // The address given is reported as a tried-and-rejected candidate, so the dialog can say
        // why instead of only "nothing found".
        assert_eq!(found.candidates.len(), 1);
        assert!(
            found.candidates[0]
                .rejected
                .as_deref()
                .is_some_and(|r| r.starts_with("the response is not a feed")),
            "{:?}",
            found.candidates[0].rejected
        );
    }

    #[tokio::test]
    async fn a_bridge_that_answers_with_its_own_error_says_so_in_the_rejection() {
        // 200 OK, an XML content type, and a body that is a message rather than a feed: exactly what
        // a broken feed bridge returns, and exactly what must not be reported as a bare "no feed".
        let fetcher = FixtureFetcher::new().with("https://e.example/?type=x", xml("无法获取内容"));
        let found = discover(&fetcher, &Settings::default(), "https://e.example/?type=x")
            .await
            .expect("discovered");
        let rejected = found.candidates[0].rejected.as_deref().expect("a reason");
        assert!(rejected.contains("无法获取内容"), "{rejected}");
    }

    #[tokio::test]
    async fn a_long_page_is_not_quoted_back_at_the_reader() {
        let fetcher = FixtureFetcher::new().with(
            "https://e.example/",
            html(&format!(
                "<html><body><p>{}</p></body></html>",
                "word ".repeat(200)
            )),
        );
        let found = discover(&fetcher, &Settings::default(), "https://e.example/")
            .await
            .expect("discovered");
        assert_eq!(
            found.candidates[0].rejected.as_deref(),
            Some("the response is not a feed")
        );
    }

    #[tokio::test]
    async fn the_settings_reach_the_request() {
        let settings = Settings {
            user_agent: "test-agent/1.0".to_owned(),
            max_bytes: 1234,
            ..Settings::default()
        };
        let request = template(&settings, "https://e.example/feed.xml");
        assert_eq!(request.user_agent, "test-agent/1.0");
        assert_eq!(request.max_bytes, 1234);
        assert_eq!(request.url, "https://e.example/feed.xml");
    }

    #[tokio::test]
    async fn a_wechat_address_is_answered_without_being_fetched() {
        // No fixture is registered for the address: a fetch would fail loudly, so a clean answer
        // proves the bridge guidance short-circuits before any request.
        let fetcher = FixtureFetcher::new();
        let found = discover(
            &fetcher,
            &Settings::default(),
            "https://mp.weixin.qq.com/mp/profile_ext?action=home&__biz=MzExample==",
        )
        .await
        .expect("answered without fetching");
        assert!(found.found().is_none());
        let rejected = found.candidates[0].rejected.as_deref().expect("a reason");
        assert!(rejected.contains("we-mp-rss"), "{rejected}");
    }
}
