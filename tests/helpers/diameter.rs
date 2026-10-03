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
    let id =
        ServerForm {
            protocol: "diameter".into(),
            host: Some("127.0.0.1".into()),
            port: Some(0),
            instruction: Some(String::new()),
            event_handlers: handlers,
            startup_params: Some(params.unwrap_or_else(
                || json!({"origin_host":"server.example","origin_realm":"example"}),
            )),
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
    let id =
        ClientForm {
            protocol: "diameter".into(),
            remote_addr: Some(remote),
            instruction: Some(String::new()),
            event_handlers: handlers,
            startup_params: Some(params.unwrap_or_else(
                || json!({"origin_host":"client.example","origin_realm":"example"}),
            )),
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

pub(crate) fn policy(verdict: &str) -> Vec<Value> {
    vec![
        json!({"event_pattern":"diameter_aa_request","handler":{"type":"static","actions":[{"type":"respond_diameter_aa","reply":{"verdict":verdict,"service_type":1,"filter_ids":["allow-test"],"session_timeout":60}}]}}),
    ]
}
pub(crate) fn aa(password: &str, kind: u32) -> Value {
    let mut v = json!({"type":"send_diameter_aa","username":"alice","auth_request_type":kind,"nas_identifier":"test-nas","nas_port":7});
    if kind != 2 {
        v["password"] = json!(password);
    }
    v
}
pub(crate) fn identity(host: &str) -> netget::server::diameter::codec::Identity {
    netget::server::diameter::codec::Identity {
        host: host.into(),
        realm: "example".into(),
    }
}
pub(crate) async fn intercept(state: &AppState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
pub(crate) async fn negotiated(addr: SocketAddr) -> tokio::net::TcpStream {
    use netget::server::diameter::codec::*;
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut p = Packet::request(CER, 0).unwrap();
    capability_fields(
        &mut p,
        &identity("client.example"),
        "127.0.0.1".parse().unwrap(),
    );
    write_packet(&mut socket, &p).await.unwrap();
    let a = read_packet(&mut socket, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(a.matches(&p));
    assert_eq!(a.num(RESULT).unwrap(), 2001);
    socket
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum PeerKind {
    Python,
    Go,
}
impl PeerKind {
    fn command(self) -> tokio::process::Command {
        match self {
            Self::Python => {
                let python = std::env::var("NETGET_DIAMETER_PYTHON")
                    .expect("required pinned python-diameter0.9.0 peer Python");
                let mut c = tokio::process::Command::new(python);
                c.arg("-u").arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/server/diameter/peer.py"
                ));
                c
            }
            Self::Go => tokio::process::Command::new(
                std::env::var("NETGET_DIAMETER_PEER")
                    .expect("required unchanged pinned go-diameter4.5.0 peer binary"),
            ),
        }
    }
}
pub(crate) async fn peer_client(
    kind: PeerKind,
    addr: SocketAddr,
    password: &str,
    request_type: u32,
) -> Value {
    let mut cmd = kind.command();
    match kind {
        PeerKind::Python => {
            cmd.args([
                "--mode",
                "client",
                "--port",
                &addr.port().to_string(),
                "--password",
                password,
                "--request-type",
                &request_type.to_string(),
            ]);
        }
        PeerKind::Go => {
            cmd.args([
                "-mode",
                "client",
                "-address",
                &addr.to_string(),
                "-password",
                password,
                "-request-type",
                &request_type.to_string(),
            ]);
        }
    }
    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut death_tie = super::child_guard::arm_death_tie(child.id().unwrap());
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    if let Some(tie) = death_tie.as_mut() {
        tie.disarm();
    }
    assert!(
        output.status.success(),
        "{kind:?} peer failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
pub(crate) struct Peer {
    pub addr: SocketAddr,
    child: tokio::process::Child,
    death_tie: Option<super::child_guard::DeathTie>,
}
impl Peer {
    pub(crate) async fn start(kind: PeerKind) -> Self {
        use tokio::io::AsyncBufReadExt;
        let mut cmd = kind.command();
        match kind {
            PeerKind::Python => {
                cmd.args(["--mode", "server"]);
            }
            PeerKind::Go => {
                cmd.args(["-mode", "server", "-address", "127.0.0.1:0"]);
            }
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let death_tie = super::child_guard::arm_death_tie(child.id().unwrap());
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("peer ready line");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ready"], true);
        let addr = match kind {
            PeerKind::Go => value["address"].as_str().unwrap().parse().unwrap(),
            PeerKind::Python => format!("127.0.0.1:{}", value["port"].as_u64().unwrap())
                .parse()
                .unwrap(),
        };
        Self {
            addr,
            child,
            death_tie,
        }
    }
    pub(crate) async fn stop(mut self) {
        use tokio::io::AsyncWriteExt;
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"\n")
            .await
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await {
            Ok(status) => {
                if let Some(tie) = self.death_tie.as_mut() {
                    tie.disarm();
                }
                assert!(
                    status.unwrap().success(),
                    "healthy upstream peer stop must succeed"
                );
            }
            Err(_) => {
                self.child.kill().await.unwrap();
                self.child.wait().await.unwrap();
                if let Some(tie) = self.death_tie.as_mut() {
                    tie.disarm();
                }
                panic!("upstream healthy stop deadline");
            }
        }
    }
}
