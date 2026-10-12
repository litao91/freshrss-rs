//! CLI subcommands that are development and operations tools rather than the server itself.
//!
//! [`parse`] is the one the plan calls for explicitly: it runs a URL or a file through the whole
//! feed pipeline — fetch, parse, GUID decision, sanitize, hash — and prints the result, so a parser
//! change can be eyeballed against a real feed without standing up a database, registering a user
//! and waiting for the scheduler.

use std::io::{Read as _, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use clap::Args;
use freshrss_feed::guid::{decide_guid, GuidInput, UnicityCriteria};
use freshrss_feed::hash::{
    authors_field, content_hash_hex, enclosures_field, original_content, tags_field, HashInput,
};
use freshrss_feed::http::{
    FetchOutcome, FetchRequest, Fetcher as _, ReqwestFetcher, DEFAULT_USER_AGENT,
};
use freshrss_feed::raw::RawItem;
use freshrss_feed::sanitize::{sanitize_html, SanitizeOptions};
use freshrss_feed::{parse_feed, FeedKind, RawFeed};
use serde::Serialize;

/// Arguments for `freshrss-rs parse <url|file>`.
#[derive(Debug, Args)]
pub struct ParseArgs {
    /// A feed URL, or a path to a local file.
    ///
    /// A path is used when it exists on disk; anything else is treated as a URL. That ordering
    /// means `parse ./feed.xml` works from any directory without a `file://` scheme, which the
    /// fetcher would refuse.
    pub source: String,

    /// Force a feed kind instead of detecting it. See `--list-kinds`.
    #[arg(long, value_name = "N")]
    pub kind: Option<i16>,

    /// Print the machine-readable JSON form instead of the human summary.
    #[arg(long)]
    pub json: bool,

    /// Article unicity policy, as stored in a feed's `unicityCriteria` attribute.
    #[arg(long, value_name = "POLICY", default_value = "id")]
    pub unicity: String,

    /// Base URL for resolving relative links and sanitizing. Defaults to the source URL.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// Request timeout in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    pub timeout: u64,

    /// Also print each article's sanitized body.
    #[arg(long)]
    pub show_content: bool,

    /// Sanitize with a byte budget, exercising the truncate-and-retry loop.
    #[arg(long, value_name = "BYTES")]
    pub max_length: Option<usize>,

    /// Print the feed kinds this build supports and exit.
    #[arg(long)]
    pub list_kinds: bool,

    /// HTTP Basic auth credentials as `user:password`, for a protected feed.
    #[arg(long, value_name = "USER:PASS", hide_env_values = true)]
    pub http_auth: Option<String>,

    /// Do not verify the TLS certificate. For a self-signed internal feed only.
    #[arg(long)]
    pub insecure: bool,

    /// Read the body from stdin instead of `source`.
    #[arg(long)]
    pub stdin: bool,
}

/// The JSON shape `--json` emits.
///
/// Deliberately flat and string-typed for ids and dates, so it can be piped into `jq` and diffed
/// between runs of the same fixture without a schema.
#[derive(Debug, Serialize)]
struct FeedReport {
    source: String,
    format: String,
    kind: i16,
    unicity: String,
    title: Option<String>,
    website: Option<String>,
    description: Option<String>,
    icon: Option<String>,
    self_url: Option<String>,
    hub_url: Option<String>,
    base_url: Option<String>,
    effective_url: Option<String>,
    body_hash: Option<String>,
    status: Option<u16>,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    not_modified: bool,
    item_count: usize,
    items: Vec<ItemReport>,
}

#[derive(Debug, Serialize)]
struct ItemReport {
    index: usize,
    guid: String,
    guid_from_feed: Option<String>,
    guid_is_permalink: bool,
    title: Option<String>,
    link: Option<String>,
    date: Option<i64>,
    date_readable: Option<String>,
    updated: Option<i64>,
    authors: Vec<String>,
    tags: Vec<String>,
    hash: String,
    content_length: usize,
    sanitized_length: usize,
    enclosure_count: usize,
    enclosures: Vec<EnclosureReport>,
    thumbnail: Option<String>,
    content: Option<String>,
}

#[derive(Debug, Serialize)]
struct EnclosureReport {
    url: String,
    mime: Option<String>,
    medium: Option<String>,
    length: Option<i64>,
}

/// Runs the `parse` subcommand.
pub async fn run(args: ParseArgs) -> Result<()> {
    if args.list_kinds {
        print_kinds();
        return Ok(());
    }

    let kind = match args.kind {
        Some(code) => FeedKind::from_i16(code),
        None => FeedKind::Rss,
    };

    let (body, meta) = read_source(&args, kind).await?;
    let feed = parse_feed(&body, kind).with_context(|| format!("parsing {}", args.source))?;

    // The feed's own `xml:base` outranks both an explicit flag and the fetch URL: it is what the
    // document says its relative links resolve against, and Atom says so explicitly.
    let base_url = feed
        .base_url
        .clone()
        .or_else(|| args.base_url.clone())
        .or_else(|| meta.effective_url.clone())
        .unwrap_or_default();
    let criteria = UnicityCriteria::parse(&args.unicity);
    let sanitize_options = SanitizeOptions {
        base_url: base_url.clone(),
        force_https_domains: Vec::new(),
        max_length: args.max_length,
    };

    let report = build_report(
        &args,
        &feed,
        kind,
        criteria,
        &sanitize_options,
        &meta,
        Some(&body),
    );

    let mut out = std::io::stdout().lock();
    if args.json {
        serde_json::to_writer_pretty(&mut out, &report).context("writing JSON")?;
        writeln!(out)?;
    } else {
        write_human(&mut out, &report, args.show_content)?;
    }
    Ok(())
}

/// Where the body came from, and what the transport reported.
#[derive(Debug, Default)]
struct SourceMeta {
    status: Option<u16>,
    effective_url: Option<String>,
    body_hash: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    not_modified: bool,
}

/// Loads the body from a file, stdin, or the network.
async fn read_source(args: &ParseArgs, kind: FeedKind) -> Result<(Vec<u8>, SourceMeta)> {
    if args.stdin {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("reading stdin")?;
        return Ok((buf, SourceMeta::default()));
    }

    let path = PathBuf::from(&args.source);
    if path.is_file() {
        let body = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        return Ok((body, SourceMeta::default()));
    }

    // Not a file on disk, so it has to be a URL.
    let fetcher = ReqwestFetcher::new().context("building the HTTP client")?;
    let mut request = FetchRequest::new(args.source.clone()).with_kind(kind);
    request.timeout = Duration::from_secs(args.timeout);
    request.ssl_verify = !args.insecure;
    request.user_agent = DEFAULT_USER_AGENT.to_owned();
    if let Some(creds) = &args.http_auth {
        let (user, pass) = creds.split_once(':').unwrap_or((creds, ""));
        request.basic_auth = Some((user.to_owned(), pass.to_owned()));
    }

    match fetcher.fetch(&request).await {
        Ok(FetchOutcome::Found(found)) => Ok((
            found.body.clone(),
            SourceMeta {
                status: Some(found.status),
                effective_url: Some(found.effective_url.clone()),
                body_hash: Some(found.hash.clone()),
                etag: found.etag,
                last_modified: found.last_modified,
                content_type: found.content_type,
                not_modified: false,
            },
        )),
        Ok(FetchOutcome::NotModified) => {
            // There is nothing to parse, but saying so plainly is more useful than an error: it
            // confirms the conditional-GET path works.
            bail!("the server answered 304 Not Modified, so there is no body to parse")
        }
        Err(err) => Err(err).with_context(|| format!("fetching {}", args.source)),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_report(
    args: &ParseArgs,
    feed: &RawFeed,
    kind: FeedKind,
    criteria: UnicityCriteria,
    options: &SanitizeOptions,
    meta: &SourceMeta,
    body: Option<&[u8]>,
) -> FeedReport {
    FeedReport {
        source: args.source.clone(),
        format: format!("{:?}", feed.format),
        kind: kind.as_i16(),
        unicity: criteria.as_str().to_owned(),
        title: feed.title.clone(),
        website: feed.website.clone(),
        description: feed.description.clone(),
        icon: feed.icon.clone(),
        self_url: feed.self_url.clone(),
        hub_url: feed.hub_url.clone(),
        base_url: feed
            .base_url
            .clone()
            .or_else(|| options.base_url.clone().into()),
        effective_url: meta.effective_url.clone(),
        body_hash: meta
            .body_hash
            .clone()
            .or_else(|| body.map(freshrss_feed::http::body_hash)),
        status: meta.status,
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
        content_type: meta.content_type.clone(),
        not_modified: meta.not_modified,
        item_count: feed.items.len(),
        items: feed
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| item_report(i, item, criteria, options, args.show_content))
            .collect(),
    }
}

fn item_report(
    index: usize,
    item: &RawItem,
    criteria: UnicityCriteria,
    options: &SanitizeOptions,
    show_content: bool,
) -> ItemReport {
    // The body is sanitized *before* the GUID is decided, because that is the order FreshRSS uses
    // and a `sha1:*content*` policy has to see the same text both times or the GUID moves.
    let raw_content = item.effective_content().unwrap_or_default();
    let sanitized = sanitize_html(raw_content, options);
    let content_for_guid = (!sanitized.is_empty()).then_some(sanitized.as_str());

    let guid = decide_guid(
        &GuidInput::from_item(item, content_for_guid),
        criteria,
        true,
    );

    let hash = content_hash_hex(&HashInput {
        link: item.link.as_deref().unwrap_or_default(),
        title: item.title.as_deref().unwrap_or_default(),
        author: &authors_field(&item.authors),
        content: &original_content(&sanitized, None),
        tags: &tags_field(&item.tags),
        enclosures: &enclosures_field(&item.enclosures),
    });

    ItemReport {
        index,
        guid,
        guid_from_feed: item.guid.clone(),
        guid_is_permalink: item.guid_is_permalink,
        title: item.title.clone(),
        link: item.link.clone(),
        date: item.date,
        date_readable: item.date.map(freshrss_feed::parse::date::to_rfc3339),
        updated: item.updated,
        authors: item.authors.clone(),
        tags: item.tags.clone(),
        hash,
        content_length: raw_content.len(),
        sanitized_length: sanitized.len(),
        enclosure_count: item.enclosures.len(),
        enclosures: item
            .enclosures
            .iter()
            .map(|e| EnclosureReport {
                url: e.url.clone(),
                mime: e.mime.clone(),
                medium: e.medium.clone(),
                length: e.length,
            })
            .collect(),
        thumbnail: item.thumbnail.as_ref().map(|t| t.url.clone()),
        content: show_content.then_some(sanitized),
    }
}

fn write_human(out: &mut impl Write, report: &FeedReport, show_content: bool) -> Result<()> {
    writeln!(out, "source        {}", report.source)?;
    writeln!(out, "format        {}", report.format)?;
    if let Some(status) = report.status {
        writeln!(out, "status        {status}")?;
    }
    if let Some(hash) = &report.body_hash {
        writeln!(out, "body hash     {hash}")?;
    }
    // The conditional-GET validators, printed so it is visible what the next fetch will replay.
    if let Some(etag) = &report.etag {
        writeln!(out, "etag          {etag}")?;
    }
    if let Some(last_modified) = &report.last_modified {
        writeln!(out, "last-modified {last_modified}")?;
    }
    if let Some(content_type) = &report.content_type {
        writeln!(out, "content-type  {content_type}")?;
    }
    writeln!(
        out,
        "title         {}",
        report.title.as_deref().unwrap_or("-")
    )?;
    writeln!(
        out,
        "website       {}",
        report.website.as_deref().unwrap_or("-")
    )?;
    writeln!(
        out,
        "icon          {}",
        report.icon.as_deref().unwrap_or("-")
    )?;
    writeln!(
        out,
        "self          {}",
        report.self_url.as_deref().unwrap_or("-")
    )?;
    writeln!(out, "unicity       {}", report.unicity)?;
    writeln!(out, "items         {}", report.item_count)?;

    for item in &report.items {
        writeln!(out)?;
        writeln!(
            out,
            "[{}] {}",
            item.index,
            item.title.as_deref().unwrap_or("(no title)")
        )?;
        writeln!(out, "    guid    {}", item.guid)?;
        if item.guid_from_feed.as_deref() != Some(item.guid.as_str()) {
            writeln!(
                out,
                "            (feed supplied {:?}; permalink={})",
                item.guid_from_feed, item.guid_is_permalink
            )?;
        }
        writeln!(out, "    link    {}", item.link.as_deref().unwrap_or("-"))?;
        writeln!(
            out,
            "    date    {}",
            item.date_readable.as_deref().unwrap_or("-")
        )?;
        writeln!(out, "    hash    {}", item.hash)?;
        if !item.authors.is_empty() {
            writeln!(out, "    authors {}", item.authors.join("; "))?;
        }
        if !item.tags.is_empty() {
            writeln!(out, "    tags    {}", item.tags.join(", "))?;
        }
        writeln!(
            out,
            "    content {} bytes -> {} sanitized",
            item.content_length, item.sanitized_length
        )?;
        for e in &item.enclosures {
            writeln!(
                out,
                "    attach  {} [{} {}]",
                e.url,
                e.mime.as_deref().unwrap_or("?"),
                e.medium.as_deref().unwrap_or("?")
            )?;
        }
        if let Some(t) = &item.thumbnail {
            writeln!(out, "    thumb   {t}")?;
        }
        if show_content {
            if let Some(body) = &item.content {
                writeln!(out, "    ---")?;
                for line in body.lines() {
                    writeln!(out, "    {line}")?;
                }
                writeln!(out, "    ---")?;
            }
        }
    }
    Ok(())
}

fn print_kinds() {
    println!("code  name                        interpretation");
    for kind in FeedKind::ALL {
        println!(
            "{:>4}  {:<27} {}",
            kind.as_i16(),
            format!("{kind:?}"),
            describe_kind(kind)
        );
    }
}

fn describe_kind(kind: FeedKind) -> &'static str {
    match kind {
        FeedKind::Rss => "RSS, Atom or JSON Feed, detected from the bytes",
        FeedKind::RssForced => "RSS/Atom, parsed leniently after a strict failure",
        FeedKind::HtmlXpath => "HTML page scraped with XPath",
        FeedKind::XmlXpath => "XML document scraped with XPath",
        FeedKind::JsonXpath => "JSON document scraped with XPath",
        FeedKind::JsonFeed => "JSON Feed, read through the standard dot-notation map",
        FeedKind::JsonDotNotation => "JSON document scraped with a.b[0].c paths",
        FeedKind::HtmlXpathJsonDotNotation => "JSON embedded in an HTML page",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>Test Feed</title>
    <link>https://example.com</link>
    <item>
      <title>First</title>
      <link>https://example.com/1</link>
      <guid>tag:example,1</guid>
      <pubDate>Mon, 02 Jan 2006 15:04:05 +0000</pubDate>
      <content:encoded><![CDATA[<p>Hello <script>alert(1)</script></p>]]></content:encoded>
      <description>summary</description>
    </item>
  </channel>
</rss>"#;

    fn report_for(body: &str, kind: FeedKind, unicity: &str) -> FeedReport {
        let feed = parse_feed(body.as_bytes(), kind).unwrap();
        let options = SanitizeOptions {
            base_url: "https://example.com/feed.xml".to_owned(),
            force_https_domains: Vec::new(),
            max_length: None,
        };
        build_report(
            &ParseArgs {
                source: "test".to_owned(),
                kind: None,
                json: false,
                unicity: unicity.to_owned(),
                base_url: None,
                timeout: 30,
                show_content: false,
                max_length: None,
                http_auth: None,
                insecure: false,
                stdin: false,
                list_kinds: false,
            },
            &feed,
            kind,
            UnicityCriteria::parse(unicity),
            &options,
            &SourceMeta::default(),
            Some(body.as_bytes()),
        )
    }

    #[test]
    fn the_report_carries_the_feed_metadata() {
        let report = report_for(RSS, FeedKind::Rss, "id");
        assert_eq!(report.title.as_deref(), Some("Test Feed"));
        assert_eq!(report.format, "Rss2");
        assert_eq!(report.item_count, 1);
        assert_eq!(report.unicity, "id");
        assert!(report.body_hash.is_some_and(|h| h.len() == 40));
    }

    #[test]
    fn the_report_shows_the_decided_guid_and_its_provenance() {
        let report = report_for(RSS, FeedKind::Rss, "id");
        let item = &report.items[0];
        assert_eq!(item.guid, "tag:example,1");
        assert_eq!(item.guid_from_feed.as_deref(), Some("tag:example,1"));

        // Under a hash policy the decided GUID differs from what the feed supplied, and the report
        // has to say so or the difference is invisible.
        let report = report_for(RSS, FeedKind::Rss, "sha1:link_published");
        let item = &report.items[0];
        assert_ne!(item.guid, "tag:example,1");
        assert_eq!(item.guid.len(), 40, "a sha1 policy yields a hex digest");
        assert_eq!(item.guid_from_feed.as_deref(), Some("tag:example,1"));
    }

    #[test]
    fn the_report_includes_the_change_detection_hash() {
        let report = report_for(RSS, FeedKind::Rss, "id");
        assert_eq!(report.items[0].hash.len(), 32, "an MD5 hex digest");
    }

    #[test]
    fn the_report_records_that_the_body_was_sanitized() {
        let report = report_for(RSS, FeedKind::Rss, "id");
        let item = &report.items[0];
        assert!(item.content_length > 0);
        // The `<script>` is gone, so the sanitized body is shorter than the raw one.
        assert!(
            item.sanitized_length < item.content_length,
            "raw {} -> sanitized {}",
            item.content_length,
            item.sanitized_length
        );
    }

    #[test]
    fn show_content_includes_the_sanitized_body() {
        let feed = parse_feed(RSS.as_bytes(), FeedKind::Rss).unwrap();
        let options = SanitizeOptions {
            base_url: "https://example.com/feed.xml".to_owned(),
            ..SanitizeOptions::default()
        };
        let report = build_report(
            &ParseArgs {
                source: "test".to_owned(),
                kind: None,
                json: false,
                unicity: "id".to_owned(),
                base_url: None,
                timeout: 30,
                show_content: true,
                max_length: None,
                http_auth: None,
                insecure: false,
                stdin: false,
                list_kinds: false,
            },
            &feed,
            FeedKind::Rss,
            UnicityCriteria::Id,
            &options,
            &SourceMeta::default(),
            None,
        );
        let body = report.items[0].content.clone().unwrap();
        assert!(body.contains("<p>Hello"), "{body}");
        assert!(!body.contains("script"), "{body}");
    }

    #[test]
    fn the_human_output_names_every_field_it_prints() {
        let report = report_for(RSS, FeedKind::Rss, "id");
        let mut buf = Vec::new();
        write_human(&mut buf, &report, false).unwrap();
        let text = String::from_utf8(buf).unwrap();
        for expected in [
            "source", "format", "title", "guid", "link", "date", "hash", "content",
        ] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
        assert!(text.contains("Test Feed"), "{text}");
        assert!(text.contains("First"), "{text}");
    }

    #[test]
    fn the_json_output_round_trips() {
        let report = report_for(RSS, FeedKind::Rss, "id");
        let json = serde_json::to_string(&report).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["title"], "Test Feed");
        assert_eq!(back["item_count"], 1);
        assert_eq!(back["items"][0]["title"], "First");
        assert_eq!(back["items"][0]["guid"], "tag:example,1");
    }

    #[test]
    fn a_relative_link_is_resolved_in_the_sanitized_body() {
        let body = r#"<rss><channel><title>T</title>
          <item><title>x</title><description>&lt;img src="/a.png"&gt;</description></item>
        </channel></rss>"#;
        let report = report_for(body, FeedKind::Rss, "id");
        assert!(report.items[0].sanitized_length > 0);
    }

    #[test]
    fn kinds_are_all_described() {
        for kind in FeedKind::ALL {
            assert!(!describe_kind(kind).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn an_empty_feed_reports_zero_items_without_failing() {
        let body = r#"<rss><channel><title>T</title></channel></rss>"#;
        let report = report_for(body, FeedKind::Rss, "id");
        assert_eq!(report.item_count, 0);
        assert!(report.items.is_empty());
    }
}
