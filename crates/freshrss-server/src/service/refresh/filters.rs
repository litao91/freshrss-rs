//! Filter actions and the mark-when policies: the automation one refresh applies to each article.
//!
//! Two families, deliberately kept apart because FreshRSS keeps them apart. **Filter actions** are
//! `{search, actions}` rules the user wrote; they are evaluated with the in-memory matcher, because
//! they run over articles that do not have ids yet. **Mark-when** is configuration — "mark read on
//! arrival", "mark read when the title repeats" — and two of its policies compare an article against
//! what is already stored rather than against a query.
//!
//! The order is PHP's, and it is observable: user rules, then the folder's, then the feed's, then
//! each label's, each evaluated in the order written, and an article's title only starts counting as
//! "already read" for the articles *after* it in the batch.

use std::collections::HashSet;

use freshrss_search::ast::Expr;
use freshrss_search::expand::{expand, QueryResolver};
use freshrss_search::matcher::{matches, SearchTarget};
use freshrss_search::parser::parse;

use crate::db::dao::{category as cat_dao, entry as entry_dao, tag as tag_dao, user as user_dao};
use crate::db::Db;
use crate::domain::attributes::keys;
use crate::domain::category::Category;
use crate::domain::feed::Feed;
use crate::domain::filter_action::{FilterAction, FilterActionType};
use crate::domain::tag::Tag;
use crate::domain::user::UserConfig;
use crate::error::Result;
use crate::service::search::SavedSearches;

/// What the automation decided for one article.
///
/// Three booleans and a list rather than a mutated entry, because the caller writes new articles and
/// updated articles differently: a new one takes `read` and `star` straight into its row, while an
/// updated one can only have them *set* (never cleared) through the `COALESCE` in
/// [`crate::db::dao::entry::update_existing`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub read: bool,
    pub star: bool,
    /// Names of existing labels to attach. Resolved to ids after the commit, which is the first
    /// point at which the article has an id to link.
    pub labels: Vec<String>,
}

impl Applied {
    pub fn is_empty(&self) -> bool {
        !self.read && !self.star && self.labels.is_empty()
    }
}

/// One compiled rule.
struct Rule {
    expr: Expr,
    actions: Vec<FilterActionType>,
}

/// One source of rules.
struct Source {
    /// The label a matching `label` action attaches. Only a label source has one: in PHP a `label`
    /// action on the user, folder or feed rules sets a flag that nobody reads, so it is inert there,
    /// and being inert here keeps a feed's rules from attaching a label the feed does not own.
    label: Option<String>,
    rules: Vec<Rule>,
}

/// Every filter rule that applies to one feed's articles, compiled once per refresh.
///
/// Compiled rather than parsed per article: a feed with ten rules and two hundred articles would
/// otherwise parse the same ten queries two hundred times, and a bad regex would be reported two
/// hundred times.
pub struct Rules {
    sources: Vec<Source>,
    /// The labels that can be attached, by name. A `label` action names a label that already exists;
    /// this is what resolves the name to the id the link table needs, after the commit.
    labels: Vec<(String, i64)>,
}

impl Rules {
    /// Compiles the rules of the user, the folder, the feed and each of the user's labels.
    ///
    /// A rule whose search does not parse — or whose saved query no longer exists — is dropped with
    /// a warning rather than failing the refresh. The user typed it into a settings box, possibly
    /// against an older grammar, and one stale rule must not stop articles arriving.
    pub fn compile(
        resolver: &dyn QueryResolver,
        user: &UserConfig,
        category: Option<&Category>,
        feed: &Feed,
        labels: &[Tag],
        now: i64,
    ) -> Self {
        let mut sources = Vec::new();
        sources.push(Source {
            label: None,
            rules: compile_rules(resolver, &user.filters, now, "the user's"),
        });
        if let Some(category) = category {
            let rules = category.filter_actions();
            sources.push(Source {
                label: None,
                rules: compile_rules(resolver, &rules, now, "a folder's"),
            });
        }
        let feed_rules = feed.filter_actions();
        sources.push(Source {
            label: None,
            rules: compile_rules(resolver, &feed_rules, now, "a feed's"),
        });
        for tag in labels {
            let rules = tag.filter_actions();
            if rules.is_empty() {
                continue;
            }
            sources.push(Source {
                label: Some(tag.name.clone()),
                rules: compile_rules(resolver, &rules, now, "a label's"),
            });
        }
        sources.retain(|source| !source.rules.is_empty());
        let labels = labels
            .iter()
            .filter(|tag| tag.has_label_filter())
            .map(|tag| (tag.name.clone(), tag.id))
            .collect();
        Rules { sources, labels }
    }

    /// The id of an existing label with this name, when there is one.
    pub fn label_id(&self, name: &str) -> Option<i64> {
        self.labels
            .iter()
            .find(|(label, _)| label == name)
            .map(|(_, id)| *id)
    }

    /// True when there is nothing to evaluate, which is the common case and worth not paying for.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Evaluates every rule against one article.
    ///
    /// `updated` is whether this refresh found the article's content had changed upstream. `star`
    /// and `label` are skipped for those, so an upstream edit cannot overrule a deliberate un-star;
    /// `read` still applies, because marking a revised article read is the common intent.
    pub fn apply(&self, target: &dyn SearchTarget, updated: bool) -> Applied {
        let mut applied = Applied::default();
        for source in &self.sources {
            for rule in &source.rules {
                // A rule that cannot be evaluated against this article — an `e:` over an unstored
                // item, say — counts as not matching rather than as an error: the alternative is one
                // bad rule aborting a whole refresh.
                if !matches(&rule.expr, target).unwrap_or(false) {
                    continue;
                }
                for action in &rule.actions {
                    match action {
                        FilterActionType::Read => applied.read = true,
                        FilterActionType::Star if !updated => applied.star = true,
                        FilterActionType::Label if !updated => {
                            if let Some(name) = &source.label {
                                if !applied.labels.iter().any(|n| n == name) {
                                    applied.labels.push(name.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        applied
    }
}

fn compile_rules(
    resolver: &dyn QueryResolver,
    rules: &[FilterAction],
    now: i64,
    whose: &str,
) -> Vec<Rule> {
    rules
        .iter()
        .filter(|rule| rule.is_valid())
        .filter_map(|rule| {
            let parsed = parse(&rule.search, now).and_then(|expr| expand(&expr, resolver, now));
            match parsed {
                Ok(expr) => Some(Rule {
                    expr,
                    actions: rule.actions.clone(),
                }),
                Err(err) => {
                    tracing::warn!(
                        search = %rule.search,
                        error = %err,
                        "ignoring {whose} filter rule, which does not parse"
                    );
                    None
                }
            }
        })
        .collect()
}

/// The mark-when settings that apply to one feed's articles.
///
/// Each is resolved feed attribute → folder attribute → user config, which is the chain PHP follows
/// for these (with the two category policies having no user-config fallback, exactly as there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Policies {
    /// New articles arrive already read.
    pub read_upon_reception: bool,
    /// An upstream edit clears the read flag.
    pub mark_updated_unread: bool,
    /// How many of the feed's most recent titles count as already read. 0 disables.
    pub same_title_in_feed: usize,
    /// How many of the folder's most recent titles count as already read. 0 disables.
    pub same_title_in_category: usize,
    /// How many of the folder's most recent guids count as already read. 0 disables.
    pub same_guid_in_category: usize,
}

impl Policies {
    pub fn resolve(feed: &Feed, category: Option<&Category>, user: &UserConfig) -> Self {
        // The feed attribute may be stored as a boolean (`false` meaning "off for this feed even
        // though my user config says on") or as a count, and the two have to be told apart: a count
        // of 0 and an explicit `false` agree, but an absent attribute defers to the user.
        let same_title_in_feed = match feed.attributes.get(keys::READ_WHEN_SAME_TITLE_IN_FEED) {
            Some(value) if value.as_bool() == Some(false) => 0,
            Some(value) => count(value.as_i64()),
            None => usize::from(user.mark_when.same_title_in_feed),
        };
        Policies {
            read_upon_reception: feed
                .attributes
                .get_bool(keys::READ_UPON_RECEPTION)
                .unwrap_or(user.mark_when.reception),
            mark_updated_unread: feed
                .attributes
                .get_bool(keys::MARK_UPDATED_ARTICLE_UNREAD)
                .unwrap_or(user.mark_when.updated_unread),
            same_title_in_feed,
            same_title_in_category: category
                .and_then(|c| c.attributes.get(keys::READ_WHEN_SAME_TITLE_IN_CATEGORY))
                .map(|value| count(value.as_i64()))
                .unwrap_or(0),
            same_guid_in_category: category
                .and_then(|c| c.attributes.get(keys::READ_WHEN_SAME_GUID_IN_CATEGORY))
                .map(|value| count(value.as_i64()))
                .unwrap_or(0),
        }
    }

    /// True when any of the three comparison policies is on, which is what decides whether the
    /// "already read" sets are loaded and tracked at all.
    pub fn tracks_duplicates(&self) -> bool {
        self.same_title_in_feed > 0
            || self.same_title_in_category > 0
            || self.same_guid_in_category > 0
    }
}

fn count(value: Option<i64>) -> usize {
    usize::try_from(value.unwrap_or(0).max(0)).unwrap_or(usize::MAX)
}

/// The titles and guids already known to be read, which the duplicate-title and duplicate-guid
/// policies compare each article against.
///
/// Seeded from the database — an article repeating a title from *yesterday* is the case the policy
/// exists for — and then grown as the batch is processed, in feed order.
#[derive(Debug, Default)]
pub struct Seen {
    titles_in_feed: HashSet<String>,
    titles_in_category: HashSet<String>,
    guids_in_category: HashSet<String>,
}

impl Seen {
    /// Loads the stored half of the sets.
    ///
    /// Takes the connection rather than the pool because a refresh calls this from inside its write:
    /// the sets have to be the ones the write itself sees, or an article committed by a concurrent
    /// refresh could be counted twice or not at all.
    pub async fn load(
        conn: &mut sqlx::SqliteConnection,
        feed: &Feed,
        policies: &Policies,
    ) -> Result<Seen> {
        if !policies.tracks_duplicates() {
            return Ok(Seen::default());
        }
        let mut seen = Seen::default();
        if policies.same_title_in_feed > 0 {
            seen.titles_in_feed =
                entry_dao::list_recent_titles(conn, feed.id, policies.same_title_in_feed)
                    .await?
                    .into_iter()
                    .collect();
        }
        if let Some(category) = feed.category {
            if policies.same_title_in_category > 0 {
                seen.titles_in_category = entry_dao::list_recent_titles_in_category(
                    conn,
                    feed.id_user,
                    category,
                    policies.same_title_in_category,
                )
                .await?
                .into_iter()
                .collect();
            }
            if policies.same_guid_in_category > 0 {
                seen.guids_in_category = entry_dao::list_recent_guids_in_category(
                    conn,
                    feed.id_user,
                    category,
                    policies.same_guid_in_category,
                )
                .await?
                .into_iter()
                .collect();
            }
        }
        Ok(seen)
    }

    /// Whether this article repeats something already read.
    ///
    /// Only the sets whose policy is on are consulted, and [`Seen::load`] only fills those, so a
    /// disabled policy cannot match by accident.
    pub fn duplicates(&self, policies: &Policies, title: &str, guid: &str) -> bool {
        (policies.same_title_in_feed > 0 && self.titles_in_feed.contains(title))
            || (policies.same_title_in_category > 0 && self.titles_in_category.contains(title))
            || (policies.same_guid_in_category > 0 && self.guids_in_category.contains(guid))
    }

    /// Records one processed article, so the ones after it see it.
    pub fn record(&mut self, policies: &Policies, title: &str, guid: &str) {
        if policies.same_title_in_feed > 0 {
            self.titles_in_feed.insert(title.to_owned());
        }
        if policies.same_title_in_category > 0 {
            self.titles_in_category.insert(title.to_owned());
        }
        if policies.same_guid_in_category > 0 {
            self.guids_in_category.insert(guid.to_owned());
        }
    }
}

/// Decides the read flag for one article, which is where the two families meet.
///
/// `updated` articles skip the duplicate checks on purpose: a revised article's title is usually the
/// title of the article it revises, so counting it as a duplicate would mark every correction read
/// for exactly the reason the policy exists to catch *different* articles sharing a headline.
pub fn read_flag(
    policies: &Policies,
    applied: &Applied,
    seen: &Seen,
    updated: bool,
    title: &str,
    guid: &str,
) -> bool {
    applied.read
        || (!updated && (policies.read_upon_reception || seen.duplicates(policies, title, guid)))
}

/// Everything one refresh needs to decide what to do with each article, loaded once.
///
/// Loaded before the write rather than inside it: compiling rules parses queries and resolves saved
/// searches, and the writer lock is process-wide, so holding it for that would pause every other
/// user's commit for work that touches no rows.
pub struct Automation {
    pub policies: Policies,
    pub rules: Rules,
}

impl Automation {
    /// Reads the user config, the folder, the labels and the saved searches the rules may reference.
    pub async fn load(db: &Db, feed: &Feed, now: i64) -> Result<Automation> {
        let (id_user, category_id) = (feed.id_user, feed.category);
        let (config, category, tags, saved) = db
            .with(move |mut conn| async move {
                let config = user_dao::find_by_id(&mut conn, id_user)
                    .await?
                    .map(|user| user.config);
                let category = match category_id {
                    Some(id) => cat_dao::find_by_id(&mut conn, id, id_user).await?,
                    None => None,
                };
                let tags = tag_dao::list(&mut conn, id_user).await?;
                let saved = SavedSearches::load(&mut conn, id_user).await?;
                Ok((config, category, tags, saved))
            })
            .await?;
        let config = config.unwrap_or_default();
        Ok(Automation {
            policies: Policies::resolve(feed, category.as_ref(), &config),
            rules: Rules::compile(&saved, &config, category.as_ref(), feed, &tags, now),
        })
    }

    /// True when there is nothing to apply, which is the common case: most feeds carry no rules and
    /// most users leave the mark-when defaults off.
    pub fn is_inert(&self) -> bool {
        self.rules.is_empty()
            && !self.policies.tracks_duplicates()
            && !self.policies.read_upon_reception
            && !self.policies.mark_updated_unread
    }
}

#[cfg(test)]
mod tests {
    use freshrss_search::matcher::SearchTarget;

    use crate::domain::attributes::{keys, Attributes};
    use crate::domain::filter_action::FilterActionType;
    use crate::service::search::SavedSearches;

    use super::*;

    /// A target with the fields a test cares about; everything else is empty.
    struct Target {
        title: String,
        content: String,
        author: String,
    }

    impl Target {
        fn new(title: &str) -> Self {
            Target {
                title: title.to_owned(),
                content: String::new(),
                author: String::new(),
            }
        }
    }

    impl SearchTarget for Target {
        fn id(&self) -> Option<i64> {
            Some(1)
        }
        fn feed_id(&self) -> i64 {
            1
        }
        fn category_id(&self) -> Option<i64> {
            None
        }
        fn title(&self) -> &str {
            &self.title
        }
        fn content(&self) -> &str {
            &self.content
        }
        fn author(&self) -> &str {
            &self.author
        }
        fn link(&self) -> &str {
            ""
        }
        fn tags(&self) -> &str {
            ""
        }
        fn label_ids(&self) -> &[i64] {
            &[]
        }
        fn label_names(&self) -> &[String] {
            &[]
        }
        fn date(&self) -> i64 {
            0
        }
        fn last_modified(&self) -> Option<i64> {
            None
        }
        fn last_user_modified(&self) -> Option<i64> {
            None
        }
    }

    const NOW: i64 = 1_700_000_000;

    fn rules(rules: &[(&str, &[FilterActionType])]) -> Rules {
        let parsed: Vec<FilterAction> = rules
            .iter()
            .map(|(search, actions)| FilterAction {
                search: (*search).to_owned(),
                actions: actions.to_vec(),
            })
            .collect();
        let mut feed = Feed::new(1, "https://e.example/f", "Example");
        feed.set_filter_actions(parsed);
        Rules::compile(
            &SavedSearches::empty(),
            &UserConfig::default(),
            None,
            &feed,
            &[],
            NOW,
        )
    }

    #[test]
    fn a_matching_rule_applies_its_actions() {
        let rules = rules(&[("intitle:release", &[FilterActionType::Read])]);
        let applied = rules.apply(&Target::new("Release 1.0"), false);
        assert!(applied.read);
        assert!(!applied.star);

        let applied = rules.apply(&Target::new("Changelog"), false);
        assert!(applied.is_empty());
    }

    #[test]
    fn star_and_label_are_skipped_for_updated_articles_but_read_is_not() {
        let rules = rules(&[(
            "intitle:release",
            &[
                FilterActionType::Read,
                FilterActionType::Star,
                FilterActionType::Label,
            ],
        )]);
        let applied = rules.apply(&Target::new("Release 1.0"), true);
        assert!(
            applied.read,
            "marking a correction read is the common intent"
        );
        assert!(
            !applied.star,
            "an edit must not overrule a deliberate un-star"
        );
        assert!(applied.labels.is_empty());
    }

    #[test]
    fn a_label_action_on_a_feed_rule_attaches_nothing() {
        // PHP's `label` action sets a flag only the label source reads, so on a feed rule it is
        // inert. Attaching a label a feed does not own would be a surprise with no way to undo it
        // in bulk.
        let rules = rules(&[("intitle:release", &[FilterActionType::Label])]);
        let applied = rules.apply(&Target::new("Release 1.0"), false);
        assert!(applied.labels.is_empty());
    }

    #[test]
    fn a_label_source_attaches_its_own_label() {
        let mut tag = Tag::new(1, "todo");
        let _ = tag.attributes.set_typed(
            keys::FILTERS,
            &[FilterAction {
                search: "intitle:release".to_owned(),
                actions: vec![FilterActionType::Label],
            }],
        );
        let feed = Feed::new(1, "https://e.example/f", "Example");
        let rules = Rules::compile(
            &SavedSearches::empty(),
            &UserConfig::default(),
            None,
            &feed,
            &[tag],
            NOW,
        );
        let applied = rules.apply(&Target::new("Release 1.0"), false);
        assert_eq!(applied.labels, vec!["todo".to_owned()]);
    }

    #[test]
    fn a_rule_that_does_not_parse_is_dropped_rather_than_fatal() {
        let rules = rules(&[
            ("intitle:a OR", &[FilterActionType::Read]),
            ("intitle:release", &[FilterActionType::Read]),
        ]);
        assert_eq!(rules.sources.len(), 1);
        assert_eq!(rules.sources[0].rules.len(), 1, "the good rule survived");
        assert!(rules.apply(&Target::new("Release 1.0"), false).read);
    }

    #[test]
    fn rules_compile_in_source_order() {
        let user = UserConfig {
            filters: vec![FilterAction {
                search: "intitle:user".to_owned(),
                actions: vec![FilterActionType::Read],
            }],
            ..UserConfig::default()
        };
        let mut feed = Feed::new(1, "https://e.example/f", "Example");
        feed.set_filter_actions(vec![FilterAction {
            search: "intitle:feed".to_owned(),
            actions: vec![FilterActionType::Star],
        }]);
        let rules = Rules::compile(&SavedSearches::empty(), &user, None, &feed, &[], NOW);
        assert_eq!(rules.sources.len(), 2, "user first, feed second");
    }

    #[test]
    fn an_empty_rule_set_is_reported_as_empty() {
        let feed = Feed::new(1, "https://e.example/f", "Example");
        let rules = Rules::compile(
            &SavedSearches::empty(),
            &UserConfig::default(),
            None,
            &feed,
            &[],
            NOW,
        );
        assert!(rules.is_empty());
        assert!(rules.apply(&Target::new("anything"), false).is_empty());
    }

    // --- policies -----------------------------------------------------------

    fn feed_with(attributes: Attributes) -> Feed {
        let mut feed = Feed::new(1, "https://e.example/f", "Example");
        feed.attributes = attributes;
        feed
    }

    #[test]
    fn policies_defer_to_the_user_when_the_feed_says_nothing() {
        let mut user = UserConfig::default();
        user.mark_when.reception = true;
        user.mark_when.updated_unread = true;
        user.mark_when.same_title_in_feed = true;
        let policies = Policies::resolve(&feed_with(Attributes::new()), None, &user);
        assert!(policies.read_upon_reception);
        assert!(policies.mark_updated_unread);
        assert_eq!(
            policies.same_title_in_feed, 1,
            "a boolean user setting loads one title, which is PHP's cast"
        );
    }

    #[test]
    fn a_feed_overrides_the_user_in_both_directions() {
        let mut user = UserConfig::default();
        user.mark_when.reception = true;
        let mut attributes = Attributes::new();
        attributes.set_bool(keys::READ_UPON_RECEPTION, Some(false));
        attributes.set(keys::READ_WHEN_SAME_TITLE_IN_FEED, 25);
        let policies = Policies::resolve(&feed_with(attributes), None, &user);
        assert!(
            !policies.read_upon_reception,
            "an explicit false beats the user's true"
        );
        assert_eq!(policies.same_title_in_feed, 25);
    }

    #[test]
    fn the_category_policies_have_no_user_fallback() {
        let mut user = UserConfig::default();
        user.mark_when.same_title_in_category = true;
        user.mark_when.same_guid_in_category = true;
        let policies = Policies::resolve(&feed_with(Attributes::new()), None, &user);
        assert_eq!(policies.same_title_in_category, 0);
        assert_eq!(policies.same_guid_in_category, 0);

        let mut category = Category::new(1, "News");
        category
            .attributes
            .set(keys::READ_WHEN_SAME_TITLE_IN_CATEGORY, 10);
        let policies = Policies::resolve(&feed_with(Attributes::new()), Some(&category), &user);
        assert_eq!(policies.same_title_in_category, 10);
        assert_eq!(policies.same_guid_in_category, 0);
    }

    // --- the read flag ------------------------------------------------------

    #[test]
    fn reception_and_duplicates_only_apply_to_new_articles() {
        let policies = Policies {
            read_upon_reception: true,
            ..Policies::default()
        };
        let seen = Seen::default();
        assert!(read_flag(
            &policies,
            &Applied::default(),
            &seen,
            false,
            "t",
            "g"
        ));
        assert!(
            !read_flag(&policies, &Applied::default(), &seen, true, "t", "g"),
            "an edit is not a new arrival"
        );
    }

    #[test]
    fn a_duplicate_title_marks_read_only_when_the_policy_is_on() {
        let mut seen = Seen::default();
        seen.titles_in_feed.insert("Breaking".to_owned());

        let off = Policies::default();
        assert!(!read_flag(
            &off,
            &Applied::default(),
            &seen,
            false,
            "Breaking",
            "g"
        ));

        let on = Policies {
            same_title_in_feed: 10,
            ..Policies::default()
        };
        assert!(read_flag(
            &on,
            &Applied::default(),
            &seen,
            false,
            "Breaking",
            "g"
        ));
        assert!(
            !read_flag(&on, &Applied::default(), &seen, false, "Other", "g"),
            "a different title is not a duplicate"
        );
    }

    #[test]
    fn recording_is_gated_on_the_policy_so_disabled_sets_stay_empty() {
        let mut seen = Seen::default();
        seen.record(&Policies::default(), "Breaking", "g");
        assert!(seen.titles_in_feed.is_empty());

        let on = Policies {
            same_title_in_feed: 10,
            ..Policies::default()
        };
        seen.record(&on, "Breaking", "g");
        assert!(seen.duplicates(&on, "Breaking", "other-guid"));
    }
}
