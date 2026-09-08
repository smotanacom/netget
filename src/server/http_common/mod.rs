//! Shared HTTP/HTTP2 implementation components
//!
//! This module contains shared logic used by both HTTP/1.1 and HTTP/2 implementations
//! to avoid code duplication while maintaining protocol-specific boundaries.

pub mod actions;
pub mod handler;

pub use actions::{execute_http_response_action, HttpResponseData};
pub use handler::{
    build_response, extract_request_data, BodyTooLarge, RequestData, RequestFilter,
    MAX_REQUEST_BODY_BYTES,
};
