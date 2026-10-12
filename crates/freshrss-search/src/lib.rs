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
pub mod error;
pub mod normalize;
pub mod regex;

pub use ast::{DateField, DateRange, Expr, IdSet, Op, Pred, TextPat};
pub use error::SearchError;
pub use normalize::{fold, ilike};
pub use regex::{Flags, Pattern};
