//! Errors raised while parsing or compiling a search query.
//!
//! Every variant here is a *caller* mistake, so the API layer maps them all to HTTP 400 with
//! the code `bad_search`.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SearchError {
    #[error("search query is {len} characters, the limit is {max}")]
    TooLong { len: usize, max: usize },

    #[error("parentheses nested {depth} deep, the limit is {max}")]
    TooDeep { depth: usize, max: usize },

    #[error("unbalanced parenthesis at offset {offset}")]
    UnbalancedParen { offset: usize },

    #[error("unexpected end of query, expected a term after '{token}'")]
    UnexpectedEnd { token: String },

    #[error("unexpected '{token}' at offset {offset}")]
    UnexpectedToken { token: String, offset: usize },

    #[error("invalid regular expression {pattern:?}: {reason}")]
    BadRegex { pattern: String, reason: String },

    #[error("invalid date interval {value:?}: {reason}")]
    BadDateInterval { value: String, reason: String },

    #[error("'{field}:' expects {expected}")]
    BadOperand {
        field: String,
        expected: &'static str,
    },

    #[error("saved search {name:?} refers to itself")]
    QueryCycle { name: String },

    #[error("saved search {name:?} does not exist")]
    QueryNotFound { name: String },

    #[error("saved-search expansion exceeded the budget of {max} expansions")]
    BudgetExceeded { max: usize },
}

impl SearchError {
    /// True when the query is merely too complex rather than malformed. Used by the API to
    /// phrase the 400 without implying a syntax error.
    pub fn is_limit(&self) -> bool {
        matches!(
            self,
            SearchError::TooLong { .. } | SearchError::TooDeep { .. } | SearchError::BudgetExceeded { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_errors_are_distinguished_from_syntax_errors() {
        assert!(SearchError::TooLong { len: 1, max: 0 }.is_limit());
        assert!(SearchError::TooDeep { depth: 1, max: 0 }.is_limit());
        assert!(!SearchError::UnbalancedParen { offset: 0 }.is_limit());
    }

    #[test]
    fn messages_are_user_presentable() {
        let msg = SearchError::BadRegex {
            pattern: "([a".into(),
            reason: "unclosed group".into(),
        }
        .to_string();
        assert!(msg.contains("([a"), "{msg}");
        assert!(msg.contains("unclosed group"), "{msg}");
    }
}
