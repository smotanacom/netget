//! An oversized request body is refused, not buffered — and refusing it is not a token.
//!
//! Every OAuth2 endpoint here reads a form body and then embeds it in an LLM prompt. Until
//! this bound existed the read was `req.into_body().collect()`, which has no limit at all:
//! `Incoming` buffers exactly as much as the peer chooses to send, so one unauthenticated
//! `POST /token` could take the whole process's memory before any credential was looked at.
//! `POST /introspect` and `POST /revoke` are the same shape, and neither needs a client id.
//!
//! The second half of each assertion is the part that matters more. Refusing a body is a
//! *failure to process the request*, so it must land on the fail-closed side of every
//! endpoint's contract: no authorization code, no access token, and never `active: true`. A
//! bound that refused the read and then fell through to a permissive default would be worse
//! than no bound at all.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features oauth2 \
//!       --test server -- --test-threads=100 oauth2::body_limit

#![cfg(all(test, feature = "oauth2"))]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use serde_json::Value;
use std::time::Duration;

/// The server's own cap. Kept as a literal rather than imported so the test fails if the
/// constant is quietly raised: a body limit that tracks whatever the code says is not a limit.
const SERVER_LIMIT: usize = 1024 * 1024;

/// A server whose model answers nothing, so any endpoint that got as far as the LLM would
/// fail closed there instead. That keeps the assertions about the *body limit* rather than
/// about what a model happened to say.
fn startup_only(prompt: &str) -> NetGetConfig {
    NetGetConfig::new_no_scripts(prompt).with_mock(|mock| {
        mock.on_instruction_containing("Open oauth2")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "OAuth2",
                    "instruction": "OAuth2 authorization server"
                }
            ]))
            .expect_calls(1)
            .and()
    })
}

/// A form body comfortably past the cap. `grant_type` is real so that, had the body been
/// accepted, the request would have been a well-formed one — the refusal is about size.
fn oversized_form(key: &str) -> String {
    format!("{key}={}", "A".repeat(SERVER_LIMIT + 64 * 1024))
}

async fn post_form(url: &str, body: String) -> E2EResult<(u16, String)> {
    let client = reqwest::Client::new();
    let response = tokio::time::timeout(
        Duration::from_secs(25),
        client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| "No OAuth2 response within 25s — an oversized body must be refused promptly")??;

    let status = response.status().as_u16();
    let text = response.text().await?;
    println!("{url} -> {status} {text}");
    Ok((status, text))
}

/// `/token`: refused, and no token.
#[tokio::test]
async fn test_oauth2_token_refuses_an_oversized_body_without_issuing_one() -> E2EResult<()> {
    let server = start_netget_server(startup_only("Open oauth2 on port {AVAILABLE_PORT}.")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (status, text) = post_form(
        &format!("http://127.0.0.1:{}/token", server.port),
        oversized_form("code"),
    )
    .await?;

    // Exactly 413, not merely "some failure": with the bound removed the request is read
    // in full, reaches the model, and fails closed at 400/500 instead — which a looser
    // assertion would accept, leaving the bound untested.
    assert_eq!(
        status, 413,
        "an oversized body must be refused before it is read, got {status}: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    assert!(
        body["access_token"].is_null(),
        "a refused request must never carry a token: {text}"
    );
    assert!(
        body["refresh_token"].is_null(),
        "a refused request must never carry a refresh token: {text}"
    );

    // The process is still serving: the point of the bound is that the oversized request
    // costs at most the cap and the server survives it.
    let (status, _) = post_form(
        &format!("http://127.0.0.1:{}/token", server.port),
        "grant_type=authorization_code&code=abc".to_string(),
    )
    .await?;
    assert!(
        (400..600).contains(&status),
        "the server must still be answering after refusing an oversized body, got {status}"
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

/// `/introspect`: refused, and never `active: true`.
///
/// This is the endpoint where a permissive fallback did the most damage historically — the
/// hardcoded `{"active": true}` validated every bearer token in existence — so the assertion
/// is on `active` specifically rather than only on the status.
#[tokio::test]
async fn test_oauth2_introspect_refuses_an_oversized_body_without_validating() -> E2EResult<()> {
    let server = start_netget_server(startup_only("Open oauth2 on port {AVAILABLE_PORT}.")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (status, text) = post_form(
        &format!("http://127.0.0.1:{}/introspect", server.port),
        oversized_form("token"),
    )
    .await?;

    assert_eq!(
        status, 413,
        "an oversized body must be refused before it is read, got {status}: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    assert_ne!(
        body["active"].as_bool(),
        Some(true),
        "a token nobody looked at must never introspect as active: {text}"
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

/// `/revoke`: refused with a non-200.
///
/// RFC 7009 §2.2 fixes the *success* reply at 200 whatever the token was, which makes it
/// tempting to answer 200 here too. It would be wrong for the same reason the LLM-failure
/// path is: nothing processed the request, so the token still exists, and §2.2.1 has the
/// client retry only if the answer is not a success.
#[tokio::test]
async fn test_oauth2_revoke_refuses_an_oversized_body_without_claiming_success() -> E2EResult<()> {
    let server = start_netget_server(startup_only("Open oauth2 on port {AVAILABLE_PORT}.")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (status, _text) = post_form(
        &format!("http://127.0.0.1:{}/revoke", server.port),
        oversized_form("token"),
    )
    .await?;

    assert_ne!(
        status, 200,
        "a 200 tells the client the token is gone; nothing read the request"
    );
    assert_eq!(
        status, 413,
        "an oversized body must be refused before it is read, got {status}"
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
