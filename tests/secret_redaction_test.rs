//! Credential-named values are redacted wherever NetGet echoes startup parameters or actions.
//!
//! `src/utils/redact.rs` is applied to the `open_client` / `open_server` summary on the status
//! stream and to the executor's per-action DEBUG line. The end-to-end check is in
//! `tests/client/radius/real_server_test.rs`, which asserts the RADIUS shared secret is absent
//! from everything NetGet printed; this file pins the function itself.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test secret_redaction_test

use netget::utils::redact::{redact_sensitive, REDACTED};
use serde_json::json;

#[test]
fn credential_named_keys_are_redacted_at_any_depth_and_nothing_else_is() {
    let shown = redact_sensitive(&json!({
        "secret": "testing123",
        "accounting_port": 1813,
        "nested": {"Bind_Password": "pw", "user": "alice", "list": [{"api_key": "k"}]},
        "shared_secret": null,
        "token": "startup-token",
        "auth": {"client_token": "returned-token", "token_type": "service", "token_policies": ["default"], "token_present": true}
    }));
    assert_eq!(shown["secret"], REDACTED);
    assert_eq!(shown["accounting_port"], 1813);
    assert_eq!(shown["nested"]["Bind_Password"], REDACTED);
    assert_eq!(shown["nested"]["user"], "alice");
    assert_eq!(shown["nested"]["list"][0]["api_key"], REDACTED);
    assert!(
        shown["shared_secret"].is_null(),
        "an absent value is not invented"
    );
    assert!(!shown.to_string().contains("testing123"));
    assert_eq!(shown["token"], REDACTED);
    assert_eq!(shown["auth"]["client_token"], REDACTED);
    assert_eq!(shown["auth"]["token_type"], "service");
    assert_eq!(shown["auth"]["token_policies"], json!(["default"]));
    assert_eq!(shown["auth"]["token_present"], true);
}

#[test]
fn nesting_past_the_bound_is_hidden_rather_than_walked() {
    use netget::utils::redact::MAX_REDACT_DEPTH;
    let mut value = json!("bottom");
    for _ in 0..(MAX_REDACT_DEPTH + 10) {
        value = json!({ "n": value });
    }
    let shown = redact_sensitive(&value).to_string();
    assert!(
        !shown.contains("bottom"),
        "the walk must stop at MAX_REDACT_DEPTH"
    );
    assert!(shown.contains(REDACTED));
}

#[test]
fn bearer_credentials_and_http_auth_headers_are_hidden_without_hiding_usage() {
    let original = json!({
        "access_token": "access credential",
        "refreshToken": "refresh credential",
        "id_token": "identity credential",
        "idToken": "camel case identity credential",
        "authToken": "camel case auth credential",
        "token": "generic credential",
        "headers": {
            "Authorization": "Bearer credential",
            "Proxy-Authorization": "Basic credential",
            "X-API-Key": "API credential",
            "Cookie": "session=credential",
            "Set-Cookie": "session=credential; HttpOnly"
        },
        "input_tokens": 100,
        "max_tokens": 200,
        "token_url": "https://example.test/token"
    });
    let shown = redact_sensitive(&original);
    for key in [
        "access_token",
        "refreshToken",
        "id_token",
        "idToken",
        "authToken",
        "token",
    ] {
        assert_eq!(shown[key], REDACTED, "{key}");
    }
    for value in shown["headers"].as_object().unwrap().values() {
        assert_eq!(value, REDACTED);
    }
    for key in ["input_tokens", "max_tokens", "token_url"] {
        assert_eq!(shown[key], original[key]);
    }
    assert_eq!(
        original["access_token"], "access credential",
        "redaction must not change the credential used on the wire"
    );
}
