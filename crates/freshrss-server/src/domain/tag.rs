//! The `tag` row — a user-created label, distinct from the feed-provided keywords an article
//! carries in its own `tags` column.
//!
//! The distinction matters for search: `#foo` matches the feed's keywords, while `L:` and
//! `labels:` match these rows through the `entrytag` join.

use crate::domain::attributes::Attributes;
use crate::domain::filter_action::FilterAction;

pub const TAG_NAME_MAX_LEN: usize = 191;

#[derive(Debug, Clone, PartialEq)]
pub struct Tag {
    pub id: i64,
    pub id_user: i64,
    pub name: String,
    pub attributes: Attributes,
}

impl Tag {
    pub fn new(id_user: i64, name: impl Into<String>) -> Self {
        Self {
            id: 0,
            id_user,
            name: name.into(),
            attributes: Attributes::new(),
        }
    }

    /// Normalizes and validates a label name.
    ///
    /// `#` is stripped from the front because users type `#todo` into a label field when the
    /// search syntax taught them to, and storing it would make `labels:todo` miss the row.
    pub fn normalize_name(raw: &str) -> Result<String, &'static str> {
        let trimmed = raw.trim().trim_start_matches('#').trim();
        let name: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            return Err("label name must not be empty");
        }
        if name.chars().count() > TAG_NAME_MAX_LEN {
            return Err("label name must be at most 191 characters");
        }
        // A comma would make the name ambiguous in `labels:"a,b"` and in the `#a #b` column
        // format, so reject it rather than silently splitting.
        if name.contains(',') {
            return Err("label name must not contain a comma");
        }
        Ok(name)
    }

    /// Filter actions owned by this label. FreshRSS stores them in the tag's `attributes` and
    /// applies them after a refresh commits, because attaching a label needs real entry ids.
    pub fn filter_actions(&self) -> Vec<FilterAction> {
        self.attributes
            .get_typed::<Vec<FilterAction>>(crate::domain::attributes::keys::FILTERS)
            .unwrap_or_default()
    }

    /// True when this label auto-attaches itself to matching articles.
    pub fn has_label_filter(&self) -> bool {
        self.filter_actions()
            .iter()
            .any(|rule| rule.is_valid() && rule.performs(crate::domain::filter_action::FilterActionType::Label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attributes::keys;
    use crate::domain::filter_action::FilterActionType;

    #[test]
    fn names_are_trimmed_and_a_leading_hash_is_removed() {
        assert_eq!(Tag::normalize_name("todo").unwrap(), "todo");
        assert_eq!(Tag::normalize_name("  todo  ").unwrap(), "todo");
        assert_eq!(Tag::normalize_name("#todo").unwrap(), "todo");
        assert_eq!(Tag::normalize_name("##todo").unwrap(), "todo");
        assert_eq!(Tag::normalize_name("read\n  later").unwrap(), "read later");
    }

    #[test]
    fn empty_and_over_long_names_are_rejected() {
        assert!(Tag::normalize_name("").is_err());
        assert!(Tag::normalize_name("   ").is_err());
        assert!(Tag::normalize_name("#").is_err(), "a bare hash is not a name");
        assert!(Tag::normalize_name(&"t".repeat(TAG_NAME_MAX_LEN)).is_ok());
        assert!(Tag::normalize_name(&"t".repeat(TAG_NAME_MAX_LEN + 1)).is_err());
    }

    #[test]
    fn commas_are_rejected_because_they_are_separators() {
        assert!(Tag::normalize_name("a,b").is_err());
        assert!(Tag::normalize_name("a, b").is_err());
    }

    #[test]
    fn no_filters_by_default() {
        let t = Tag::new(1, "todo");
        assert!(t.filter_actions().is_empty());
        assert!(!t.has_label_filter());
    }

    #[test]
    fn a_label_filter_is_detected() {
        let mut t = Tag::new(1, "todo");
        t.attributes
            .set_typed(
                keys::FILTERS,
                &vec![FilterAction {
                    search: "intitle:rust".into(),
                    actions: vec![FilterActionType::Label],
                }],
            )
            .expect("serializable");
        assert!(t.has_label_filter());
        assert_eq!(t.filter_actions().len(), 1);
    }

    #[test]
    fn a_read_only_filter_does_not_make_the_label_auto_attach() {
        let mut t = Tag::new(1, "todo");
        t.attributes
            .set_typed(
                keys::FILTERS,
                &vec![FilterAction {
                    search: "intitle:rust".into(),
                    actions: vec![FilterActionType::Read],
                }],
            )
            .expect("serializable");
        assert!(!t.has_label_filter());
    }
}
