//! The `attributes` JSON blob shared by `user`, `category`, `feed`, `entry` and `tag`.
//!
//! FreshRSS models this as a dynamic map with typed accessors (`AttributesTrait`), and that shape
//! is the right one here too: the set of keys differs per table, most rows have only one or two,
//! and new keys must survive a round-trip through code that has never heard of them. So this is a
//! map wrapper with typed accessors rather than a struct with a field per key — a struct with
//! `#[serde(flatten)]` would still drop keys on any code path that rebuilt it from typed fields.
//!
//! Keys are serialized in sorted order (`serde_json::Map` is a `BTreeMap` unless `preserve_order`
//! is enabled) so stored JSON and test snapshots are stable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Key names, spelled once. FreshRSS uses these exact strings, and OPML import/export plus the
/// Google-Reader-compatible paths depend on them matching.
pub mod keys {
    /// Feed: GUID policy override. See `freshrss_feed::guid::UnicityCriteria`.
    pub const UNICITY_CRITERIA: &str = "unicityCriteria";
    /// Feed: set when the user chose the GUID policy explicitly, so auto-degradation is skipped.
    pub const UNICITY_CRITERIA_FORCED: &str = "unicityCriteriaForced";
    /// Feed: SHA-1 of the last fetched body, with volatile elements stripped.
    pub const SIMPLEPIE_HASH: &str = "SimplePieHash";
    /// Feed: the validators from the last successful fetch, replayed as a conditional GET.
    ///
    /// These keys are ours rather than FreshRSS's: it keeps them in the HTTP cache file beside the
    /// cached body, and this port has no cache files.
    pub const ETAG: &str = "etag";
    pub const LAST_MODIFIED: &str = "lastModified";
    /// Feed: why the last refresh failed, in words.
    ///
    /// Ours rather than FreshRSS's, which stores only the timestamp in `feed.error` and so can tell
    /// the user *that* a feed is broken but not *why*. The problems dialog needs the reason: "410
    /// Gone" and "certificate expired" call for opposite responses, and both look identical as a
    /// number. Cleared by any successful refresh.
    pub const LAST_ERROR: &str = "lastError";
    /// Feed/category/user: retention settings.
    pub const ARCHIVING: &str = "archiving";
    /// Feed/category/tag/user: `[{search, actions}]`.
    pub const FILTERS: &str = "filters";
    /// Feed: per-request HTTP timeout in seconds.
    pub const TIMEOUT: &str = "timeout";
    /// Feed: disable TLS verification for this feed.
    pub const SSL_VERIFY: &str = "ssl_verify";
    /// Feed: allow-listed request overrides (proxy, headers, user agent…).
    pub const CURL_PARAMS: &str = "curl_params";
    /// Feed: XPath map for kinds 10/15/20/35.
    pub const XPATH: &str = "xpath";
    /// Feed: dot-notation map for kinds 30/35.
    pub const JSON_DOTNOTATION: &str = "json_dotnotation";
    /// Feed: XPath expression that extracts embedded JSON from an HTML page (kind 35).
    pub const XPATH_TO_JSON: &str = "xPathToJson";
    /// Feed: discovered icon URL.
    pub const FEED_ICON_URL: &str = "feedIconUrl";
    /// Feed/category: dynamic OPML subscription URL.
    pub const OPML_URL: &str = "opml_url";
    /// Entry: pre-scrape content, kept so `hash` stays stable across full-text extraction.
    pub const ORIGINAL_CONTENT: &str = "original_content";
    /// Entry: the article body scraped from the article's own page on demand, from the reading
    /// pane's "full article" button. Kept beside `content` rather than in it so an upstream edit
    /// detected at the next refresh cannot silently discard what the reader asked for, and so the
    /// reader can switch back to the feed's own text.
    pub const FULL_CONTENT: &str = "full_content";
    /// Entry: `[{url, type, medium, length, title, description, height, width}]`.
    pub const ENCLOSURES: &str = "enclosures";
    /// Entry: `{url, height, width, time}`.
    pub const THUMBNAIL: &str = "thumbnail";
    /// Feed/category/user: mark-as-read policies.
    pub const READ_UPON_RECEPTION: &str = "read_upon_reception";
    pub const READ_UPON_GONE: &str = "read_upon_gone";
    pub const KEEP_MAX_N_UNREAD: &str = "keep_max_n_unread";
    pub const MARK_UPDATED_ARTICLE_UNREAD: &str = "mark_updated_article_unread";
    pub const READ_WHEN_SAME_TITLE_IN_FEED: &str = "read_when_same_title_in_feed";
    pub const READ_WHEN_SAME_TITLE_IN_CATEGORY: &str = "read_when_same_title_in_category";
    pub const READ_WHEN_SAME_GUID_IN_CATEGORY: &str = "read_when_same_guid_in_category";
    /// Feed: CSS selector removing boilerplate from scraped full text.
    pub const PATH_ENTRIES_FILTER: &str = "path_entries_filter";
    /// Feed: BooleanSearch conditions gating full-text extraction.
    pub const PATH_ENTRIES_CONDITIONS: &str = "path_entries_conditions";
    /// Feed: what to do with scraped content — `replace` (default), `append` or `prepend`.
    pub const CONTENT_ACTION: &str = "content_action";
}

/// A JSON object of extension attributes. Empty is stored as SQL NULL, not `"{}"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes(Map<String, Value>);

impl Attributes {
    pub fn new() -> Self {
        Self(Map::new())
    }

    /// Parses a stored column value. `NULL`, empty and malformed all yield an empty set — the same
    /// tolerance as `FreshRSS_AttributesTrait::fromArray`, which drops invalid JSON rather than
    /// failing the row.
    pub fn from_db(raw: Option<&str>) -> Self {
        let Some(text) = raw.filter(|t| !t.trim().is_empty()) else {
            return Self::new();
        };
        match serde_json::from_str::<Value>(text) {
            // Non-string keys cannot occur in JSON, but an array or scalar can; ignore those.
            Ok(Value::Object(map)) => Self(map),
            _ => Self::new(),
        }
    }

    /// The column value to store, or `None` for SQL NULL when there is nothing to persist.
    pub fn to_db(&self) -> Option<String> {
        if self.0.is_empty() {
            None
        } else {
            Some(self.to_json())
        }
    }

    pub fn to_json(&self) -> String {
        Value::Object(self.0.clone()).to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    // --- typed accessors ---------------------------------------------------

    /// The raw value, for keys with no typed accessor yet.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// Reads a boolean, accepting the JSON `true`/`false` only. A string `"true"` is not a
    /// boolean; FreshRSS's `attributeBoolean` is stricter than PHP's truthiness for the same
    /// reason.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.0.get(key).and_then(Value::as_bool)
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_i64)
    }

    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.0
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    }

    pub fn get_object(&self, key: &str) -> Option<&Map<String, Value>> {
        self.0.get(key).and_then(Value::as_object)
    }

    /// Deserializes a key into a typed value, reporting a decode error as `None` so one malformed
    /// attribute cannot break loading the row.
    pub fn get_typed<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Option<T> {
        self.0.get(key).and_then(|v| T::deserialize(v).ok())
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn set_typed<T: Serialize>(
        &mut self,
        key: &str,
        value: &T,
    ) -> serde_json::Result<&mut Self> {
        let json = serde_json::to_value(value)?;
        self.0.insert(key.to_string(), json);
        Ok(self)
    }

    /// Removes a key. Passing `None` to the typed setters routes here, matching FreshRSS's
    /// `_attribute($key, null)` convention.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0.remove(key)
    }

    pub fn set_str(&mut self, key: &str, value: Option<impl Into<String>>) -> &mut Self {
        match value {
            Some(v) => {
                self.set(key, v.into());
            }
            None => {
                self.remove(key);
            }
        }
        self
    }

    pub fn set_bool(&mut self, key: &str, value: Option<bool>) -> &mut Self {
        match value {
            Some(v) => {
                self.set(key, v);
            }
            None => {
                self.remove(key);
            }
        }
        self
    }

    /// Merges another set over this one. Used to layer feed → category → user defaults.
    pub fn merge_from(&mut self, other: &Attributes) -> &mut Self {
        for (k, v) in &other.0 {
            self.0.insert(k.clone(), v.clone());
        }
        self
    }

    /// Returns the first key present in `candidates`, for the settings FreshRSS stores under
    /// several historical names.
    pub fn first_str<'a>(&'a self, candidates: &[&'a str]) -> Option<&'a str> {
        candidates.iter().find_map(|k| self.get_str(k))
    }

    /// A stable, sorted view for test assertions and the `/search/preview` debug output.
    pub fn to_sorted_map(&self) -> BTreeMap<String, Value> {
        self.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

impl Serialize for Attributes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Attributes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Accept `null` as "no attributes" so API clients can send either.
        Ok(
            match Option::<Map<String, Value>>::deserialize(deserializer)? {
                Some(map) => Self(map),
                None => Self::new(),
            },
        )
    }
}

impl From<Map<String, Value>> for Attributes {
    fn from(map: Map<String, Value>) -> Self {
        Self(map)
    }
}

impl From<Attributes> for Value {
    fn from(a: Attributes) -> Self {
        Value::Object(a.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_round_trips_to_sql_null() {
        let a = Attributes::new();
        assert!(a.is_empty());
        assert_eq!(a.to_db(), None);
        assert!(Attributes::from_db(None).is_empty());
        assert!(Attributes::from_db(Some("")).is_empty());
        assert!(Attributes::from_db(Some("   ")).is_empty());
    }

    #[test]
    fn non_empty_round_trips() {
        let mut a = Attributes::new();
        a.set(keys::TIMEOUT, 30);
        let stored = a.to_db().expect("non-empty stores");
        assert_eq!(Attributes::from_db(Some(&stored)), a);
    }

    #[test]
    fn malformed_json_is_tolerated_rather_than_failing_the_row() {
        assert!(Attributes::from_db(Some("{not json")).is_empty());
        assert!(Attributes::from_db(Some("[1,2,3]")).is_empty());
        assert!(Attributes::from_db(Some("\"a string\"")).is_empty());
        assert!(Attributes::from_db(Some("null")).is_empty());
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        // The whole point of the map shape: code that only knows `timeout` must not destroy
        // `xpath`, `curl_params` or anything a future version adds.
        let raw = r#"{"timeout":30,"xpath":{"item":"//div"},"some_future_key":{"nested":[1,2]}}"#;
        let a = Attributes::from_db(Some(raw));
        assert_eq!(a.get_i64(keys::TIMEOUT), Some(30));
        assert!(a.contains("some_future_key"));
        let back = Attributes::from_db(a.to_db().as_deref());
        assert_eq!(back, a);
        assert_eq!(
            back.get("some_future_key").and_then(|v| v.get("nested")),
            Some(&json!([1, 2]))
        );
    }

    #[test]
    fn typed_accessors_read_the_right_json_types() {
        let a = Attributes::from_db(Some(
            r#"{"s":"text","b":true,"i":42,"u":7,"o":{"k":"v"},"wrong_type":"not-a-bool"}"#,
        ));
        assert_eq!(a.get_str("s"), Some("text"));
        assert_eq!(a.get_bool("b"), Some(true));
        assert_eq!(a.get_i64("i"), Some(42));
        assert_eq!(a.get_u32("u"), Some(7));
        assert_eq!(
            a.get_object("o").and_then(|m| m.get("k")),
            Some(&json!("v"))
        );
        assert_eq!(a.get_bool("wrong_type"), None, "a string is not a boolean");
        assert_eq!(a.get_str("missing"), None);
    }

    #[test]
    fn setting_none_removes_the_key() {
        let mut a = Attributes::new();
        a.set(keys::READ_UPON_GONE, true);
        assert!(a.contains(keys::READ_UPON_GONE));
        a.set_bool(keys::READ_UPON_GONE, None);
        assert!(!a.contains(keys::READ_UPON_GONE));
        assert!(a.is_empty(), "removing the last key empties the set");
        assert_eq!(a.to_db(), None);
    }

    #[test]
    fn typed_structs_serialize_and_deserialize() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Archiving {
            keep_max: u32,
            keep_favourites: bool,
        }

        let mut a = Attributes::new();
        a.set_typed(
            keys::ARCHIVING,
            &Archiving {
                keep_max: 50,
                keep_favourites: true,
            },
        )
        .expect("serializable");

        let back: Option<Archiving> = a.get_typed(keys::ARCHIVING);
        assert_eq!(
            back,
            Some(Archiving {
                keep_max: 50,
                keep_favourites: true
            })
        );
    }

    #[test]
    fn a_malformed_typed_value_is_none_not_a_panic() {
        let a = Attributes::from_db(Some(r#"{"archiving":"definitely not an object"}"#));
        #[derive(Debug, Deserialize)]
        struct Archiving {
            #[allow(dead_code)]
            keep_max: u32,
        }
        assert!(a.get_typed::<Archiving>(keys::ARCHIVING).is_none());
    }

    #[test]
    fn merge_layers_later_values_over_earlier_ones() {
        let mut base = Attributes::new();
        base.set(keys::TIMEOUT, 10);
        base.set(keys::SSL_VERIFY, true);

        let mut over = Attributes::new();
        over.set(keys::TIMEOUT, 45);

        base.merge_from(&over);
        assert_eq!(base.get_i64(keys::TIMEOUT), Some(45));
        assert_eq!(
            base.get_bool(keys::SSL_VERIFY),
            Some(true),
            "keys absent from the override are preserved"
        );
    }

    #[test]
    fn first_str_resolves_legacy_key_names() {
        let a = Attributes::from_db(Some(r#"{"new_name":"x"}"#));
        assert_eq!(a.first_str(&["old_name", "new_name"]), Some("x"));
        assert_eq!(a.first_str(&["missing", "also_missing"]), None);
    }

    #[test]
    fn serialization_is_a_json_object_and_null_is_accepted() {
        let mut a = Attributes::new();
        a.set("k", "v");
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"k":"v"}"#);

        let from_null: Attributes = serde_json::from_str("null").expect("null means none");
        assert!(from_null.is_empty());
        let from_obj: Attributes = serde_json::from_str(r#"{"k":"v"}"#).unwrap();
        assert_eq!(from_obj, a);
    }

    #[test]
    fn keys_are_sorted_for_stable_output() {
        let a = Attributes::from_db(Some(r#"{"z":1,"a":2,"m":3}"#));
        assert_eq!(a.to_json(), r#"{"a":2,"m":3,"z":1}"#);
        assert_eq!(a.keys().collect::<Vec<_>>(), vec!["a", "m", "z"]);
    }
}
