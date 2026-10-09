//! BMP fixtures: a collector policy script, collector and exporter through the shared forms, the
//! pinned GoBGP and gobmp peers (`tests/server/bmp/install_peers.py`) and access-log waits.
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
i=json.load(sys.stdin); e=i['event']
if i['event_type_id']=='bmp_initiation' and e.get('sys_name')=='blocked':
    print(json.dumps({'actions':[{'type':'bmp_close','reason':'unknown router'}]}))
else:
    print(json.dumps({'actions':[{'type':'bmp_continue'}]}))
"#;

/// Keep monitoring every router except one whose sysName is "blocked".
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

pub(crate) async fn server_in(state: &AppState, handlers: Vec<Value>) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "bmp".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
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

/// A BMP exporter whose events are answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["bmp_connected", "bmp_collector_closed"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "bmp".into(),
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
    .map_err(|_| anyhow::anyhow!("BMP client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
    within: Duration,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(within, async {
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn tool(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must name the binary built by tests/server/bmp/install_peers.py (GoBGP 4.9.0, gobmp 1.1.0); this evidence never skips"))
}

const HINT: InstallHint = InstallHint {
    brew: "go (then tests/server/bmp/install_peers.py)",
    apt: "golang (then tests/server/bmp/install_peers.py)",
};

/// The peer router: GoBGP in AS 65002 accepting a session from AS 65001 on its BGP port
/// (`port1`); its API on `port`.
pub(crate) async fn start_route_source() -> super::E2EResult<RealServer> {
    let config = r#"[global.config]
  as = 65002
  router-id = "10.0.0.2"
  port = {port1}
  local-address-list = ["127.0.0.1"]
[[neighbors]]
  [neighbors.config]
    neighbor-address = "127.0.0.1"
    peer-as = 65001
  [neighbors.transport.config]
    passive-mode = true
"#;
    gobgpd(config).extra_ports(1).start().await
}

/// The monitored router: GoBGP in AS 65001 (not listening) connecting to the route source and
/// exporting BMP to `collector`, pre-policy, with statistics every 15 s (GoBGP's minimum).
pub(crate) async fn start_exporter(
    source_bgp_port: u16,
    collector: SocketAddr,
) -> super::E2EResult<RealServer> {
    let config = format!(
        r#"[global.config]
  as = 65001
  router-id = "10.0.0.1"
  port = -1
[[neighbors]]
  [neighbors.config]
    neighbor-address = "127.0.0.1"
    peer-as = 65002
  [neighbors.transport.config]
    remote-port = {source_bgp_port}
[[bmp-servers]]
  [bmp-servers.config]
    address = "{}"
    port = {}
    route-monitoring-policy = "pre-policy"
    statistics-timeout = 15
"#,
        collector.ip(),
        collector.port()
    );
    gobgpd(&config).start().await
}

fn gobgpd(config: &str) -> super::real_server::RealServerBuilder {
    RealServer::builder(&tool("NETGET_BMP_GOBGPD"), HINT)
        .config_file("gobgpd.toml", config)
        .args([
            "-f",
            "{dir}/gobgpd.toml",
            "--api-hosts=127.0.0.1:{port}",
            "--pprof-disable",
            "-p",
        ])
        .ready_when_log_matches("Finished reading the config file")
        .startup_timeout(Duration::from_secs(30))
}

/// Run the gobgp CLI against a gobgpd's API port.
pub(crate) async fn gobgp(api: &str, args: &[&str]) -> String {
    let port = api.rsplit(':').next().unwrap();
    let out = tokio::process::Command::new(tool("NETGET_BMP_GOBGP"))
        .args(["-p", port])
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "gobgp {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// gobmp collecting on `port`, printing each parsed message.
pub(crate) async fn start_gobmp() -> super::E2EResult<RealServer> {
    RealServer::builder(&tool("NETGET_BMP_GOBMP"), HINT)
        .args([
            "--source-port={port}",
            "--performance-port={port1}",
            "--dump=console",
            "--logtostderr",
            "--v=5",
        ])
        .extra_ports(1)
        .ready_when_log_matches("Starting gobmp server")
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
}

/// gobmp's printed messages: (MsgType, Msg JSON).
pub(crate) fn gobmp_messages(log: &str) -> Vec<(u64, Value)> {
    log.lines()
        .filter_map(|l| {
            let kind: u64 = l
                .split("{MsgType:")
                .nth(1)?
                .split(' ')
                .next()?
                .parse()
                .ok()?;
            let body = l.split(" Msg:").nth(1)?.trim_end();
            let body = body.strip_suffix('}')?;
            Some((kind, serde_json::from_str(body).ok()?))
        })
        .collect()
}
