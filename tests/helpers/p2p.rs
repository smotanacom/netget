//! Shared fixtures for P2P protocol integration checks.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

pub(crate) async fn server_in(
    state: &AppState,
    protocol: &str,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: protocol.into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (id, addr)
}

pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    protocol: &str,
    handlers: Vec<Value>,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: protocol.into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("P2P client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if rows.len() >= count {
                rows.sort_by_key(|e| e.id);
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn python() -> String {
    std::env::var("NETGET_P2P_PYTHON")
        .expect("NETGET_P2P_PYTHON must name the pinned P2P peer venv; peer tests never skip")
}
pub(crate) fn script(protocol: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/server/{protocol}/peer.py"))
}
pub(crate) async fn peer(protocol: &str, role: &str, addr: SocketAddr) -> Value {
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(python())
            .arg(script(protocol))
            .args([role, &addr.ip().to_string(), &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer stalled")
    .expect("peer unavailable");
    assert!(
        result.status.success(),
        "independent peer failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).expect("peer JSON")
}
pub(crate) async fn peer_server(protocol: &str) -> (tokio::process::Child, SocketAddr) {
    let (c, a, _) = peer_server_details(protocol).await;
    (c, a)
}
pub(crate) async fn peer_server_details(
    protocol: &str,
) -> (tokio::process::Child, SocketAddr, Value) {
    use tokio::io::AsyncBufReadExt;
    let mut c = tokio::process::Command::new(python())
        .arg(script(protocol))
        .args(["server", "127.0.0.1", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("peer unavailable");
    let mut lines = tokio::io::BufReader::new(c.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("peer readiness");
    let v: Value = serde_json::from_str(&line).unwrap();
    (
        c,
        format!("127.0.0.1:{}", v["port"].as_u64().unwrap())
            .parse()
            .unwrap(),
        v,
    )
}
pub(crate) fn quiet() -> Vec<Value> {
    vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})]
}
pub(crate) async fn send(s: &AppState, id: ClientId, a: Value) -> Value {
    let v = s
        .send_to_client(id, a, Duration::from_secs(15))
        .await
        .unwrap();
    match v {
        netget::state::client_handles::ClientSendOutcome::Executed { detail } => {
            serde_json::from_str(&detail).unwrap()
        }
        other => panic!("unexpected {other:?}"),
    }
}

pub(crate) async fn rebind_udp(a: SocketAddr) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tokio::net::UdpSocket::bind(a).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("UDP owner did not release socket");
}

pub(crate) async fn tls_certificate() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let cert = d.path().join("cert.pem");
    let key = d.path().join("key.pem");
    let output = tokio::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-keyout",
        ])
        .arg(key)
        .arg("-out")
        .arg(cert)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "openssl: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    d
}

pub(crate) async fn malformed_and_owner_shutdown(protocol: &str, wire: &[u8]) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let s = state();
    let (id, addr) = server_in(&s, protocol, quiet(), json!({})).await;
    let mut bad = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _ = bad.write_all(wire).await;
    let mut received = vec![];
    let result = tokio::time::timeout(Duration::from_secs(3), bad.read_to_end(&mut received)).await;
    assert!(result.is_ok(), "{protocol}: malformed peer remained open");
    let mut live = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.remove_server(id).await;
    let mut remaining = vec![];
    assert!(
        tokio::time::timeout(Duration::from_secs(3), live.read_to_end(&mut remaining))
            .await
            .is_ok(),
        "{protocol}: owner shutdown left peer alive"
    );
    assert!(
        tokio::net::TcpListener::bind(addr).await.is_ok(),
        "{protocol}: owner did not release listener"
    );
}
