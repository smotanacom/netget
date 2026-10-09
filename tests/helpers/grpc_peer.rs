//! Mandatory generated grpcio 1.75.1 peer, with port-zero readiness and owned process.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const SCHEMA: &str = include_str!("grpc_streams.proto");
pub async fn state() -> netget::state::AppState {
    let state = netget::state::AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    state
}
pub async fn netget_server(
    state: &netget::state::AppState,
    handlers: Vec<Value>,
    extra: Value,
) -> Result<(netget::state::ServerId, u16)> {
    let mut parameters = serde_json::json!({"proto_schema":SCHEMA});
    parameters.as_object_mut().unwrap().extend(
        extra
            .as_object()
            .context("startup extras must be object")?
            .clone(),
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ServerForm {
        protocol: "grpc".into(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params: Some(parameters),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(state, sender)
    .await?;
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(port) = state
                .get_server(id)
                .await
                .and_then(|server| server.local_addr.map(|address| address.port()))
            {
                return port;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("NetGet gRPC did not bind")?;
    Ok((id, port))
}
fn python() -> PathBuf {
    std::env::var_os("NETGET_GRPCIO_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| "python3".into())
}
fn command(directory: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(python());
    command
        .arg(directory.join("grpc_peer.py"))
        .arg("--schema")
        .arg(directory.join("grpc_streams.proto"))
        .kill_on_drop(true)
        .stdin(Stdio::null());
    command
}
async fn files() -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    tokio::fs::write(
        directory.path().join("grpc_peer.py"),
        include_str!("grpc_peer.py"),
    )
    .await?;
    tokio::fs::write(directory.path().join("grpc_streams.proto"), SCHEMA).await?;
    Ok(directory)
}
pub struct Peer {
    pub port: u16,
    pub ca_file: Option<PathBuf>,
    _child: tokio::process::Child,
    _directory: tempfile::TempDir,
}
impl Peer {
    pub async fn server() -> Result<Self> {
        Self::start(false, None).await
    }
    pub async fn tls() -> Result<Self> {
        Self::start(true, None).await
    }
    pub async fn reflection(mode: &str) -> Result<Self> {
        Self::start(false, Some(mode)).await
    }
    async fn start(tls: bool, mode: Option<&str>) -> Result<Self> {
        let directory = files().await?;
        let mut command = command(directory.path());
        if let Some(mode) = mode {
            command.arg("--reflection-mode").arg(mode);
        }
        let ca_file = if tls {
            let cert = directory.path().join("cert.pem");
            let key = directory.path().join("key.pem");
            let status = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new("openssl")
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
                    ])
                    .arg("-keyout")
                    .arg(&key)
                    .arg("-out")
                    .arg(&cert)
                    .kill_on_drop(true)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            )
            .await??;
            anyhow::ensure!(
                status.success(),
                "openssl is required to provision independent TLS peer"
            );
            command.arg("--cert").arg(&cert).arg("--key").arg(&key);
            Some(cert)
        } else {
            None
        };
        let mut child = command.arg("--server").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
            .context("grpcio peer missing: set NETGET_GRPCIO_PYTHON and install tests/helpers/grpcio-peer-requirements.txt")?;
        let mut lines =
            BufReader::new(child.stdout.take().context("peer stdout unavailable")?).lines();
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .context("grpcio readiness deadline")??;
        let Some(line) = line else {
            let output = child.wait_with_output().await?;
            anyhow::bail!(
                "mandatory grpcio peer failed; install pinned requirements: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let ready: Value = serde_json::from_str(&line)?;
        let port = ready["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .context("bad grpcio readiness port")?;
        anyhow::ensure!(
            ready["grpcio"] == "1.75.1",
            "unexpected grpcio peer version"
        );
        Ok(Self {
            port,
            ca_file,
            _child: child,
            _directory: directory,
        })
    }
}
pub async fn client(port: u16, scenario: &str) -> Result<Value> {
    let directory = files().await?;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        command(directory.path())
            .arg("--target")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--scenario")
            .arg(scenario)
            .output(),
    )
    .await
    .context("grpcio client deadline")??;
    anyhow::ensure!(
        output.status.success(),
        "grpcio failed; install pinned requirements: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("grpcio client output is not JSON")
}
pub fn pool() -> Result<prost_reflect::DescriptorPool> {
    use prost::Message;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("grpc_streams.proto"), SCHEMA)?;
    let output = std::process::Command::new("protoc")
        .current_dir(directory.path())
        .arg("--proto_path=.")
        .arg("--descriptor_set_out=/dev/stdout")
        .arg("--include_imports")
        .arg("grpc_streams.proto")
        .output()
        .context("protoc must be installed for generic streaming tests")?;
    anyhow::ensure!(
        output.status.success(),
        "protoc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    prost_reflect::DescriptorPool::from_file_descriptor_set(prost_types::FileDescriptorSet::decode(
        output.stdout.as_slice(),
    )?)
    .context("fixture descriptors invalid")
}

pub fn semantic_handler() -> serde_json::Value {
    let code = r#"import json,sys
d=json.load(sys.stdin)
e=d['event']
k=d['event_type_id']
a=[{'type':'grpc_stream_wait','milliseconds':1000}]
if k=='grpc_stream_opened' and e['method']=='Watch':
    req=e['message']
    a=[{'type':'grpc_stream_send','message':{'name':'watch-'+str(i),'value':i,'tags':req.get('tags',[]),'counts':req.get('counts',{})}} for i in range(3)]
    a.append({'type':'grpc_stream_finish'})
elif k=='grpc_stream_message' and e['method']=='Chat':
    req=e['message']
    a=[{'type':'grpc_stream_send','message':{'name':req['name'],'value':req['value']+1}}]
elif k=='grpc_stream_input_closed':
    a=[]
    if e['method']=='Collect': a=[{'type':'grpc_stream_send','message':{'name':'collected','value':5,'counts':{'messages':e['input_count']}}}]
    a.append({'type':'grpc_stream_finish'})
print(json.dumps({'actions':a}))
"#;
    serde_json::json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":code}})
}
