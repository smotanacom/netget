#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    llm::OllamaClient,
    state::{AccessLogOwner, AppState, ClientId, ServerId},
};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
pub fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}
pub fn empty() -> Value {
    json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})
}
pub async fn client(
    state: &AppState,
    port: u16,
    params: Value,
    handlers: Vec<Value>,
) -> anyhow::Result<ClientId> {
    let llm = OllamaClient::new("http://127.0.0.1:1");
    state.set_llm_client(llm.clone()).await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    ClientForm {
        protocol: "otlp".into(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(state, llm, tx)
    .await
}
pub async fn server(state: &AppState, handlers: Vec<Value>) -> (ServerId, u16) {
    state
        .set_llm_client(OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "otlp".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let s = state.get_server(id).await.unwrap();
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
            if let netget::state::ServerStatus::Error(error) = s.status {
                panic!("{error}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (id, port)
}
pub async fn send(
    state: &AppState,
    id: ClientId,
    action: Value,
) -> anyhow::Result<netget::state::client_handles::ClientSendOutcome> {
    state
        .send_to_client(id, action, Duration::from_secs(8))
        .await
}
pub async fn event(state: &AppState, id: ClientId, name: &str, index: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let logs = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await;
            let events: Vec<_> = logs
                .iter()
                .rev()
                .filter(|log| log.event_type == name)
                .collect();
            if let Some(log) = events.get(index) {
                return log.request.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
pub fn exports() -> [Value; 3] {
    [
        json!({"type":"export_otlp_traces","service_name":"checkout","resource_attributes":{"deployment.environment.name":"test","batch":7,"enabled":true},"scope_name":"netget-peer-test","spans":[{"name":"collector trace marker","trace_id":"0102030405060708090a0b0c0d0e0f10","span_id":"0102030405060708","start_time_unix_nano":1720000000000000000u64,"end_time_unix_nano":1720000000001000000u64,"kind":"client","status":"error","status_message":"card refused","attributes":{"component":"payments"}}]}),
        json!({"type":"export_otlp_gauge","service_name":"checkout","name":"collector.queue.depth","unit":"1","data_points":[{"value":7,"time_unix_nano":1720000000000000000u64,"attributes":{"region":"west"}},{"value":3.5,"time_unix_nano":1720000000001000000u64,"attributes":{"region":"east"}}]}),
        json!({"type":"export_otlp_logs","service_name":"checkout","logs":[{"body":"collector log marker","time_unix_nano":1720000000000000000u64,"severity_number":17,"severity_text":"ERROR","attributes":{"retry":false},"trace_id":"0102030405060708090a0b0c0d0e0f10","span_id":"0102030405060708"}]}),
    ]
}
pub struct Collector {
    pub child: tokio::process::Child,
    pub dir: tempfile::TempDir,
    pub port: u16,
    pub tls: bool,
    pub transport: String,
}
impl Collector {
    pub async fn start(transport: &str, tls: bool) -> Self {
        let bin = std::env::var("NETGET_OTELCOL_BIN").unwrap_or_else(|_| "otelcol".into());
        let version=tokio::process::Command::new(&bin).arg("--version").output().await.expect("Install official core otelcol0.162.0 and set NETGET_OTELCOL_BIN; no peer skip is permitted");
        assert!(
            version.status.success()
                && String::from_utf8_lossy(&version.stdout).contains("0.162.0"),
            "required pinned Collector0.162.0: {version:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        if tls {
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
                    "-addext",
                    "keyUsage=digitalSignature,keyEncipherment",
                    "-addext",
                    "extendedKeyUsage=serverAuth",
                    "-keyout",
                ])
                .arg(dir.path().join("key.pem"))
                .arg("-out")
                .arg(dir.path().join("cert.pem"))
                .output()
                .await
                .expect("openssl is required for independent TLS peer certificate");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let tls_config = if tls {
            format!(
                "        tls:\n          cert_file: {}\n          key_file: {}\n",
                dir.path().join("cert.pem").display(),
                dir.path().join("key.pem").display()
            )
        } else {
            String::new()
        };
        let config=format!("receivers:\n  otlp:\n    protocols:\n      {transport}:\n        endpoint: 127.0.0.1:0\n{tls_config}exporters:\n  debug:\n    verbosity: detailed\nservice:\n  telemetry:\n    metrics:\n      level: none\n  pipelines:\n    traces:\n      receivers: [otlp]\n      exporters: [debug]\n    metrics:\n      receivers: [otlp]\n      exporters: [debug]\n    logs:\n      receivers: [otlp]\n      exporters: [debug]\n");
        std::fs::write(dir.path().join("collector.yaml"), config).unwrap();
        let output = std::fs::File::create(dir.path().join("output.log")).unwrap();
        let mut child = tokio::process::Command::new(bin)
            .arg("--config")
            .arg(dir.path().join("collector.yaml"))
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Pinned Collector logs the actual listener address after binding port0.
        let port = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!(
                        "Collector exited {status}: {}",
                        std::fs::read_to_string(dir.path().join("output.log")).unwrap()
                    );
                }
                let output = std::fs::read_to_string(dir.path().join("output.log")).unwrap();
                if output.contains("Everything is ready") {
                    if let Some(port) = output
                        .lines()
                        .filter(|line| {
                            line.contains("Starting GRPC server")
                                || line.contains("Starting HTTP server")
                        })
                        .find_map(|line| {
                            line.rsplit('\t')
                                .next()
                                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                                .and_then(|data| {
                                    data["endpoint"]
                                        .as_str()
                                        .and_then(|addr| addr.strip_prefix("127.0.0.1:"))
                                        .and_then(|port| port.parse::<u16>().ok())
                                })
                                .filter(|port| *port != 0)
                        })
                    {
                        return port;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Collector never exposed its actual bound listener: {}",
                std::fs::read_to_string(dir.path().join("output.log")).unwrap()
            )
        });
        Self {
            child,
            dir,
            port,
            tls,
            transport: transport.into(),
        }
    }
    pub fn params(&self) -> Value {
        let mut p = json!({"transport":self.transport,"tls":self.tls,"gzip":true});
        if self.tls {
            p["ca_cert_path"] = json!(self.dir.path().join("cert.pem"));
            p["server_name"] = json!("localhost");
        }
        p
    }
    pub fn output(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("output.log")).unwrap()
    }
    pub async fn wait_output(&self, needles: &[&str]) -> String {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let output = self.output();
                if needles.iter().all(|n| output.contains(n)) {
                    return output;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing Collector diagnostics: {}", self.output()))
    }
    pub async fn stop(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}
