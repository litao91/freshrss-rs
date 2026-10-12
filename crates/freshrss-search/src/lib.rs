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
//! The AST stores **plaintext** only. Normalization ([`normalize::fold`]) happens inside `sql.rs`
//! before binding and inside `matcher.rs` before comparing — both calling the same function, so
//! the two backends agree by construction.

pub mod ast;
pub mod dateinterval;
pub mod error;
pub mod lexer;
pub mod matcher;
pub mod normalize;
pub mod parser;
pub mod regex;
pub mod sql;

pub use ast::{DateField, DateRange, Expr, IdSet, Op, Pred, QueryRef, TextPat};
pub use error::SearchError;
pub use lexer::{tokenize, Token, Value};
pub use matcher::{matches, SearchTarget};
pub use normalize::{fold, ilike};
pub use parser::parse;
pub use regex::{Flags, Pattern};
pub use sql::{compile, Fragment, Param, SqlCtx};
