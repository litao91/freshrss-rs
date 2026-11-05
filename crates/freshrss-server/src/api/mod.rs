//! The HTTP API: handlers, DTOs, extractors and the error envelope.

pub mod cookie;
pub mod dto;
pub mod error;
pub mod extract;
pub mod greader;
pub mod v1;

pub use error::{ApiError, ApiResult};
