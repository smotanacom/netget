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

/// Four client parameters that are credentials without being called one. Each reached the
/// `open_client` summary and the executor's DEBUG line verbatim: `is_sensitive_key` matched
/// `password` but not `passcode`, nothing in `community`, nothing in `proxy_auth`, and
/// `auth` only as the substring of `auth_token` / `authorization`. A bare `auth` is hidden
/// when it is a string; an `auth` object (Vault's answer) is still walked, as the first test
/// in this file requires.
#[test]
fn credentials_that_are_not_called_one_are_redacted_and_their_describers_are_not() {
    let shown = redact_sensitive(&json!({
        "passcode": "stomp-pass",
        "community": "public",
        "proxy_auth": "user:pw",
        "auth": "alice:s3cret",
        "Auth": "alice:s3cret",
        "nested": {"community_string": "private", "PROXY-AUTH": "u:p"},
        // Describers of a mechanism, not the credential itself.
        "auth_type": "basic",
        "auth_url": "https://idp.example/authorize",
        "authenticated": true,
        "auth_method": "password-less",
        "routing_key": "orders.created",
        "access_key_id": "AKIA-not-a-secret",
        "key_type": "ed25519",
        "secret_access_key": "this one is"
    }));
    for key in [
        "passcode",
        "community",
        "proxy_auth",
        "auth",
        "Auth",
        "secret_access_key",
    ] {
        assert_eq!(shown[key], json!(REDACTED), "{key} must be hidden");
    }
    assert_eq!(shown["nested"]["community_string"], json!(REDACTED));
    assert_eq!(shown["nested"]["PROXY-AUTH"], json!(REDACTED));
    assert_eq!(shown["auth_type"], json!("basic"));
    assert_eq!(shown["auth_url"], json!("https://idp.example/authorize"));
    assert_eq!(shown["authenticated"], json!(true));
    assert_eq!(shown["auth_method"], json!("password-less"));
    assert_eq!(shown["routing_key"], json!("orders.created"));
    assert_eq!(shown["access_key_id"], json!("AKIA-not-a-secret"));
    assert_eq!(shown["key_type"], json!("ed25519"));
}
