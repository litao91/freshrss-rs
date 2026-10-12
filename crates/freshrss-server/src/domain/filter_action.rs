//! Filter actions — the only automation FreshRSS applies to articles during a refresh.
//!
//! Stored as the `filters` attribute on a feed, category, tag or user:
//! `[{"search": "<boolean search>", "actions": ["read", "star", "label"]}]`.
//!
//! There are exactly three action types. Notably there is no "mark unread", "delete" or
//! "add tag by name" — the mark-as-read family (`read_upon_reception`, `read_upon_gone`,
//! `read_when_same_title_*`, `keep_max_n_unread`) is separate per-feed/per-user configuration,
//! not a filter action.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterActionType {
    /// Mark the article read.
    Read,
    /// Star the article. Skipped for articles that were *updated* rather than newly seen, so an
    /// upstream edit cannot overrule a deliberate un-star.
    Star,
    /// Attach the owning label. Same updated-article guard as [`Self::Star`].
    Label,
}

impl FilterActionType {
    /// True when the action must be skipped for an article whose content changed upstream.
    ///
    /// Only `star` and `label` are guarded; `read` still applies, because marking a revised
    /// article read is the common intent (a corrections page, a re-published post).
    pub const fn skipped_for_updated_articles(self) -> bool {
        matches!(self, FilterActionType::Star | FilterActionType::Label)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            FilterActionType::Read => "read",
            FilterActionType::Star => "star",
            FilterActionType::Label => "label",
        }
    }
}

/// One `{search, actions}` rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterAction {
    /// A BooleanSearch query, stored **unexpanded**: `S:`/`search:` references are resolved at
    /// evaluation time so renaming or deleting a saved query does not silently corrupt the rule.
    pub search: String,
    pub actions: Vec<FilterActionType>,
}

impl FilterAction {
    /// FreshRSS's `FilterAction::fromJSON` requires both a non-empty search and a non-empty
    /// action list; a rule missing either is inert and is dropped rather than stored.
    pub fn is_valid(&self) -> bool {
        !self.search.trim().is_empty() && !self.actions.is_empty()
    }

    pub fn performs(&self, action: FilterActionType) -> bool {
        self.actions.contains(&action)
    }

    /// Filters a list of rules down to the usable ones.
    pub fn retain_valid(rules: &mut Vec<FilterAction>) {
        rules.retain(FilterAction::is_valid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_match_the_stored_format() {
        assert_eq!(FilterActionType::Read.as_str(), "read");
        assert_eq!(FilterActionType::Star.as_str(), "star");
        assert_eq!(FilterActionType::Label.as_str(), "label");
        assert_eq!(serde_json::to_string(&FilterActionType::Read).unwrap(), "\"read\"");
    }

    #[test]
    fn star_and_label_are_guarded_against_updated_articles_but_read_is_not() {
        assert!(FilterActionType::Star.skipped_for_updated_articles());
        assert!(FilterActionType::Label.skipped_for_updated_articles());
        assert!(!FilterActionType::Read.skipped_for_updated_articles());
    }

    #[test]
    fn a_rule_needs_both_a_search_and_an_action() {
        let good = FilterAction {
            search: "intitle:sponsored".into(),
            actions: vec![FilterActionType::Read],
        };
        assert!(good.is_valid());
        assert!(good.performs(FilterActionType::Read));
        assert!(!good.performs(FilterActionType::Star));

        assert!(!FilterAction { search: "".into(), actions: vec![FilterActionType::Read] }.is_valid());
        assert!(!FilterAction { search: "   ".into(), actions: vec![FilterActionType::Read] }.is_valid());
        assert!(!FilterAction { search: "x".into(), actions: vec![] }.is_valid());
    }

    #[test]
    fn invalid_rules_are_dropped() {
        let mut rules = vec![
            FilterAction { search: "a".into(), actions: vec![FilterActionType::Read] },
            FilterAction { search: "".into(), actions: vec![FilterActionType::Star] },
            FilterAction { search: "b".into(), actions: vec![] },
        ];
        FilterAction::retain_valid(&mut rules);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].search, "a");
    }

    #[test]
    fn round_trips_through_the_stored_json_shape() {
        let json = r#"[{"search":"author:bot","actions":["read","star"]}]"#;
        let rules: Vec<FilterAction> = serde_json::from_str(json).expect("parse");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].search, "author:bot");
        assert_eq!(
            rules[0].actions,
            vec![FilterActionType::Read, FilterActionType::Star]
        );
        assert_eq!(serde_json::to_string(&rules).unwrap(), json);
    }

    #[test]
    fn an_unknown_action_is_rejected_rather_than_ignored() {
        let json = r#"[{"search":"a","actions":["delete"]}]"#;
        assert!(serde_json::from_str::<Vec<FilterAction>>(json).is_err());
    }
}
