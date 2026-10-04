//! MQTT-SN fixtures: a gateway policy script, gateway and client through the shared forms, the
//! pinned mqtt-sn-tools and Paho gateway (`tests/server/mqtt_sn/install_peers.py`), Mosquitto
//! from the system, and access-log waits.
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
if k=='mqttsn_connect' and e['client_id']=='intruder': out({'type':'mqttsn_reject','return_code':'not_supported'})
if k=='mqttsn_message' and e['topic'].startswith('admin/'): out({'type':'mqttsn_reject','return_code':'not_supported'})
if k=='mqttsn_subscribe' and e['topic']=='hold/#':
    out({'type':'mqttsn_accept','qos':1},{'type':'mqttsn_publish','topic':'hold/greeting','payload':'hello','qos':1,'client_id':e['client_id']})
out({'type':'mqttsn_accept'})
"#;

/// Accept everything except the client "intruder" and publishes under admin/; a subscription
/// to hold/# is granted at QoS 1 at most and answered with hold/greeting "hello".
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

pub(crate) async fn server_in(state: &AppState, params: Value) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "mqtt_sn".into(),
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

/// An MQTT-SN client whose events are answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = [
        "mqttsn_connected",
        "mqttsn_result",
        "mqttsn_message_received",
        "mqttsn_disconnected",
    ]
    .iter()
    .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
    .collect();
    let id = ClientForm {
        protocol: "mqtt_sn".into(),
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
    .map_err(|_| anyhow::anyhow!("MQTT-SN client did not connect"))??;
    Ok(id)
}

/// Access-log requests of one kind matching `want`, once there are `count`.
pub(crate) async fn wait_for(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
    want: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind && want(&e.request))
                .collect::<Vec<_>>();
            if rows.len() >= count {
                rows.sort_by_key(|e| e.id);
                break rows.into_iter().map(|e| e.request).collect();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} matching {kind} access-log entries"))
}

pub(crate) fn tool(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must name the binary built by tests/server/mqtt_sn/install_peers.py (mqtt-sn-tools, Paho MQTT-SN Gateway); this evidence never skips"))
}

/// Run mqtt-sn-pub to completion: (success, everything it printed).
pub(crate) async fn mqtt_sn_pub(port: u16, args: &[&str]) -> (bool, String) {
    let out = tokio::process::Command::new(tool("NETGET_MQTTSN_PUB"))
        .args(["-h", "127.0.0.1", "-p", &port.to_string(), "-d"])
        .args(args)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(40), out)
        .await
        .expect("mqtt-sn-pub hung")
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

/// A long-running process whose output is collected.
pub(crate) struct Watched {
    pub child: tokio::process::Child,
    pub out: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Watched {
    pub fn spawn(program: &str, args: &[&str]) -> Self {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("{program}: {e}"));
        let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        for stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let out = out.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    out.lock().unwrap().push_str(&(l + "\n"));
                }
            });
        }
        Self { child, out }
    }
    pub fn text(&self) -> String {
        self.out.lock().unwrap().clone()
    }
    pub async fn wait_for(&self, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !self.text().contains(needle) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never printed {needle:?}:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

const MOSQUITTO: InstallHint = InstallHint {
    brew: "mosquitto",
    apt: "mosquitto mosquitto-clients",
};

pub(crate) async fn start_mosquitto() -> super::E2EResult<RealServer> {
    RealServer::builder("mosquitto", MOSQUITTO)
        .args(["-p", "{port}", "-v"])
        .ready_when_log_matches("running")
        .start()
        .await
}

/// The Paho MQTT-SN Gateway on UDP `port` in front of the Mosquitto on `broker`.
pub(crate) async fn start_paho_gateway(broker: u16) -> super::E2EResult<RealServer> {
    let config = format!(
        "GatewayID=9\nGatewayName=PahoGW\nMaxNumberOfClients=30\nKeepAlive=60\nBrokerName=127.0.0.1\nBrokerPortNo={broker}\nBrokerSecurePortNo={{port2}}\nAggregatingGateway=NO\nQoS-1=NO\nForwarder=NO\nPredefinedTopic=NO\nClientAuthentication=NO\nGatewayPortNo={{port}}\nMulticastPortNo={{port1}}\nMulticastIP=225.1.1.1\nMulticastTTL=1\nShearedMemory=NO\n"
    );
    let gw = RealServer::builder(
        &tool("NETGET_MQTTSN_GATEWAY"),
        InstallHint {
            brew: "cmake openssl@3 (then tests/server/mqtt_sn/install_peers.py)",
            apt: "cmake g++ libssl-dev (then tests/server/mqtt_sn/install_peers.py)",
        },
    )
    .config_file("gateway.conf", &config)
    .args(["-f", "{dir}/gateway.conf"])
    .extra_ports(2)
    .without_tcp_readiness()
    .start()
    .await?;
    // The gateway's stdout is block-buffered on a pipe, so readiness is a CONNECT it answers.
    use netget::server::mqtt_sn::packet::{self, Flags, Packet};
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    probe.connect(("127.0.0.1", gw.port)).await?;
    let connect = packet::encode(&Packet::Connect {
        flags: Flags {
            clean_session: true,
            ..Flags::default()
        },
        duration: 10,
        client_id: "readiness-probe".into(),
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut buf = [0u8; 64];
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("the Paho gateway never answered CONNECT:\n{}", gw.log()).into());
        }
        probe.send(&connect).await?;
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(500), probe.recv(&mut buf)).await
        {
            if packet::decode(&buf[..n]).is_ok_and(|p| matches!(p, Packet::ConnAck { rc: 0 })) {
                probe
                    .send(&packet::encode(&Packet::Disconnect { duration: None }))
                    .await?;
                return Ok(gw);
            }
        }
    }
}
