//! The `category` row — a folder in the reader's sidebar.

use serde::{Deserialize, Serialize};

use crate::domain::attributes::Attributes;

/// FreshRSS's `category.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "i16", into = "i16")]
pub enum CategoryKind {
    /// An ordinary folder the user created.
    #[default]
    Normal,
    /// A folder whose contents come from a remote OPML document, refreshed periodically.
    /// Stored and exported, but not fetched in v1.
    DynamicOpml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownCategoryKind(pub i16);

impl std::fmt::Display for UnknownCategoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not a valid category kind", self.0)
    }
}

impl std::error::Error for UnknownCategoryKind {}

impl CategoryKind {
    pub const fn as_i16(self) -> i16 {
        match self {
            CategoryKind::Normal => 0,
            CategoryKind::DynamicOpml => 2,
        }
    }

    pub const fn from_i16(value: i16) -> Result<Self, UnknownCategoryKind> {
        match value {
            0 => Ok(CategoryKind::Normal),
            2 => Ok(CategoryKind::DynamicOpml),
            other => Err(UnknownCategoryKind(other)),
        }
    }
}

impl TryFrom<i16> for CategoryKind {
    type Error = UnknownCategoryKind;

    fn try_from(value: i16) -> Result<Self, Self::Error> {
        Self::from_i16(value)
    }
}

impl From<CategoryKind> for i16 {
    fn from(kind: CategoryKind) -> Self {
        kind.as_i16()
    }
}

/// The folder every new feed lands in when the caller does not name one. FreshRSS seeds a single
/// global row with `id = 1`; we seed one per user at registration.
pub const DEFAULT_CATEGORY_NAME: &str = "Uncategorized";

pub const CATEGORY_NAME_MAX_LEN: usize = 191;

#[derive(Debug, Clone, PartialEq)]
pub struct Category {
    pub id: i64,
    pub id_user: i64,
    pub name: String,
    pub kind: CategoryKind,
    pub last_update: i64,
    /// 0 when healthy, otherwise the unix time of the last failure. Same convention as `feed.error`.
    pub error: i64,
    /// Sidebar ordering; ascending.
    pub position: i64,
    pub attributes: Attributes,
}

impl Category {
    pub fn new(id_user: i64, name: impl Into<String>) -> Self {
        Self {
            id: 0,
            id_user,
            name: name.into(),
            kind: CategoryKind::default(),
            last_update: 0,
            error: 0,
            position: 0,
            attributes: Attributes::new(),
        }
    }

    /// Normalizes and validates a folder name.
    ///
    /// Whitespace is collapsed and trimmed because names come from OPML imports and free-text
    /// forms, where stray newlines are common. The empty result is rejected rather than silently
    /// renamed, since `UNIQUE(id_user, name)` would otherwise collect several "" folders.
    pub fn normalize_name(raw: &str) -> Result<String, &'static str> {
        let name: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            return Err("folder name must not be empty");
        }
        if name.chars().count() > CATEGORY_NAME_MAX_LEN {
            return Err("folder name must be at most 191 characters");
        }
        Ok(name)
    }

    pub fn is_dynamic_opml(&self) -> bool {
        self.kind == CategoryKind::DynamicOpml
    }

    pub fn has_error(&self) -> bool {
        self.error > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_values_match_freshrss() {
        assert_eq!(CategoryKind::Normal.as_i16(), 0);
        assert_eq!(CategoryKind::DynamicOpml.as_i16(), 2);
        assert_eq!(CategoryKind::from_i16(0).unwrap(), CategoryKind::Normal);
        assert_eq!(
            CategoryKind::from_i16(2).unwrap(),
            CategoryKind::DynamicOpml
        );
        assert!(CategoryKind::from_i16(1).is_err());
        assert_eq!(CategoryKind::default(), CategoryKind::Normal);
    }

    #[test]
    fn new_defaults_to_a_healthy_normal_folder() {
        let c = Category::new(7, "News");
        assert_eq!(c.id_user, 7);
        assert_eq!(c.name, "News");
        assert_eq!(c.kind, CategoryKind::Normal);
        assert_eq!(c.error, 0);
        assert!(!c.has_error());
        assert!(!c.is_dynamic_opml());
        assert!(c.attributes.is_empty());
    }

    #[test]
    fn names_are_trimmed_and_whitespace_collapsed() {
        assert_eq!(Category::normalize_name("  News  ").unwrap(), "News");
        assert_eq!(
            Category::normalize_name("World\n  News\t!").unwrap(),
            "World News !"
        );
        assert_eq!(Category::normalize_name("Tech").unwrap(), "Tech");
    }

    #[test]
    fn empty_and_whitespace_only_names_are_rejected() {
        assert!(Category::normalize_name("").is_err());
        assert!(Category::normalize_name("   ").is_err());
        assert!(Category::normalize_name("\n\t ").is_err());
    }

    #[test]
    fn over_long_names_are_rejected() {
        let long = "n".repeat(CATEGORY_NAME_MAX_LEN);
        assert!(Category::normalize_name(&long).is_ok());
        let too_long = "n".repeat(CATEGORY_NAME_MAX_LEN + 1);
        assert!(Category::normalize_name(&too_long).is_err());
    }

    #[test]
    fn error_is_a_timestamp_flag() {
        let mut c = Category::new(1, "x");
        assert!(!c.has_error());
        c.error = 1_700_000_000;
        assert!(c.has_error());
    }
}
