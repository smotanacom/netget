//! CPU-only checks for test harness selection and deadlines. No models are contacted.
#![allow(dead_code, unused_imports)]
mod helpers;

use helpers::common::{live_ollama_opt_in, retry_with_backoff};
use std::time::{Duration, Instant};

#[test]
fn real_inference_requires_an_explicit_affirmative_opt_in() {
    for value in [
        None,
        Some(""),
        Some("0"),
        Some("false"),
        Some("no"),
        Some("off"),
        Some("typo"),
    ] {
        assert!(
            !live_ollama_opt_in(value),
            "{value:?} must not enable real inference"
        );
    }
    for value in ["1", "true", "yes", "on", " TRUE "] {
        assert!(live_ollama_opt_in(Some(value)), "{value}");
    }
}

#[test]
fn binary_resolution_uses_the_cargo_built_executable() {
    // A deliberate runtime override remains supported; only assert the compile
    // time default when no caller supplied one.
    if std::env::var("CARGO_BIN_EXE_netget").is_err() {
        assert_eq!(
            helpers::common::get_netget_binary_path().unwrap(),
            std::path::PathBuf::from(env!("CARGO_BIN_EXE_netget"))
        );
    }
}

#[tokio::test]
async fn retry_timeout_bounds_a_condition_that_never_completes() {
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        retry_with_backoff(
            || std::future::pending::<Result<(), std::io::Error>>(),
            Duration::from_millis(1),
            Duration::from_millis(10),
            Duration::from_millis(30),
        ),
    )
    .await
    .expect("the helper's own deadline must end a pending attempt");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("condition did not complete"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn retry_timeout_also_bounds_the_backoff_sleep() {
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        retry_with_backoff(
            || async { Err::<(), _>(std::io::Error::other("not ready")) },
            Duration::from_secs(20),
            Duration::from_secs(30),
            Duration::from_millis(30),
        ),
    )
    .await
    .expect("backoff must not sleep past the total deadline");
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn retry_still_returns_success_after_transient_failures() {
    let mut attempts = 0;
    let value = retry_with_backoff(
        || {
            attempts += 1;
            let attempt = attempts;
            async move {
                if attempt < 3 {
                    Err(std::io::Error::other("not ready"))
                } else {
                    Ok(42)
                }
            }
        },
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(value, 42);
    assert_eq!(attempts, 3);
}

#[path = "../examples/external_protocol/src/lib.rs"]
mod external_echo;

#[test]
fn external_protocol_example_tracks_current_traits_and_action_encoding() {
    use netget::llm::actions::protocol_trait::{ActionResult, Protocol, Server};
    let protocol = external_echo::EchoProtocol::new();
    protocol
        .get_startup_examples()
        .validate(protocol.protocol_name())
        .unwrap();
    let ActionResult::Output(bytes) = protocol
        .execute_action(serde_json::json!({"type":"send_echo_data", "data":"hello"}))
        .unwrap()
    else {
        panic!("expected output")
    };
    assert_eq!(bytes, b"hello");
    assert!(protocol
        .execute_action(serde_json::json!({"type":"send_echo_data"}))
        .is_err());
}

#[tokio::test]
#[allow(deprecated)]
async fn external_echo_stops_listener_and_connected_peers() {
    use netget::llm::actions::protocol_trait::Server;
    use netget::state::{app_state::AppState, server::ServerInstance, ServerId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = std::sync::Arc::new(AppState::new_with_options(
        false,
        "http://127.0.0.1:1".into(),
    ));
    let id = state
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "Echo".into(),
            String::new(),
        ))
        .await;
    let (status_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = netget::protocol::SpawnContext {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        mac_address: None,
        interface: None,
        host: Some("127.0.0.1".into()),
        port: Some(0),
        llm_client: netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        state: state.clone(),
        status_tx,
        server_id: id,
        startup_params: None,
    };
    let address = external_echo::EchoProtocol::new().spawn(ctx).await.unwrap();
    let mut peer = tokio::net::TcpStream::connect(address).await.unwrap();
    peer.write_all(b"echo").await.unwrap();
    let mut echoed = [0; 4];
    tokio::time::timeout(Duration::from_secs(3), peer.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echoed, b"echo");
    state.remove_server(id).await;
    let n = tokio::time::timeout(Duration::from_secs(3), peer.read(&mut echoed))
        .await
        .expect("server removal must stop peer tasks")
        .unwrap();
    assert_eq!(n, 0);
    // EOF proves the registered task cancellation ran; the accept task is
    // cancelled by the same removal before another event loop turn.
    tokio::task::yield_now().await;
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}
