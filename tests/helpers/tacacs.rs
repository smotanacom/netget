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
    let id = ServerForm {
        protocol: "tacacs".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: handlers,
        startup_params: Some(params.unwrap_or_else(|| json!({"shared_secret":"test-secret"}))),
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
        protocol: "tacacs".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: handlers,
        startup_params: Some(params.unwrap_or_else(|| json!({"shared_secret":"test-secret"}))),
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

pub(crate) fn authentication(method: &str) -> Value {
    json!({"type":"authenticate_tacacs","username":"alice","password":"correct","method":method,"port":"tty1","remote_address":"192.0.2.10"})
}
pub(crate) fn authorization() -> Value {
    json!({"type":"authorize_tacacs","request":{"username":"alice","port":"tty1","remote_address":"192.0.2.10","arguments":[{"name":"service","value":"shell"},{"name":"cmd","value":"show"},{"name":"cmd-arg","value":"version"}]}})
}
pub(crate) fn accounting(kind: &str) -> Value {
    json!({"type":"account_tacacs","record_type":kind,"request":{"username":"alice","port":"tty1","remote_address":"192.0.2.10","arguments":[{"name":"task_id","value":"42"},{"name":"start_time","value":"1700000000"}]}})
}
pub(crate) fn policies() -> Vec<Value> {
    vec![
        json!({"event_pattern":"tacacs_authentication","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']['request']\nstatus='pass' if e['username']=='alice' and e['password']=='correct' else 'fail'\nprint(json.dumps({'actions':[{'type':'respond_tacacs_authentication','reply':{'status':status,'server_message':'policy authentication'}}]}))"}}),
        json!({"event_pattern":"tacacs_authorization","handler":{"type":"static","actions":[{"type":"respond_tacacs_authorization","reply":{"status":"pass_add","arguments":[{"name":"priv-lvl","value":"15"},{"name":"audit","value":"enabled","mandatory":false}]}}]}}),
        json!({"event_pattern":"tacacs_accounting","handler":{"type":"static","actions":[{"type":"record_tacacs_accounting","reply":{"status":"success","server_message":"observed in shared log"}}]}}),
    ]
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
pub(crate) fn header(kind: u8, version: u8) -> netget::server::tacacs::codec::Header {
    netget::server::tacacs::codec::Header {
        kind,
        version,
        sequence: 1,
        flags: 0,
        session_id: 0x01020304,
        length: 0,
    }
}
pub(crate) async fn reply(
    peer: &mut tokio::net::TcpStream,
    header: netget::server::tacacs::codec::Header,
    body: &[u8],
) {
    netget::server::tacacs::codec::write_packet(peer, header, body, b"test-secret")
        .await
        .unwrap();
}
pub(crate) async fn read(
    peer: &mut tokio::net::TcpStream,
) -> (netget::server::tacacs::codec::Header, Vec<u8>) {
    netget::server::tacacs::codec::read_packet(peer, b"test-secret", Duration::from_secs(10))
        .await
        .unwrap()
}
pub(crate) struct Peer {
    pub(crate) addr: SocketAddr,
    child: tokio::process::Child,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}
impl Peer {
    pub(crate) async fn start() -> Self {
        use tokio::io::AsyncBufReadExt;
        let binary = std::env::var("NETGET_TACACS_PEER")
            .expect("required pinned nwaples/tacplus0.0.3 SDK peer; missing peer must fail");
        let version = tokio::process::Command::new(&binary)
            .arg("-version")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(version.status.success());
        let version: Value = serde_json::from_slice(&version.stdout).unwrap();
        assert_eq!(version["version"], "0.0.3");
        assert_eq!(
            version["revision"],
            "01141c615540e7ae8bf5ca1b412d0788cb34222b"
        );
        assert_eq!(version["source_modified"], false);
        let mut child = tokio::process::Command::new(binary)
            .args(["-role", "server", "-addr", "127.0.0.1:0"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let addr = serde_json::from_str::<Value>(&ready).unwrap()["ready"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        Self { addr, child, lines }
    }
    pub(crate) async fn recorded(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    pub(crate) async fn stop(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}
