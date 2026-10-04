//! Zenoh fixtures: a handler policy, server and client through the shared forms, the pinned
//! zenoh-pico examples (`tests/server/zenoh/install_peers.py`) and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use super::real_server::{InstallHint, RealServer};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='zenoh_sample':
    if e['key']=='demo/echo/in': out({'type':'zenoh_put','key':'demo/echo/out','payload':'echo: '+e['payload']})
    if e['key']=='demo/ask': out({'type':'zenoh_get','selector':'demo/pq'})
    out()
if k=='zenoh_query':
    if e['key']=='demo/q/fail': out({'type':'zenoh_reply_error','payload':'no such thing'})
    out({'type':'zenoh_reply','payload':'answer for '+e['key']})
out()
"#;

/// demo/echo/in is echoed to demo/echo/out; a sample on demo/ask queries demo/pq; queries are
/// answered "answer for <key>", except demo/q/fail, which gets an error.
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

pub(crate) async fn server_in(state: &AppState, params: Value) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "zenoh".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(policy()),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(20), async {
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

/// A Zenoh client whose events are answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["zenoh_connected", "zenoh_sample", "zenoh_get_result"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "zenoh".into(),
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
    tokio::time::timeout(Duration::from_secs(20), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("Zenoh client did not connect"))??;
    Ok(id)
}

pub(crate) async fn wait_for(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    want: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .find(|e| e.event_type == kind && want(&e.request))
            {
                break e.request;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for a matching {kind} access-log entry"))
}

pub(crate) fn pico(example: &str) -> String {
    let dir = std::env::var("NETGET_ZENOH_PICO").expect("NETGET_ZENOH_PICO must name the zenoh-pico examples directory from tests/server/zenoh/install_peers.py (zenoh-pico 1.10.1); this evidence never skips");
    format!("{dir}/{example}")
}

/// Run a zenoh-pico example to completion (its stdout is block-buffered on a pipe, so only a
/// finished process says what it printed): (success, output).
pub(crate) async fn run_pico(example: &str, args: &[&str]) -> (bool, String) {
    let run = tokio::process::Command::new(pico(example))
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .unwrap_or_else(|_| panic!("{example} {args:?} did not finish"))
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// A zenoh-pico example running as a peer listening on `{port}`.
pub(crate) async fn pico_peer(example: &str, args: &[&str]) -> super::E2EResult<RealServer> {
    let mut all = vec![
        "-m".to_owned(),
        "peer".into(),
        "-l".into(),
        "tcp/127.0.0.1:{port}".into(),
    ];
    all.extend(args.iter().map(|a| a.to_string()));
    RealServer::builder(
        &pico(example),
        InstallHint {
            brew: "cmake (then tests/server/zenoh/install_peers.py)",
            apt: "cmake (then tests/server/zenoh/install_peers.py)",
        },
    )
    .args(all)
    .start()
    .await
}
