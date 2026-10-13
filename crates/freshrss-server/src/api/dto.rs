//! API data transfer objects.
//!
//! These are separate from the domain types on purpose, and the rule is one-directional: DTOs are
//! built *from* domain values via `From`, never the reverse. Two things follow from that, and both
//! are the reason this module exists:
//!
//! * **The password hash has no route to a response.** `domain::user::User` does not implement
//!   `Serialize`, so forgetting a `#[serde(skip)]` cannot leak it — the field simply has no DTO
//!   counterpart.
//! * **DTOs carry plaintext.** Nothing here HTML-escapes. Text is stored clean and Svelte escapes
//!   at render; escaping on the way out is how FreshRSS ends up with double-encoded titles.

use freshrss_feed::{Candidate, Discovered, FeedFormat, Source};
use serde::{Deserialize, Serialize};

use crate::domain::category::{Category, CategoryKind};
use crate::domain::feed::{Feed, LetterAvatar};
use crate::domain::ids::EntryId;
use crate::domain::user::{SortOrder, User, UserConfig};
use crate::service::jobs::{Job, JobState};
use crate::service::refresh::run::FeedFailure;
use crate::service::refresh::{Outcome, Status};

#[derive(Debug, Clone, Serialize)]
pub struct UserDto {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub created_at: i64,
    /// The letter for `.rd-avatar` and `.rd-favicon--letter`.
    pub initial: String,
}

impl From<&User> for UserDto {
    fn from(user: &User) -> Self {
        Self {
            id: user.id,
            username: user.username.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
            created_at: user.created_at,
            initial: user.initial(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserConfigDto {
    pub ttl_default: i64,
    pub sort: String,
    pub sort_order: SortOrder,
    pub secondary_sort: String,
    pub secondary_sort_order: SortOrder,
    pub posts_per_page: u32,
    pub language: String,
    pub archiving: crate::config::ArchivingOptions,
    pub mark_when: crate::config::MarkWhen,
}

impl From<&UserConfig> for UserConfigDto {
    fn from(cfg: &UserConfig) -> Self {
        Self {
            ttl_default: cfg.ttl_default,
            sort: cfg.sort.clone(),
            sort_order: cfg.sort_order,
            secondary_sort: cfg.secondary_sort.clone(),
            secondary_sort_order: cfg.secondary_sort_order,
            posts_per_page: cfg.posts_per_page,
            language: cfg.language.clone(),
            archiving: cfg.archiving.clone(),
            mark_when: cfg.mark_when,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionDto {
    pub id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// True for the session making this request, so the UI can label it "this device".
    pub current: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FaviconDto {
    /// The feed's own icon, when one was discovered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Fallback for `.rd-favicon--letter`.
    pub letter: String,
    pub color: String,
}

impl FaviconDto {
    pub fn for_feed(feed: &Feed, icon_endpoint: impl Fn(i64) -> String) -> Self {
        let avatar = LetterAvatar::for_name(feed.display_name());
        Self {
            // Only point at the icon endpoint once an icon is actually known; a feed with no
            // `feedIconUrl` would otherwise issue a request that can only 404.
            url: feed.feed_icon_url().map(|_| icon_endpoint(feed.id)),
            letter: avatar.letter,
            color: avatar.color,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedDto {
    /// Serialized as a string for consistency with entry ids, so the frontend has one rule.
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub url: String,
    pub website: String,
    pub description: String,
    pub kind: i16,
    pub priority: i8,
    pub ttl: i32,
    pub muted: bool,
    pub category: Option<i64>,
    /// 0 when healthy, otherwise the unix time of the last failed refresh.
    pub error: i64,
    pub last_update: i64,
    pub unread: i64,
    pub total: i64,
    pub favicon: FaviconDto,
}

impl FeedDto {
    pub fn new(feed: &Feed) -> Self {
        Self {
            id: feed.id,
            name: feed.display_name().to_string(),
            // Never expose stored credentials, including any embedded in the URL.
            url: feed.url_without_credentials(),
            website: feed.website.clone(),
            description: feed.description.clone(),
            kind: feed.kind.as_i16(),
            priority: feed.priority.get(),
            ttl: feed.ttl.seconds(),
            muted: feed.is_muted(),
            category: feed.category,
            error: feed.error,
            last_update: feed.last_update,
            unread: feed.cache_nb_unreads,
            total: feed.cache_nb_entries,
            favicon: FaviconDto::for_feed(feed, |id| format!("/api/v1/feeds/{id}/icon")),
        }
    }
}

/// The answer to `POST /feeds`: the subscription, and what its first fetch pulled in.
///
/// `new_entries` is what lets the add dialog say "12 new articles" rather than just "added" — and
/// what makes a feed that published nothing visible as such, which is otherwise indistinguishable
/// from one that failed.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedFeedDto {
    pub feed: FeedDto,
    pub new_entries: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub kind: i16,
    pub position: i64,
    pub error: i64,
    pub last_update: i64,
    pub unread: i64,
    pub total: i64,
    pub is_dynamic_opml: bool,
    pub feeds: Vec<FeedDto>,
}

impl CategoryDto {
    pub fn new(category: &Category, feeds: Vec<Feed>) -> Self {
        let unread = feeds.iter().map(|f| f.cache_nb_unreads).sum();
        let total = feeds.iter().map(|f| f.cache_nb_entries).sum();
        Self {
            id: category.id,
            name: category.name.clone(),
            kind: category.kind.as_i16(),
            position: category.position,
            error: category.error,
            last_update: category.last_update,
            unread,
            total,
            is_dynamic_opml: category.kind == CategoryKind::DynamicOpml,
            feeds: feeds.iter().map(FeedDto::new).collect(),
        }
    }
}

/// One row of the sidebar's label list.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LabelCountDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub unread: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct ScopeCounts {
    pub unread: i64,
    pub total: i64,
}

/// The numbers behind "All articles", "Starred" and the `.rd-segment-count` badge.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct CountsDto {
    /// Everything the user owns.
    pub all: ScopeCounts,
    /// Feeds at main-stream priority or above — the default "All articles" scope.
    pub main_stream: ScopeCounts,
    pub important: ScopeCounts,
    pub starred: ScopeCounts,
    pub labels: Vec<LabelCountDto>,
}

/// The whole sidebar in one response.
///
/// Deliberately denormalized: building it from per-feed count queries is the N+1 that FreshRSS's
/// `cache_nbUnreads` column exists to avoid, and the reader requests this on every navigation.
#[derive(Debug, Clone, Serialize)]
pub struct TreeDto {
    pub categories: Vec<CategoryDto>,
    /// Feeds whose folder was deleted (`feed.category` is `ON DELETE SET NULL`). They still need to
    /// be reachable, or deleting a folder would hide subscriptions without removing them.
    pub orphans: Vec<FeedDto>,
    pub counts: CountsDto,
    pub problems: ProblemCountDto,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct ProblemCountDto {
    pub count: i64,
}

/// Everything the SPA needs to render without a waterfall of follow-up requests.
#[derive(Debug, Clone, Serialize)]
pub struct BootstrapDto {
    pub user: UserDto,
    pub config: UserConfigDto,
    pub tree: TreeDto,
    pub version: VersionDto,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionDto {
    pub freshrss_rs: String,
    pub schema: i64,
}

/// One URL the add-feed dialog considered.
///
/// Rejected candidates are kept rather than dropped: "we tried nine links on that page and none was
/// a feed" is a far more useful answer than "not found", and it is the only way the user can spot
/// that the feed they wanted was the one that answered 403.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateDto {
    pub url: String,
    /// Which pass found it, so the dialog can rank a `<link rel=alternate>` above a URL that merely
    /// happened to contain the word "rss".
    pub source: Source,
    /// True when the address was fetched and parsed as a feed.
    pub verified: bool,
    pub title: Option<String>,
    pub website: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub item_count: Option<usize>,
    pub format: Option<FeedFormat>,
    /// Why it was not accepted, when it was not.
    pub rejected: Option<String>,
}

impl From<&Candidate> for CandidateDto {
    fn from(candidate: &Candidate) -> Self {
        let probe = candidate.probe.as_ref();
        Self {
            url: candidate.url.clone(),
            source: candidate.source,
            verified: probe.is_some(),
            title: probe.and_then(|p| p.title.clone()),
            website: probe.and_then(|p| p.website.clone()),
            description: probe.and_then(|p| p.description.clone()),
            icon: probe.and_then(|p| p.icon.clone()),
            item_count: probe.map(|p| p.item_count),
            format: probe.map(|p| p.format),
            rejected: candidate.rejected.clone(),
        }
    }
}

/// The whole answer to "is there a feed behind this address".
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredDto {
    /// True when the address given is itself a feed, so the dialog can skip the candidate list and
    /// subscribe straight away.
    pub is_feed: bool,
    /// The address that was fetched, after redirects.
    pub url: String,
    pub candidates: Vec<CandidateDto>,
}

impl From<&Discovered> for DiscoveredDto {
    fn from(found: &Discovered) -> Self {
        Self {
            is_feed: found.is_feed(),
            url: found.url.clone(),
            candidates: found.candidates.iter().map(CandidateDto::from).collect(),
        }
    }
}

/// One row of `#rd-problems-dialog`.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemFeedDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub url: String,
    /// Unix seconds of the last failed check; 0 when the feed is merely muted.
    pub error: i64,
    pub muted: bool,
    /// Unix seconds of the last successful refresh; 0 when there never was one.
    pub last_update: i64,
    pub unread: i64,
    pub total: i64,
    /// Why the last check failed, in words. The dialog's whole job is to tell the user what to do,
    /// and "410 Gone" and "certificate expired" call for opposite responses.
    pub message: Option<String>,
}

impl From<&Feed> for ProblemFeedDto {
    fn from(feed: &Feed) -> Self {
        Self {
            id: feed.id,
            name: feed.display_name().to_string(),
            url: feed.url_without_credentials(),
            error: feed.error,
            muted: feed.is_muted(),
            last_update: feed.last_update,
            unread: feed.cache_nb_unreads,
            total: feed.cache_nb_entries,
            message: feed.last_error_message().map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProblemsDto {
    pub count: i64,
    pub feeds: Vec<ProblemFeedDto>,
}

/// The outcome of refreshing one feed, for the dialog's inline "retry" result.
#[derive(Debug, Clone, Serialize)]
pub struct FeedRefreshDto {
    pub new_entries: usize,
    pub updated_entries: usize,
    pub unchanged: usize,
    pub status: &'static str,
    /// Present only on a failure, so the dialog can say why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&Outcome> for FeedRefreshDto {
    fn from(outcome: &Outcome) -> Self {
        Self {
            new_entries: outcome.new_entries,
            updated_entries: outcome.updated_entries,
            unchanged: outcome.unchanged,
            status: status_name(&outcome.status),
            message: match &outcome.status {
                Status::Failed { message, .. } => Some(message.clone()),
                _ => None,
            },
        }
    }
}

/// The wire name of a refresh outcome: what the frontend switches on.
fn status_name(status: &Status) -> &'static str {
    match status {
        Status::Fetched => "fetched",
        Status::Unchanged => "unchanged",
        Status::NotModified => "not_modified",
        Status::Muted => "muted",
        Status::Failed { .. } => "failed",
    }
}

/// One feed's failure inside a job report.
#[derive(Debug, Clone, Serialize)]
pub struct JobErrorDto {
    #[serde(serialize_with = "serialize_id")]
    pub feed_id: i64,
    pub name: String,
    pub message: String,
    pub http_status: Option<u16>,
}

impl From<&FeedFailure> for JobErrorDto {
    fn from(failure: &FeedFailure) -> Self {
        Self {
            feed_id: failure.feed_id,
            name: failure.name.clone(),
            message: failure.message.clone(),
            http_status: failure.http_status,
        }
    }
}

/// A job the client is polling.
#[derive(Debug, Clone, Serialize)]
pub struct RefreshJobDto {
    pub state: JobState,
    pub feeds_done: usize,
    pub new_entries: usize,
    pub updated_entries: usize,
    pub errors: Vec<JobErrorDto>,
    /// Unix seconds, or `null` while the job is still running.
    pub finished_at: Option<i64>,
}

impl From<&Job> for RefreshJobDto {
    fn from(job: &Job) -> Self {
        Self {
            state: job.state,
            feeds_done: job.report.feeds_done,
            new_entries: job.report.new_entries,
            updated_entries: job.report.updated_entries,
            errors: job.report.errors.iter().map(JobErrorDto::from).collect(),
            finished_at: job.finished_at,
        }
    }
}

/// What the top-bar spinner needs on load: is a job already running server-side.
#[derive(Debug, Clone, Serialize)]
pub struct RefreshStatusDto {
    pub running: bool,
    /// The job to resume polling, or `null` when idle.
    pub job_id: Option<String>,
    /// Counts from the current or most recent job, so a reload mid-run still has something to show.
    pub feeds_done: usize,
    pub new_entries: usize,
}

impl From<&Job> for RefreshStatusDto {
    fn from(job: &Job) -> Self {
        Self {
            running: job.state == JobState::Running,
            job_id: Some(job.id.clone()),
            feeds_done: job.report.feeds_done,
            new_entries: job.report.new_entries,
        }
    }
}

impl RefreshStatusDto {
    /// The idle answer, for a user who has never asked for a refresh.
    pub fn idle() -> Self {
        Self {
            running: false,
            job_id: None,
            feeds_done: 0,
            new_entries: 0,
        }
    }
}

/// One entry in the list pane.
///
/// `content_html` is deliberately absent: a page of 50 full articles would be megabytes, and the
/// reading pane fetches the single entry it needs.
#[derive(Debug, Clone, Serialize)]
pub struct EntrySummaryDto {
    pub id: EntryId,
    #[serde(serialize_with = "serialize_id")]
    pub feed_id: i64,
    pub feed_name: String,
    #[serde(serialize_with = "serialize_id_opt")]
    pub category_id: Option<i64>,
    pub category_name: Option<String>,
    pub title: String,
    pub link: String,
    pub author: String,
    pub summary: String,
    pub date: i64,
    pub last_modified: Option<i64>,
    pub last_seen: i64,
    pub is_read: bool,
    pub is_favorite: bool,
    pub tags: Vec<String>,
    pub labels: Vec<LabelRefDto>,
    pub thumbnail: Option<String>,
    pub has_enclosures: bool,
    /// True when the article carries audio or video, which is what shows `.rd-item-media`.
    pub has_media: bool,
    /// True when a refresh found this article's content had changed upstream.
    pub is_updated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LabelRefDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
}

/// The full article, for the reading pane.
#[derive(Debug, Clone, Serialize)]
pub struct EntryDetailDto {
    pub entry: EntryFullDto,
    /// Ids of the neighbouring articles in the current listing, so `.rd-read-bar`'s prev/next
    /// buttons need no extra round trip.
    pub prev_id: Option<EntryId>,
    pub next_id: Option<EntryId>,
    pub feed: FeedRefDto,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryFullDto {
    #[serde(flatten)]
    pub summary: EntrySummaryDto,
    /// Sanitized HTML. This is the one value the frontend renders with `{@html}`.
    pub content_html: String,
    pub content_text: String,
    pub guid: String,
    pub enclosures: Vec<crate::domain::entry::Enclosure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedRefDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub website: String,
}

/// A page of entries plus the cursor for the next one.
#[derive(Debug, Clone, Serialize)]
pub struct EntryPageDto {
    pub items: Vec<EntrySummaryDto>,
    /// Opaque; pass back unchanged to fetch the next page. `None` when there is no next page.
    pub cursor: Option<String>,
    pub has_more: bool,
    /// The snapshot fence. Echo it back on subsequent pages so articles arriving mid-scroll do not
    /// appear part-way down the list.
    pub id_max: EntryId,
    pub unread_in_scope: i64,
}

/// Writes an id as a JSON string.
///
/// Entry ids exceed 2^53, the largest integer a JavaScript number represents exactly, so a numeric
/// id would be silently rounded. Feed and category ids are small, but they are strings too so the
/// frontend has one rule rather than two.
fn serialize_id<S: serde::Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

fn serialize_id_opt<S: serde::Serializer>(
    id: &Option<i64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match id {
        Some(v) => serializer.serialize_some(&v.to_string()),
        None => serializer.serialize_none(),
    }
}

/// An id as a client may send it: a JSON string, or a number.
#[derive(Deserialize)]
#[serde(untagged)]
enum IdRepr {
    Number(i64),
    Text(String),
}

/// Reads an optional id that may arrive as a string or as a number.
///
/// Ids go *out* as strings (see [`serialize_id`]), so a client that round-trips one — reads a folder
/// id from `/tree`, sends it back on `POST /feeds` — sends a string. Accepting only a number would
/// make that request fail with a message about types, which reads like a client bug and is not one.
/// Accepting only a string would break a hand-written `curl`. An empty string is `None`, because
/// that is what a cleared `<select>` posts.
pub fn deserialize_id<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match Option::<IdRepr>::deserialize(deserializer)? {
        None => Ok(None),
        Some(IdRepr::Number(id)) => Ok(Some(id)),
        Some(IdRepr::Text(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<i64>()
                .map(Some)
                .map_err(|err| D::Error::custom(format!("{trimmed:?} is not an id: {err}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attributes::{keys, Attributes};
    use crate::domain::kind::FeedKind;
    use crate::domain::priority::Priority;
    use crate::domain::ttl::Ttl;

    fn feed() -> Feed {
        Feed {
            id: 7,
            id_user: 1,
            url: "https://example.com/feed.xml".into(),
            kind: FeedKind::Rss,
            category: Some(3),
            name: "Example".into(),
            website: "https://example.com".into(),
            description: "A feed".into(),
            last_update: 1_700_000_000,
            priority: Priority::MAIN_STREAM,
            path_entries: None,
            path_entries_filter: None,
            http_auth: None,
            error: 0,
            ttl: Ttl::new(3600, false),
            attributes: Attributes::new(),
            cache_nb_entries: 210,
            cache_nb_unreads: 5,
        }
    }

    #[test]
    fn the_password_hash_has_no_field_in_any_dto() {
        let user = User {
            id: 1,
            username: "alice".into(),
            email: Some("a@b.co".into()),
            password_hash: "$argon2id$SECRET".into(),
            is_admin: false,
            enabled: true,
            created_at: 0,
            last_seen_at: None,
            config: UserConfig::default(),
            attributes: Attributes::new(),
        };
        let json = serde_json::to_string(&UserDto::from(&user)).expect("serialize");
        assert!(!json.contains("argon2"), "{json}");
        assert!(!json.contains("password"), "{json}");
        assert!(!json.contains("SECRET"), "{json}");
        assert!(json.contains("\"username\":\"alice\""));
        assert!(json.contains("\"initial\":\"A\""));
    }

    #[test]
    fn feed_dtos_never_expose_stored_credentials() {
        let mut f = feed();
        f.set_http_auth(Some("alice"), Some("s3cret"));
        let json = serde_json::to_string(&FeedDto::new(&f)).expect("serialize");
        assert!(!json.contains("s3cret"), "{json}");
        assert!(!json.contains("alice"), "{json}");

        f.http_auth = None;
        f.url = "https://bob:pw@example.com/feed.xml".into();
        let dto = FeedDto::new(&f);
        assert_eq!(dto.url, "https://example.com/feed.xml");
        assert!(
            !serde_json::to_string(&dto).unwrap().contains("pw"),
            "credentials embedded in the url must be stripped too"
        );
    }

    #[test]
    fn feed_dto_reports_muted_and_ttl_separately() {
        let mut f = feed();
        let dto = FeedDto::new(&f);
        assert_eq!(dto.ttl, 3600);
        assert!(!dto.muted);

        f.ttl = Ttl::new(3600, true);
        let dto = FeedDto::new(&f);
        assert_eq!(dto.ttl, 3600, "the interval survives muting");
        assert!(
            dto.muted,
            "the sign is exposed as a flag, not as a negative number"
        );
    }

    #[test]
    fn favicon_only_links_the_endpoint_when_an_icon_is_known() {
        let f = feed();
        assert_eq!(FaviconDto::for_feed(&f, |_| "x".into()).url, None);
        assert_eq!(FaviconDto::for_feed(&f, |_| "x".into()).letter, "E");

        let mut with_icon = feed();
        with_icon
            .attributes
            .set(keys::FEED_ICON_URL, "https://example.com/icon.png");
        assert_eq!(
            FaviconDto::for_feed(&with_icon, |id| format!("/api/v1/feeds/{id}/icon"))
                .url
                .as_deref(),
            Some("/api/v1/feeds/7/icon")
        );
    }

    #[test]
    fn category_counts_roll_up_from_its_feeds() {
        let category = Category {
            id: 3,
            id_user: 1,
            name: "News".into(),
            kind: CategoryKind::Normal,
            last_update: 0,
            error: 0,
            position: 1,
            attributes: Attributes::new(),
        };
        let mut second = feed();
        second.id = 8;
        second.cache_nb_entries = 40;
        second.cache_nb_unreads = 7;

        let dto = CategoryDto::new(&category, vec![feed(), second]);
        assert_eq!(dto.total, 250);
        assert_eq!(dto.unread, 12);
        assert_eq!(dto.feeds.len(), 2);
        assert!(!dto.is_dynamic_opml);
    }

    #[test]
    fn an_empty_folder_counts_zero() {
        let category = Category {
            id: 3,
            id_user: 1,
            name: "Empty".into(),
            kind: CategoryKind::Normal,
            last_update: 0,
            error: 0,
            position: 1,
            attributes: Attributes::new(),
        };
        let dto = CategoryDto::new(&category, vec![]);
        assert_eq!(dto.total, 0);
        assert_eq!(dto.unread, 0);
    }

    #[test]
    fn ids_serialize_as_strings() {
        let json = serde_json::to_string(&FeedDto::new(&feed())).expect("serialize");
        assert!(json.contains("\"id\":\"7\""), "{json}");
        assert!(
            json.contains("\"category\":3"),
            "a nullable id stays a number in this field"
        );

        #[derive(Serialize)]
        struct Probe {
            #[serde(serialize_with = "serialize_id_opt")]
            id: Option<i64>,
        }
        assert_eq!(
            serde_json::to_string(&Probe { id: Some(12) }).unwrap(),
            r#"{"id":"12"}"#
        );
        assert_eq!(
            serde_json::to_string(&Probe { id: None }).unwrap(),
            r#"{"id":null}"#
        );
    }

    #[test]
    fn entry_ids_serialize_as_strings_too() {
        #[derive(Serialize)]
        struct Probe {
            id: EntryId,
        }
        let probe = Probe {
            id: EntryId::from_micros(1_728_656_123_456_789),
        };
        assert_eq!(
            serde_json::to_string(&probe).unwrap(),
            r#"{"id":"1728656123456789"}"#
        );
    }

    /// A request body carrying an id, the way a client that read it from a response sends it back.
    #[derive(Debug, Deserialize)]
    struct IdRequest {
        #[serde(default, deserialize_with = "deserialize_id")]
        category_id: Option<i64>,
    }

    fn id_of(json: &str) -> Option<i64> {
        serde_json::from_str::<IdRequest>(json)
            .unwrap_or_else(|err| panic!("{json} should deserialize: {err}"))
            .category_id
    }

    #[test]
    fn an_id_may_arrive_as_a_string_or_a_number() {
        assert_eq!(id_of(r#"{"category_id":"3"}"#), Some(3));
        assert_eq!(id_of(r#"{"category_id":3}"#), Some(3));
        assert_eq!(id_of(r#"{"category_id":null}"#), None);
        assert_eq!(id_of(r#"{"category_id":""}"#), None, "a cleared select");
        assert_eq!(id_of("{}"), None);
    }

    #[test]
    fn an_id_that_is_not_a_number_is_rejected_with_a_readable_message() {
        let err = serde_json::from_str::<IdRequest>(r#"{"category_id":"abc"}"#).unwrap_err();
        assert!(err.to_string().contains("not an id"), "{err}");
    }

    #[test]
    fn a_problems_row_carries_the_reason_the_feed_recorded() {
        let mut f = feed();
        assert_eq!(ProblemFeedDto::from(&f).message, None);

        f.error = 1_700_000_999;
        f.set_last_error_message(Some("http 503: unavailable"));
        let dto = ProblemFeedDto::from(&f);
        assert_eq!(dto.message.as_deref(), Some("http 503: unavailable"));
        assert_eq!(dto.error, 1_700_000_999);
        assert_eq!(dto.name, "Example");
        assert_eq!(dto.unread, 5);
        assert_eq!(dto.total, 210);

        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(json.contains(r#""id":"7""#), "{json}");
    }

    #[test]
    fn a_rejected_candidate_says_why_and_a_verified_one_says_what() {
        let rejected = Candidate {
            url: "https://example.com/rss".into(),
            source: Source::LocalBody,
            probe: None,
            rejected: Some("the response is not a feed".into()),
        };
        let dto = CandidateDto::from(&rejected);
        assert!(!dto.verified);
        assert_eq!(dto.title, None);
        assert_eq!(dto.item_count, None);
        assert_eq!(dto.rejected.as_deref(), Some("the response is not a feed"));
        assert_eq!(
            serde_json::to_string(&dto).unwrap(),
            r#"{"url":"https://example.com/rss","source":"local_body","verified":false,"title":null,"website":null,"description":null,"icon":null,"item_count":null,"format":null,"rejected":"the response is not a feed"}"#,
            "the dialog reads every one of these, so the shape is the contract"
        );

        let found = Discovered {
            url: "https://example.com/".into(),
            candidates: vec![Candidate {
                url: "https://example.com/feed.xml".into(),
                source: Source::Link,
                probe: Some(freshrss_feed::discovery::Probe {
                    title: Some("Example".into()),
                    website: Some("https://example.com/".into()),
                    description: None,
                    icon: None,
                    item_count: 12,
                    format: FeedFormat::Rss2,
                }),
                rejected: None,
            }],
        };
        let dto = DiscoveredDto::from(&found);
        assert!(!dto.is_feed, "the address given was a page, not a feed");
        assert_eq!(dto.candidates.len(), 1);
        assert!(dto.candidates[0].verified);
        assert_eq!(dto.candidates[0].item_count, Some(12));
    }

    #[test]
    fn a_refresh_outcome_names_itself_on_the_wire() {
        let fetched = Outcome {
            status: Status::Fetched,
            new_entries: 3,
            updated_entries: 1,
            unchanged: 8,
        };
        let dto = FeedRefreshDto::from(&fetched);
        assert_eq!(dto.status, "fetched");
        assert_eq!(dto.message, None);
        assert!(
            !serde_json::to_string(&dto).unwrap().contains("message"),
            "an absent message is omitted, not sent as null"
        );

        let failed = Outcome {
            status: Status::Failed {
                message: "http 503: unavailable".into(),
                http_status: Some(503),
            },
            new_entries: 0,
            updated_entries: 0,
            unchanged: 0,
        };
        let dto = FeedRefreshDto::from(&failed);
        assert_eq!(dto.status, "failed");
        assert_eq!(dto.message.as_deref(), Some("http 503: unavailable"));

        for (status, want) in [
            (Status::Unchanged, "unchanged"),
            (Status::NotModified, "not_modified"),
            (Status::Muted, "muted"),
        ] {
            assert_eq!(status_name(&status), want);
        }
    }

    #[test]
    fn a_job_reports_its_state_and_counts() {
        let job = Job {
            id: "abc".into(),
            id_user: 1,
            state: JobState::Done,
            started_at: 1_700_000_000,
            finished_at: Some(1_700_000_010),
            report: crate::service::refresh::run::Report {
                feeds_done: 4,
                new_entries: 20,
                updated_entries: 2,
                muted: 0,
                errors: vec![FeedFailure {
                    feed_id: 9,
                    name: "Broken".into(),
                    message: "http 404".into(),
                    http_status: Some(404),
                }],
            },
        };
        let dto = RefreshJobDto::from(&job);
        assert_eq!(dto.feeds_done, 4);
        assert_eq!(dto.new_entries, 20);
        assert_eq!(dto.finished_at, Some(1_700_000_010));
        assert_eq!(dto.errors.len(), 1);
        assert_eq!(
            serde_json::to_string(&dto.errors[0]).unwrap(),
            r#"{"feed_id":"9","name":"Broken","message":"http 404","http_status":404}"#
        );

        let status = RefreshStatusDto::from(&job);
        assert!(!status.running);
        assert_eq!(status.job_id.as_deref(), Some("abc"));
        assert_eq!(
            serde_json::to_string(&RefreshStatusDto::idle()).unwrap(),
            r#"{"running":false,"job_id":null,"feeds_done":0,"new_entries":0}"#
        );
    }
}
