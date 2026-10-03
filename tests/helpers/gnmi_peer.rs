//! Mandatory independent pinned gNMIc CLI and generated public grpcio peers.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
pub async fn state() -> netget::state::AppState {
    super::grpc_peer::state().await
}
pub fn path(name: &str) -> Value {
    json!({"origin":"openconfig","elem":[{"name":name,"key":{"name":"eth0"}}]})
}
pub fn notification(counter: u64) -> Value {
    json!({"timestamp":"123456789","prefix":{"target":"fixture"},"update":[{"path":path("system"),"value":{"kind":"uint","value":counter.to_string()}}]})
}
pub fn static_handler(event: &str, actions: Value) -> Value {
    json!({"event_pattern":event,"handler":{"type":"static","actions":actions}})
}
pub fn handlers() -> Vec<Value> {
    let code="import json,sys\ne=json.load(sys.stdin)['event']['request']\nenc=e.get('encoding','PROTO')\nv={'kind':'uint','value':'42'}\nif enc=='JSON': v={'kind':'json','value':{'counter':42}}\nif enc=='JSON_IETF': v={'kind':'json_ietf','value':{'counter':42}}\nif enc=='ASCII': v={'kind':'ascii','value':'value-42'}\nn={'timestamp':'123456789','prefix':{'target':'fixture'},'update':[{'path':{'origin':'openconfig','elem':[{'name':'system','key':{'name':'eth0'}}]},'value':v}]}\nif e.get('path') and e['path'][0]['elem'][0]['name']=='denied': a=[{'type':'gnmi_error','code':7,'message':'denied: fixture'}]\nelif 'mode' not in e: a=[{'type':'gnmi_get_response','notification':[n]}]\nelse:\n a=[{'type':'gnmi_update','notification':[n]},{'type':'gnmi_sync'}]\n if e['mode']=='STREAM': a.append({'type':'gnmi_wait','milliseconds':10})\nprint(json.dumps({'actions':a}))";
    let script = |event: &str| json!({"event_pattern":event,"handler":{"type":"script","language":"python","code":code}});
    vec![
        static_handler(
            "gnmi_capabilities_request",
            json!([{"type":"gnmi_capabilities","supported_models":[{"name":"fixture","organization":"OpenConfig","version":"1"}],"supported_encodings":["PROTO","JSON","JSON_IETF","ASCII"]}]),
        ),
        script("gnmi_get_request"),
        script("gnmi_subscribe_request"),
        script("gnmi_poll_request"),
        static_handler(
            "gnmi_set_request",
            json!([{"type":"gnmi_set_accepted","timestamp":"123456789"}]),
        ),
        static_handler(
            "gnmi_subscription_tick",
            json!([{"type":"gnmi_update","notification":[notification(43)]},{"type":"gnmi_finish"}]),
        ),
    ]
}
pub async fn server(
    state: &netget::state::AppState,
    handlers: Vec<Value>,
    params: Value,
) -> Result<(netget::state::ServerId, u16)> {
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ServerForm {
        protocol: "gnmi".into(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params: Some(params),
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
                .and_then(|s| s.local_addr.map(|a| a.port()))
            {
                break port;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("gNMI did not bind")?;
    Ok((id, port))
}
pub async fn client_id(
    state: &netget::state::AppState,
    port: u16,
    handlers: Vec<Value>,
    params: Value,
) -> Result<netget::state::ClientId> {
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ClientForm {
        protocol: "gnmi".into(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        instruction: Some(String::new()),
        startup_params: Some(params),
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
                break;
            }
            if state
                .get_client(id)
                .await
                .is_some_and(|c| matches!(c.status, netget::state::ClientStatus::Error(_)))
            {
                panic!("gNMI client startup failed");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("gNMI client handle missing")?;
    Ok(id)
}
pub fn wait_handlers() -> Vec<Value> {
    vec![static_handler("*", json!([{"type":"wait_for_more"}]))]
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
            for entry in state
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                    None,
                )
                .await
            {
                if entry.event_type == event && entry.request["call_id"] == call {
                    return entry.request;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {event} for call{call}"))
}
fn python() -> PathBuf {
    std::env::var_os("NETGET_GNMI_PYTHON")
        .or_else(|| std::env::var_os("NETGET_GRPCIO_PYTHON"))
        .map(PathBuf::from)
        .unwrap_or_else(|| "python3".into())
}
fn command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(python());
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/gnmi_peer.py"))
        .arg("--proto-root")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("proto/gnmi"))
        .kill_on_drop(true)
        .stdin(Stdio::null());
    command
}
pub async fn output(mut command: tokio::process::Command) -> Result<(bool, String, String)> {
    let mut child = command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("stdout missing")?;
    let stderr = child.stderr.take().context("stderr missing")?;
    tokio::time::timeout(Duration::from_secs(20), async move {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdout = stdout.take(131073);
        let mut stderr = stderr.take(16385);
        let (status, out_result, err_result) = tokio::join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        );
        out_result?;
        err_result?;
        anyhow::ensure!(
            out.len() <= 131072 && err.len() <= 16384,
            "peer output bound"
        );
        Ok::<_, anyhow::Error>((
            status?.success(),
            String::from_utf8(out)?,
            String::from_utf8_lossy(&err).into_owned(),
        ))
    })
    .await
    .context("independent gNMI peer deadline")?
}
pub async fn client(port: u16, scenario: &str, cert: Option<&Path>) -> Result<Value> {
    let mut command = command();
    command
        .arg("--target")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--scenario")
        .arg(scenario);
    if let Some(cert) = cert {
        command.arg("--cert").arg(cert);
    }
    let (success, stdout, stderr) = output(command).await?;
    anyhow::ensure!(success, "mandatory generated gNMI peer failed: {stderr}");
    Ok(serde_json::from_str(&stdout)?)
}
pub struct Peer {
    pub port: u16,
    pub ca_file: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    _child: tokio::process::Child,
    _directory: tempfile::TempDir,
}
impl Peer {
    pub async fn server(tls: bool) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let (cert, key, ca_file) = if tls {
            let (cert, key, ca) = certificate(directory.path()).await?;
            (Some(cert), Some(key), Some(ca))
        } else {
            (None, None, None)
        };
        let mut command = command();
        command.arg("--server");
        if let Some(cert) = &cert {
            command
                .arg("--cert")
                .arg(cert)
                .arg("--key")
                .arg(key.as_ref().unwrap());
        }
        let mut child=command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("mandatory gNMI SDK peer missing: set NETGET_GNMI_PYTHON with tests/helpers/grpcio-peer-requirements.txt")?;
        let stdout = child.stdout.take().context("readiness stdout missing")?;
        let mut reader = BufReader::new(stdout.take(4097));
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .context("gNMI SDK readiness deadline")??;
        if line.is_empty() {
            let mut stderr = String::new();
            if let Some(error) = child.stderr.take() {
                error.take(16385).read_to_string(&mut stderr).await?;
            }
            anyhow::bail!("mandatory gNMI SDK peer failed: {stderr}");
        }
        anyhow::ensure!(
            line.len() <= 4096 && line.ends_with('\n'),
            "readiness bound"
        );
        let ready: Value = serde_json::from_str(&line)?;
        anyhow::ensure!(
            ready["grpcio"] == "1.75.1" && ready["schema"] == "v0.14.1",
            "unexpected independent peer pin"
        );
        let port = ready["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
            .context("bad peer port")?;
        Ok(Self {
            port,
            ca_file,
            cert,
            key,
            _child: child,
            _directory: directory,
        })
    }
}
pub async fn certificate(directory: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let cert = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let ca = directory.join("ca.pem");
    let ca_key = directory.join("ca-key.pem");
    let csr = directory.join("cert.csr");
    let ext = directory.join("extensions.txt");
    tokio::fs::write(&ext,"subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n").await?;
    let mut root = tokio::process::Command::new("openssl");
    root.args([
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-subj",
        "/CN=NetGetFixtureCA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ])
    .arg("-keyout")
    .arg(&ca_key)
    .arg("-out")
    .arg(&ca);
    let mut leaf = tokio::process::Command::new("openssl");
    leaf.args([
        "req",
        "-new",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=localhost",
    ])
    .arg("-keyout")
    .arg(&key)
    .arg("-out")
    .arg(&csr);
    let mut sign = tokio::process::Command::new("openssl");
    sign.args(["x509", "-req", "-days", "1", "-CAcreateserial"])
        .arg("-in")
        .arg(csr)
        .arg("-CA")
        .arg(&ca)
        .arg("-CAkey")
        .arg(ca_key)
        .arg("-extfile")
        .arg(ext)
        .arg("-out")
        .arg(&cert);
    for command in [root, leaf, sign] {
        let (success, _, stderr) = output(command).await?;
        anyhow::ensure!(success, "openssl fixture failed: {stderr}");
    }
    Ok((cert, key, ca))
}
pub async fn gnmic(
    port: u16,
    args: &[&str],
    cert: Option<&Path>,
) -> Result<(bool, String, String)> {
    let binary = std::env::var_os("NETGET_GNMIC").unwrap_or_else(|| "gnmic".into());
    let mut version = tokio::process::Command::new(&binary);
    version.arg("version");
    let (success, stdout, stderr) = output(version)
        .await
        .context("mandatory gNMIc 0.49.0 missing: set NETGET_GNMIC")?;
    anyhow::ensure!(
        success && stdout.lines().any(|line| line.trim() == "version : 0.49.0"),
        "wrong gNMIc pin: {stdout}{stderr}"
    );
    let mut command = tokio::process::Command::new(binary);
    command.args([
        "--address",
        &format!("127.0.0.1:{port}"),
        "--encoding",
        "proto",
        "--format",
        "protojson",
        "--max-msg-size",
        "1048576",
        "--timeout",
        "5s",
        "--retry",
        "1h",
    ]);
    if let Some(cert) = cert {
        command
            .arg("--tls-ca")
            .arg(cert)
            .args(["--tls-server-name", "localhost"]);
    } else {
        command.arg("--insecure");
    }
    command.args(args);
    output(command).await
}
