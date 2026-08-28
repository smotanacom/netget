//! Does a static handler with an **empty** `actions` array suppress the LLM call?
//!
//! The root `CLAUDE.md` records this as an unexplained observation under "Known systemic
//! issues": with `{"type":"static","actions":[]}` on a `*` pattern, a probe on the `call_llm`
//! error branch fired with a real backend error, apparently proving the model was consulted;
//! changing only that JSON to `[{"type":"wait_for_more"}]` made the probe stop firing. The
//! mechanism was never found, and every candidate was ruled out by inspection.
//!
//! **This matters well beyond one xmpp test.** `src/tui/modal/form.rs` uses exactly the
//! empty-list form for every dashboard-created client's `<proto>_connected` rule, whose entire
//! purpose is to stop the connect event parking or reaching the model. If the observation
//! generalises, that rule does nothing and every dashboard-created client sends its connect
//! event to the LLM.
//!
//! # How this answers it
//!
//! The earlier observation was indirect — a probe on an error branch, in a full-suite run,
//! under load. This asks the question directly: point NetGet at a mock model that **records
//! every call it receives**, and count.
//!
//! A zero on its own would prove nothing (the harness might simply be unreachable), so the
//! negative control runs first and must be non-zero. The three cases share everything except
//! the routing table:
//!
//! | case | routing | expectation |
//! |---|---|---|
//! | control | no handler at all | the model **is** called |
//! | empty | `{"type":"static","actions":[]}` | the model is **not** called |
//! | wait_for_more | `{"type":"static","actions":[{"type":"wait_for_more"}]}` | the model is **not** called |
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test empty_static_handler_test -- --test-threads=100

#![cfg(feature = "tcp")]

mod helpers;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use helpers::mock_builder::MockLlmBuilder;
use helpers::mock_ollama::MockOllamaServer;
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::sync::mpsc;

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..200 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("server #{} never bound a port", id.as_u32());
}

/// Start a TCP server with the given routing, poke it once, and report how many LLM calls
/// the model received.
///
/// The instruction is deliberately non-empty. `ServerForm::create` substitutes a default
/// instruction when `instruction` is `None`, and any non-empty instruction makes
/// `operator_wants_dynamic` true — which is precisely the condition under which the model
/// *would* be consulted. A test that left this empty would measure nothing.
async fn llm_calls_for(routing: Option<serde_json::Value>) -> usize {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            // Unconstrained, so anything that reaches the model matches and is recorded
            // rather than 500-ing into an error path that could be mistaken for silence.
            .on_any()
            .respond_with_actions(serde_json::json!([{
                "type": "send_tcp_data",
                "data": "from-the-model",
                "encoding": "utf8"
            }]))
            .expect_at_least(0)
            .build(),
    )
    .await
    .expect("mock ollama");

    let state = AppState::new_with_options(false, false, mock.base_url());
    state
        .set_llm_client(netget::llm::OllamaClient::new(mock.base_url()))
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let id = ServerForm {
        protocol: "tcp".to_string(),
        port: Some(0),
        instruction: Some("Answer whatever arrives.".to_string()),
        event_handlers: routing.map(|r| r.as_array().unwrap().clone()),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .expect("create tcp server");

    let port = wait_for_port(&state, id).await;

    // One request, then give the server room to make (or not make) a round trip.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(b"ping").expect("write");
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);

    // Poll rather than sleep a fixed amount: a call that is going to happen has happened by
    // the time the read returns or times out, and this only waits longer when it must.
    for _ in 0..100 {
        if mock.call_count().await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let count = mock.call_count().await;
    let _ = state.remove_server(id).await;
    count
}

fn rule(handler: serde_json::Value) -> serde_json::Value {
    serde_json::json!([{ "event_pattern": "*", "handler": handler }])
}

/// The control. Without it, a zero in the other two cases would be indistinguishable from a
/// mock the server never even tried to reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_handler_the_model_is_consulted() {
    let calls = llm_calls_for(None).await;
    assert!(
        calls > 0,
        "no routing at all must reach the model — if this is 0 the harness is not measuring \
         anything and the other two assertions in this file are worthless"
    );
}

/// The claim under test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_static_handler_suppresses_the_llm_call() {
    let calls = llm_calls_for(Some(rule(
        serde_json::json!({"type": "static", "actions": []}),
    )))
    .await;
    assert_eq!(
        calls, 0,
        "a static handler with an empty actions array must answer the event itself. If this \
         fails, the CLAUDE.md observation generalises and src/tui/modal/form.rs's connect rule \
         for every dashboard-created client is a no-op — fix that too, do not just relax this."
    );
}

/// The comparison the original observation drew, so both halves are measured the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_for_more_static_handler_suppresses_the_llm_call() {
    let calls = llm_calls_for(Some(rule(serde_json::json!({
        "type": "static",
        "actions": [{"type": "wait_for_more"}]
    }))))
    .await;
    assert_eq!(
        calls, 0,
        "a static handler naming a real no-op verb must also answer without the model"
    );
}
