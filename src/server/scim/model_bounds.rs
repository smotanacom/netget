//! SCIM limits and message shapes shared by the service and the client.
use serde_json::{json, Value};

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Largest page Rust returns; ServiceProviderConfig advertises it as filter.maxResults.
pub const MAX_RESULTS: usize = 200;
pub const MAX_PATCH_OPERATIONS: usize = 64;

pub const SCIM_TYPES: &[&str] = &[
    "invalidFilter",
    "tooMany",
    "uniqueness",
    "mutability",
    "invalidSyntax",
    "invalidPath",
    "noTarget",
    "invalidValue",
    "invalidVers",
    "sensitive",
];

pub fn budget_ok(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_BODY_BYTES, 100_000, 32)
}

/// RFC 7644 §3.12: `status` is a string.
pub fn error_body(status: u16, scim_type: Option<&str>, detail: &str) -> Value {
    let mut e =
        json!({"schemas": [super::schema::ERROR], "status": status.to_string(), "detail": detail});
    if let Some(t) = scim_type {
        e["scimType"] = json!(t);
    }
    e
}
