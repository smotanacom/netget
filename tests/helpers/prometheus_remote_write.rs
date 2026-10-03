use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
pub(crate) fn batch() -> Value {
    json!({"series":[{"labels":{"__name__":"peer_temperature","site":"one 名"},"samples":[{"timestamp_ms":123,"value":21.5},{"timestamp_ms":124,"value":"stale"}]}]})
}
pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}
pub(crate) async fn server(
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (
    AppState,
    ServerId,
    SocketAddr,
    mpsc::UnboundedReceiver<String>,
) {
    let state = state();
    let (tx, rx) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "prometheus-remote-write".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
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
pub(crate) async fn client(
    remote: String,
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (AppState, ClientId) {
    let state = state();
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "prometheus-remote-write".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: handlers,
        startup_params: params,
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await
            && !state
                .get_client(id)
                .await
                .is_none_or(|c| c.status == ClientStatus::Disconnected)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}
pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
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
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
pub(crate) async fn disconnected(state: &AppState, id: ClientId) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state
                .get_client(id)
                .await
                .is_none_or(|c| c.status == ClientStatus::Disconnected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
pub(crate) async fn received(peer: &mut tokio::net::TcpStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = vec![];
        let mut buffer = [0; 8192];
        loop {
            let n = peer.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(at) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&bytes[..at]);
                let len = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|s| s.parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= at + 4 + len {
                    break bytes;
                }
            }
        }
    })
    .await
    .unwrap()
}
pub(crate) async fn reply(peer: &mut tokio::net::TcpStream, status: u16, body: &[u8]) {
    peer.write_all(
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    peer.write_all(body).await.unwrap();
    peer.shutdown().await.unwrap();
}
pub(crate) async fn prometheus_service(config: &str) -> super::real_server::RealServer {
    let binary = std::env::var("NETGET_PRW_PROMETHEUS")
        .expect("required pinned official Prometheus3.15.0; run install_peers.py, no skip");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(&binary)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("prometheus, version 3.15.0 "));
    super::real_server::RealServer::builder(
        &binary,
        super::real_server::InstallHint {
            brew: "run tests/server/prometheus_remote_write/install_peers.py ROOT",
            apt: "run tests/server/prometheus_remote_write/install_peers.py ROOT",
        },
    )
    .config_file("prometheus.yml", config)
    .args([
        "--config.file={dir}/prometheus.yml",
        "--storage.tsdb.path={dir}/data",
        "--web.listen-address=127.0.0.1:{port}",
        "--web.enable-remote-write-receiver",
        "--storage.tsdb.retention.time=1h",
        "--log.level=debug",
    ])
    .ready_when_log_matches("Server is ready to receive web requests")
    .start()
    .await
    .unwrap()
}
pub(crate) async fn exporter() -> super::real_server::RealServer {
    let binary=std::env::var("NETGET_PRW_PYTHON").expect("required isolated official prometheus-client0.22.1 exporter; run install_peers.py and set PYTHONPATH, no skip");
    let version=tokio::process::Command::new(&binary).args(["-c","import importlib.metadata as m;assert m.version('prometheus-client')=='0.22.1';print('prometheus-client0.22.1')"]).kill_on_drop(true).output().await.unwrap();
    assert!(
        version.status.success(),
        "{}",
        String::from_utf8_lossy(&version.stderr)
    );
    let script="import threading\nfrom prometheus_client import CollectorRegistry,Gauge,start_http_server\nr=CollectorRegistry()\nGauge('peer_remote_value','Value',['site'],registry=r).labels('edge 名').set(21.5)\nGauge('peer_remote_nan','NaN',registry=r).set(float('nan'))\nserver,_=start_http_server(0,addr='127.0.0.1',registry=r)\nprint('EXPORTER_PORT='+str(server.server_port),flush=True)\nthreading.Event().wait()\n";
    super::real_server::RealServer::builder(
        &binary,
        super::real_server::InstallHint {
            brew: "run remote write install_peers.py ROOT",
            apt: "run remote write install_peers.py ROOT",
        },
    )
    .config_file("exporter.py", script)
    .args(["-u", "{dir}/exporter.py"])
    .port_from_log("EXPORTER_PORT=([0-9]+)")
    .start()
    .await
    .unwrap()
}
pub(crate) async fn query(
    service: &super::real_server::RealServer,
    expression: &str,
    time_ms: Option<i64>,
) -> Value {
    let mut params = vec![("query", expression.to_owned())];
    if let Some(time) = time_ms {
        params.push(("time", format!("{:.3}", time as f64 / 1000.0)));
    }
    reqwest::Client::new()
        .get(format!("http://{}/api/v1/query", service.addr()))
        .query(&params)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
