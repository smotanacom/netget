//! The HTTP/2 client's browser transport — `FetchClient::transport(..).http2_prior_knowledge()`,
//! hyper's `client::conn::http2` over a plain TCP stream — driven natively against NetGet's own
//! HTTP/2 server (the `h2` crate), next to the reqwest backend the client uses natively. Zero
//! LLM calls: the server answers every request through a `*` static handler.
//!
//! Both backends must read the same answer: status, body, the server's header, and HTTP/2 as
//! the version — the last is what shows the transport spoke h2c with prior knowledge. With
//! `http2_prior_knowledge()` made a no-op the transport writes HTTP/1.1, NetGet's HTTP/2
//! server answers in HTTP/2 frames, and the exchange fails ("invalid HTTP version parsed"),
//! which is how this test was checked. A POST body goes out too, and an `https://` URL is
//! refused with the reason before anything is dialled.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features http2 --test client -- http2::h2_transport --test-threads=100

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::client::http_fetch::transport::HTTPS_UNSUPPORTED;
use netget::client::http_fetch::FetchClient;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::sync::mpsc;

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..1_000 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("server #{} never bound a port", id.as_u32());
}

async fn http2_server(state: &AppState) -> u16 {
    let (tx, _rx) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "http2".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "*",
            "handler": {"type": "static", "actions": [{
                "type": "send_http2_response",
                "status": 201,
                "headers": {"content-type": "text/plain", "x-h2-marker": "static"},
                "body": "answered over h2"
            }]}
        })]),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create http2 server");
    wait_for_port(state, id).await
}

#[tokio::test]
async fn the_h2_transport_reads_what_reqwest_reads_from_netgets_http2_server() {
    let state = new_state().await;
    let port = http2_server(&state).await;
    let backends = [
        (
            "reqwest",
            FetchClient::from_reqwest(
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .http2_prior_knowledge()
                    .no_proxy()
                    .build()
                    .unwrap(),
            ),
        ),
        (
            "transport",
            FetchClient::transport(Duration::from_secs(10)).http2_prior_knowledge(),
        ),
    ];
    for (name, client) in backends {
        for (method, body) in [("GET", None), ("POST", Some("a posted body"))] {
            let mut request = client.request(
                method.parse().unwrap(),
                &format!("http://127.0.0.1:{port}/greet?who=h2"),
            );
            if let Some(body) = body {
                request = request.body(body);
            }
            let response = request
                .send()
                .await
                .unwrap_or_else(|e| panic!("{name} {method}: {e:#}"));
            assert_eq!(
                response.version(),
                reqwest::Version::HTTP_2,
                "{name} {method}"
            );
            assert_eq!(response.status().as_u16(), 201, "{name} {method}");
            assert_eq!(
                response.headers()["x-h2-marker"],
                "static",
                "{name} {method}"
            );
            assert_eq!(
                response.text().await.unwrap(),
                "answered over h2",
                "{name} {method}"
            );
        }
    }
}

#[tokio::test]
async fn the_h2_transport_refuses_https_with_the_reason() {
    let err = FetchClient::transport(Duration::from_secs(5))
        .http2_prior_knowledge()
        .get("https://127.0.0.1:1/")
        .send()
        .await
        .err()
        .expect("https on the transport");
    assert!(err.to_string().contains(HTTPS_UNSUPPORTED), "{err:#}");
}
