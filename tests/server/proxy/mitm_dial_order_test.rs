//! Two ways the MITM proxy touched the outside world before anyone had decided it should.
//!
//! **The upstream was dialled before the model's decision.** `perform_mitm` connected to
//! `dest_host:dest_port` and sent a TLS ClientHello carrying the peer's SNI right after the
//! client handshake, and only then read the request and asked the model. Any proxy client
//! could therefore make NetGet open a TCP connection to an arbitrary host:port — a
//! reachability oracle for internal addresses, with the answer in handshake timing — and a
//! request the model went on to block had already cost its destination a handshake. The
//! upstream is now dialled only once the model has let the request through.
//!
//! **`ca_export_path` was `std::fs::write`.** It is a startup parameter, so the model can set
//! it through `open_server`, and the write followed symlinks and truncated whatever was
//! there. It now refuses a symlink and a file that is not a previous export.
//!
//! The model here is a closed port, so every request decision fails closed to a block
//! (`decision=llm_error`), which is exactly the decision after which no dial may happen.
//! Loopback only; the "upstream" is a listener that counts accepts and never speaks.
//!
//! Run with:
//!   cargo test --no-default-features --features proxy --test server -- proxy::mitm_dial_order

#![cfg(feature = "proxy")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::server::proxy::write_ca_export;
use netget::state::app_state::AppState;
use netget::state::server::ServerStatus;
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
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
            if let ServerStatus::Error(e) = &s.status {
                panic!("proxy server failed to start: {e}");
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("Proxy server #{} never bound a port", id.as_u32());
}

async fn start_mitm_proxy(
    state: &AppState,
    startup_params: serde_json::Value,
) -> anyhow::Result<(ServerId, u16)> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "proxy".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params: Some(startup_params),
        ..Default::default()
    }
    .create(state, tx)
    .await?;
    let port = wait_for_port(state, server_id).await;
    Ok((server_id, port))
}

/// A listener that counts accepted connections and never writes a byte.
async fn counting_upstream() -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    (port, accepted)
}

#[tokio::test]
async fn a_blocked_mitm_request_never_dials_the_upstream() {
    let state = new_state().await;
    let (_id, proxy_port) =
        start_mitm_proxy(&state, serde_json::json!({"certificate_mode": "generate"}))
            .await
            .expect("start MITM proxy");
    let (upstream_port, accepted) = counting_upstream().await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")).unwrap())
        // The proxy mints a leaf for "127.0.0.1" on the fly; nothing trusts its CA here.
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let response = client
        .get(format!("https://127.0.0.1:{upstream_port}/secret"))
        .send()
        .await;
    // The model is unreachable, so the request fails closed: a 5xx the proxy wrote itself.
    match response {
        Ok(r) => assert!(
            r.status().is_server_error(),
            "a fail-closed block must be a 5xx, got {}",
            r.status()
        ),
        Err(e) => panic!("the proxy must answer the blocked request itself, got {e}"),
    }

    // Give a stray dial every chance to land before counting.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "the upstream was dialled before the model blocked the request"
    );
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "netget-proxy-ca-export-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

#[test]
fn ca_export_refuses_to_overwrite_a_file_that_is_not_a_previous_export() {
    let dir = scratch_dir("foreign");
    let path = dir.join("authorized_keys");
    std::fs::write(&path, "ssh-ed25519 AAAA operator@host\n").unwrap();
    let err = write_ca_export(&path, PEM).expect_err("must refuse a foreign file");
    assert!(err.to_string().contains("not a PEM certificate"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "ssh-ed25519 AAAA operator@host\n",
        "the file must be untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn ca_export_refuses_a_symlink() {
    let dir = scratch_dir("symlink");
    let target = dir.join("target.txt");
    std::fs::write(&target, "keep me").unwrap();
    let link = dir.join("ca.pem");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let err = write_ca_export(&link, PEM).expect_err("must refuse a symlink");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ca_export_writes_a_new_file_and_overwrites_its_own_previous_export() {
    let dir = scratch_dir("ok");
    let path = dir.join("ca.pem");
    write_ca_export(&path, PEM).expect("a new file");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), PEM);
    let newer = PEM.replace("MIIB", "MIIC");
    write_ca_export(&path, &newer).expect("a restart overwrites the previous export");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    std::fs::write(&path, "").unwrap();
    write_ca_export(&path, PEM).expect("an empty file is fine too");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_proxy_whose_ca_export_path_is_a_foreign_file_does_not_start_and_leaves_it_alone() {
    let dir = scratch_dir("startup");
    let path = dir.join("bashrc");
    std::fs::write(&path, "export PS1='$ '\n").unwrap();
    let state = new_state().await;
    let started = start_mitm_proxy(
        &state,
        serde_json::json!({
            "certificate_mode": "generate",
            "ca_export_path": path.to_string_lossy(),
        }),
    )
    .await;
    // Either `create` reports the error or the server lands in `Error`; what matters is
    // that it is not Running and the file is intact.
    if let Ok((id, _)) = started {
        let status = state.get_server(id).await.map(|s| s.status);
        assert!(
            matches!(status, Some(ServerStatus::Error(_))),
            "the proxy must not start on a foreign ca_export_path, got {status:?}"
        );
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "export PS1='$ '\n");
    let _ = std::fs::remove_dir_all(&dir);
}
