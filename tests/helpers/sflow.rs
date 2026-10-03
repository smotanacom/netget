use netget::cli::management::ServerForm;
use netget::server::sflow::codec::Batch;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, process::Stdio, time::Duration};
use tokio::sync::mpsc;
pub(crate) fn golden() -> Vec<u8> {
    include_str!("../server/sflow/four_samples.hex")
        .split_whitespace()
        .map(|word| u8::from_str_radix(word, 16).unwrap())
        .collect()
}
pub(crate) fn batch() -> Batch {
    let ip = json!({"kind":"sampled_ipv4","packet_length":64,"protocol":17,
        "source_ip":"192.0.2.2","destination_ip":"198.51.100.2",
        "source_port":53,"destination_port":123,"tcp_flags":0,"traffic_class":16});
    let counter = json!({"kind":"vlan","vlan_id":42,"octets":9007199254740999u64,
        "unicast_packets":11,"multicast_packets":12,"broadcast_packets":13,"discards":14});
    let mut samples = vec![];
    for expanded in [false, true] {
        samples.push(json!({"kind":"flow","expanded":expanded,
            "sequence_number":if expanded {6} else {5},"source":{"class":2,"index":3},
            "sampling_rate":1000,"sample_pool":12345,"drops":4,
            "input":{"format":1,"value":9},"output":{"format":2,"value":3},"records":[ip.clone()]}));
        samples.push(json!({"kind":"counters","expanded":expanded,
            "sequence_number":if expanded {18} else {17},"source":{"class":2,"index":3},"records":[counter.clone()]}));
    }
    serde_json::from_value(json!({"agent_address":"192.0.2.1","sub_agent_id":77,
        "uptime_ms":123456,"samples":samples}))
    .unwrap()
}
pub(crate) async fn start(
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (
    AppState,
    ServerId,
    SocketAddr,
    mpsc::UnboundedReceiver<String>,
) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, rx) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "sflow".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some("Default must not call the model".into()),
        event_handlers: handlers,
        startup_params: params,
        ..Default::default()
    }
    .create(&state, tx)
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
    (state, id, addr, rx)
}
pub(crate) async fn logs(
    state: &AppState,
    id: ServerId,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut entries = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if entries.len() >= count {
                entries.sort_by_key(|e| e.id);
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
pub(crate) fn peer() -> String {
    std::env::var("NETGET_SFLOW_PEER")
        .expect("pinned unmodified Cistern/sflow peer required; run install_peers.py, no skip")
}
pub(crate) fn peer_golden() -> Vec<u8> {
    include_str!("../server/sflow/cistern_compact.hex")
        .split_whitespace()
        .map(|s| u8::from_str_radix(s, 16).unwrap())
        .collect()
}
pub(crate) fn read_records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}

pub(crate) struct Collector {
    root: tempfile::TempDir,
    pub(crate) port: u16,
    child: tokio::process::Child,
}
impl Collector {
    pub(crate) async fn start() -> Self {
        let binary = std::env::var("NETGET_SFLOW_COLLECTOR")
            .expect("pinned official GoFlow2 collector required; no missing-service skip");
        let version = tokio::process::Command::new(&binary)
            .arg("-v")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(version.status.success());
        assert!(String::from_utf8_lossy(&version.stdout).contains("GoFlow2 v2.2.7 "));
        let root = tempfile::Builder::new()
            .prefix("netget-sflow-goflow-")
            .tempdir()
            .unwrap();
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let output = std::fs::File::create(root.path().join("records.json")).unwrap();
        let error = std::fs::File::create(root.path().join("daemon.log")).unwrap();
        let child = tokio::process::Command::new(binary)
            .args([
                "-listen",
                &format!("sflow://127.0.0.1:{port}"),
                "-addr",
                "127.0.0.1:0",
                "-produce",
                "raw",
                "-format",
                "json",
                "-transport",
                "file",
            ])
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut service = Self { root, port, child };
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Retried fixture-only readiness: version5,agent192.0.2.99,subagent999,zero samples.
        let header = [
            0, 0, 0, 5, 0, 0, 0, 1, 192, 0, 2, 99, 0, 0, 3, 231, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(
                    service.child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(service.root.path().join("daemon.log")).unwrap()
                );
                probe
                    .send_to(&header, format!("127.0.0.1:{port}"))
                    .await
                    .unwrap();
                if service
                    .records()
                    .iter()
                    .any(|r| r["message"]["sub-agent-id"] == 999)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("official collector must receive a readiness datagram");
        service
    }
    fn records(&self) -> Vec<Value> {
        read_records(&self.root.path().join("records.json"))
    }
    pub(crate) async fn messages(&mut self, count: usize) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let messages = self
                    .records()
                    .into_iter()
                    .filter(|r| r["message"]["sub-agent-id"] == 77)
                    .collect::<Vec<_>>();
                if messages.len() >= count {
                    break messages;
                }
                assert!(self.child.try_wait().unwrap().is_none(), "collector exited");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    pub(crate) async fn stop(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}
