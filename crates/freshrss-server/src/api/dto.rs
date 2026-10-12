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

use serde::Serialize;

use crate::domain::category::{Category, CategoryKind};
use crate::domain::feed::{Feed, LetterAvatar};
use crate::domain::ids::EntryId;
use crate::domain::user::{SortOrder, User, UserConfig};

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

/// A feed the discovery step found at a website URL.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredFeedDto {
    pub url: String,
    pub title: String,
    pub kind: i16,
    pub website: String,
    pub description: String,
    pub item_count: usize,
    /// True when the candidate was actually fetched and parsed, not just linked from the page.
    pub verified: bool,
}

/// One row of `#rd-problems-dialog`.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemFeedDto {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub name: String,
    pub url: String,
    pub category: Option<String>,
    pub error: i64,
    pub last_update: i64,
    pub muted: bool,
    pub ttl: i32,
    pub kind: i16,
    /// A short human explanation, because the dialog's whole job is to tell the user what to do.
    pub reason: String,
    /// A machine-readable tag such as `http:404`, shown in the dialog's `<code>`.
    pub code: Option<String>,
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
}
