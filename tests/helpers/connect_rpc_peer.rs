//! Mandatory pinned Node peers; process owns port zero and reports the actual bound port.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
pub const SCHEMA: &str = include_str!("grpc_streams.proto");
pub async fn state() -> netget::state::AppState {
    let state = netget::state::AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    state
}
fn command() -> tokio::process::Command {
    let directory = std::env::var_os("NETGET_CONNECT_RPC_NODE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/helpers/connect-rpc-env"));
    let mut command = tokio::process::Command::new("node");
    command
        .current_dir(directory)
        .arg("--input-type=module")
        .arg("--eval")
        .arg(format!(
            "{}\n{}",
            include_str!("grpcweb_schema.mjs"),
            include_str!("connect_rpc_peer.mjs")
        ))
        .kill_on_drop(true)
        .stdin(Stdio::null());
    command
}
pub struct Peer {
    pub port: u16,
    _child: tokio::process::Child,
}
impl Peer {
    pub async fn server() -> Result<Self> {
        let mut child = command()
            .arg("server")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Node and pinned grpcweb-package.json are mandatory")?;
        let mut lines = BufReader::new(child.stdout.take().context("peer stdout")?).lines();
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await??;
        let Some(line) = line else {
            let output = child.wait_with_output().await?;
            anyhow::bail!(
                "mandatory Connect-ES peer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let value: Value = serde_json::from_str(&line)?;
        anyhow::ensure!(
            value["connect"] == "2.2.0" && value["protobuf"] == "2.16.0",
            "peer versions mismatch"
        );
        let port = value["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port > 0)
            .context("invalid bound port")?;
        Ok(Self {
            port,
            _child: child,
        })
    }
}
pub async fn client(port: u16, scenario: &str, fetch: bool) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        command()
            .arg(if fetch { "fetch-client" } else { "node-client" })
            .arg(format!("127.0.0.1:{port}"))
            .arg(scenario)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success(),
        "mandatory Connect-ES client failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}
pub async fn server(
    state: &netget::state::AppState,
    handlers: Vec<Value>,
    extra: Value,
) -> Result<(netget::state::ServerId, u16)> {
    let mut parameters = json!({"proto_schema":SCHEMA});
    parameters
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ServerForm {
        protocol: "connect-rpc".into(),
        port: Some(0),
        startup_params: Some(parameters),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(state, sender)
    .await?;
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(address) = state
                .get_server(id)
                .await
                .and_then(|server| server.local_addr)
            {
                return address.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("NetGet Connect server did not bind")?;
    Ok((id, port))
}
pub fn handler() -> Value {
    json!({"event_pattern":"grpc_stream_opened","handler":{"type":"script","language":"python","code":
        "import json,sys\ne=json.load(sys.stdin)['event']\nr=e['message']\nif r.get('name')=='status':\n a=[{'type':'grpc_error','code':'PERMISSION_DENIED','message':'denied: peer test'}]\nelif len(r.get('name',''))>100000:\n a=[{'type':'grpc_stream_send','message':{'name':'bounded'}},{'type':'grpc_stream_finish'}]\nelse:\n n=3 if e['server_streaming'] else 1\n a=[{'type':'grpc_stream_send','message':{'name':r['name']+'-'+str(i) if e['server_streaming'] else 'echo:'+r['name'],'value':i+1 if e['server_streaming'] else r['value']+1,'tags':r.get('tags',[]),'counts':r.get('counts',{})}} for i in range(n)]+[{'type':'grpc_stream_finish'}]\na=[{'type':'connect_rpc_metadata','phase':'headers','metadata':{'x-leading':'before:values'}},{'type':'connect_rpc_metadata','phase':'trailers','metadata':{'x-note':'after:values'}}]+a\nprint(json.dumps({'actions':a}))"
    }})
}
pub async fn client_id(
    state: &netget::state::AppState,
    port: u16,
    handlers: Vec<Value>,
    extra: Value,
) -> Result<netget::state::ClientId> {
    let mut parameters = json!({"proto_schema":SCHEMA});
    parameters
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ClientForm {
        protocol: "connect-rpc".into(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        startup_params: Some(parameters),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        sender,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state.has_client_handle(id).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("NetGet Connect client did not connect")?;
    Ok(id)
}
pub fn wait_handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"static","actions":[{"type":"wait_for_more"}]}}),
    ]
}
pub fn call(id: u32, method: &str, name: &str) -> Value {
    json!({"type":"connect_rpc_call","call_id":id,"service":"streams.Session","method":method,
    "request":{"name":name,"value":4,"tags":["blue","green"],"counts":{"first":2,"second":3}}})
}
pub async fn send(
    state: &netget::state::AppState,
    id: netget::state::ClientId,
    action: Value,
) -> Result<netget::state::client_handles::ClientSendOutcome> {
    state
        .send_to_client(id, action, Duration::from_secs(5))
        .await
}
pub async fn log(
    state: &netget::state::AppState,
    id: netget::state::ClientId,
    event: &str,
    call: u32,
) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(log) = state
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                    None,
                )
                .await
                .into_iter()
                .find(|log| log.event_type == event && log.request["call_id"] == call)
            {
                return log.request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {event} for call {call}"))
}
