//! NetGet's Anthropic client against llama.cpp's llama-server (b11500, its Anthropic-compatible
//! /v1/messages) running ggml-org's 260K-parameter test model, failing rather than skipping when
//! absent. The text is a tiny model's, so it is asserted by shape — and the chain's second
//! request, built from the first answer, is found in the server's own request log.
use super::session_test::{chain_handlers, client_with_status, executed, send, wait_for};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/anthropic/install_peers.py <root> and export what it prints")
    })
}

#[tokio::test]
async fn netget_against_llama_server() {
    let bin = env_path("NETGET_LLAMA_SERVER");
    let model = env_path("NETGET_LLAMA_MODEL");
    let server = RealServer::builder(
        bin.to_str().unwrap(),
        InstallHint {
            brew: "llama.cpp (or tests/server/anthropic/install_peers.py)",
            apt: "tests/server/anthropic/install_peers.py (builds llama-server b11500)",
        },
    )
    .args([
        "-m",
        model.to_str().unwrap(),
        "--host",
        "127.0.0.1",
        "--port",
        "{port}",
        "-c",
        "512",
        "-n",
        "64",
        "--seed",
        "42",
        "-a",
        "tinyllama-2",
        "--verbose",
    ])
    .ready_when_log_matches("listening on http://127.0.0.1")
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
    .expect("start llama-server");
    let params = json!({"model":"tinyllama-2"});
    let (state, id, _s) = client_with_status(
        server.addr(),
        chain_handlers("Once upon a time"),
        Some(params),
    )
    .await;
    let r = wait_for(&state, id, 4).await;
    let first = &r[0]["message"];
    assert_eq!(r[0]["status"], 200, "{r:?}\n{}", server.log());
    let text = first["text"].as_str().unwrap();
    assert!(!text.is_empty(), "{r:?}");
    assert_eq!(first["model"], "tinyllama-2");
    // Sixteen tokens asked for, sixteen generated: the model stops on max_tokens.
    assert_eq!(first["stop_reason"], "max_tokens", "{r:?}");
    assert_eq!(first["usage"]["output_tokens"], 16, "{r:?}");
    // The second request carried the first answer: the server's own log shows it arrived.
    let sent = serde_json::to_string(&format!("again: {text}")).unwrap();
    let sent = &sent[1..sent.len() - 1];
    assert!(
        server
            .log()
            .lines()
            .any(|l| l.contains("converted request") && l.contains(sent)),
        "llama-server never received {sent:?}\n{}",
        server.log()
    );
    let streamed = &r[1];
    assert_eq!(streamed["status"], 200, "{r:?}");
    assert_eq!(streamed["streamed"]["message_start"], 1, "{r:?}");
    assert_eq!(streamed["streamed"]["message_stop"], 1, "{r:?}");
    assert!(
        streamed["streamed"]["content_block_delta"]
            .as_u64()
            .unwrap()
            >= 1,
        "{r:?}"
    );
    assert!(
        !streamed["message"]["text"].as_str().unwrap().is_empty(),
        "{r:?}"
    );
    assert!(r[2]["input_tokens"].as_u64().unwrap() > 0, "{r:?}");
    assert!(
        r[3]["models"]
            .as_array()
            .unwrap()
            .contains(&json!("tinyllama-2")),
        "{r:?}"
    );
    // llama-server's own refusals, read from its error envelope.
    let image = json!({"type":"anthropic_create_message","max_tokens":4,"messages":[{"role":"user","content":[
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}]});
    let v = executed(send(&state, id, image).await);
    assert_eq!(v["status"], 500, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("image input is not supported"),
        "{v}"
    );
    let v = executed(
        send(
            &state,
            id,
            json!({"type":"anthropic_get_model","model_id":"tinyllama-2"}),
        )
        .await,
    );
    assert_eq!(
        (v["status"].as_u64(), &v["error"]["type"]),
        (Some(404), &json!("not_found_error")),
        "{v}"
    );
}
