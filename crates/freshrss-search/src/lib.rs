//! FreshRSS's BooleanSearch query language.
//!
//! One parse tree drives two backends that must never disagree:
//!
//! ```text
//!            query text
//!                │
//!            lexer.rs        real tokenizer (no string surgery)
//!                │
//!            parser.rs       recursive descent; `!`/`-` at the unary level
//!                │
//!             ast.rs  ◄── expand.rs   `S:`/`search:` saved queries become subtrees
//!                │
//!    ┌───────────┼──────────────┬───────────────┐
//!  sql.rs     matcher.rs     render.rs     visibility.rs
//! ```
//!
//! The AST stores **plaintext** only. Every comparison goes through [`normalize::Text`], which holds
//! a value both as stored and accent-folded: a substring needle is matched against the folded form, a
//! regex against the stored one. `sql.rs` and `matcher.rs` make that same choice, so the two backends
//! agree by construction — and `tests/search_equivalence.rs` in the server crate checks that they do.

pub mod ast;
pub mod dateinterval;
pub mod error;
pub mod lexer;
pub mod matcher;
pub mod normalize;
pub mod parser;
pub mod regex;
pub mod render;
pub mod sql;
pub mod visibility;

pub use ast::{DateField, DateRange, Expr, IdSet, Op, Pred, QueryRef, TextPat};
pub use error::SearchError;
pub use lexer::{tokenize, Token, Value};
pub use matcher::{matches, SearchTarget};
pub use normalize::{fold, ieq, ilike, Text};
pub use parser::parse;
pub use regex::{Flags, Pattern};
pub use render::{enforce, prepend, remove, render, round_trip};
pub use sql::{compile, Fragment, Param, SqlCtx};
pub use visibility::min_priority;
