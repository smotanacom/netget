//! FIX fixtures: an acceptor policy script, server and client through the shared forms, a raw
//! session for wire tests, and the pinned QuickFIX/Go peer (`tests/server/fix/install_peers.py`).
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::server::fix::codec::{self, Frame, Message};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const POLICY: &str = r#"import json,sys
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if kind=='fix_logon':
    out({'type':'fix_accept_logon'} if e['sender_comp_id'] in ('CLIENT','RAW') else {'type':'fix_reject_logon','text':'unknown counterparty '+e['sender_comp_id']})
f={x['tag']:x['value'] for x in e['fields']}
if e['msg_type']=='D':
    if f.get(55)=='REJECT': out({'type':'fix_reject','reason':'unknown_security','text':'symbol not traded here'})
    out({'type':'fix_send','msg_type':'ExecutionReport','fields':[{'name':'OrderID','value':'O-'+f[11]},{'name':'ClOrdID','value':f[11]},{'name':'ExecID','value':'E-'+f[11]},{'name':'ExecType','value':'0'},{'name':'OrdStatus','value':'0'},{'name':'Symbol','value':f[55]},{'name':'Side','value':f[54]},{'name':'LeavesQty','value':f[38]},{'name':'CumQty','value':'0'},{'name':'AvgPx','value':'0'}]})
if e['msg_type']=='B': out({'type':'fix_logout','text':'news means goodbye'})
out({'type':'fix_ignore'})
"#;

/// An acceptor policy: CLIENT and RAW may log on; a NewOrderSingle gets an ExecutionReport
/// (OrdStatus New) unless its Symbol is REJECT (BusinessMessageReject, unknown security); a
/// News message logs out; anything else is acknowledged with nothing.
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

/// Admit every logon and nothing else: application messages reach the (unreachable) model.
pub(crate) fn logon_only() -> Vec<Value> {
    vec![
        json!({"event_pattern": "fix_logon", "handler": {"type":"static","actions":[{"type":"fix_accept_logon"}]}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "fix".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
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

/// A FIX client whose events are all answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["fix_logged_on", "fix_message", "fix_logged_out"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "fix".into(),
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
    .map_err(|_| anyhow::anyhow!("FIX client did not log on"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(30), async {
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn peer(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set from tests/server/fix/install_peers.py (QuickFIX/Go 0.9.12); this evidence never skips"))
}

/// QuickFIX/Go's JSON lines.
pub(crate) fn lines(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// QuickFIX/Go as an acceptor (QFGO, accepting CLIENT) on a probed port.
pub(crate) async fn start_acceptor() -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        &peer("NETGET_FIX_QFGO"),
        super::real_server::InstallHint {
            brew: "go (then tests/server/fix/install_peers.py)",
            apt: "golang-go (then tests/server/fix/install_peers.py)",
        },
    )
    .args([
        "acceptor".to_owned(),
        "{port}".to_owned(),
        peer("NETGET_FIX_DICTIONARY"),
    ])
    .ready_when_log_matches("listening")
    .start()
    .await
}

/// A hand-driven FIX session for wire tests (SenderCompID RAW).
pub(crate) struct Raw {
    pub stream: TcpStream,
    pub buf: Vec<u8>,
    pub seq: u32,
    pub sender: String,
    pub target: String,
}

impl Raw {
    pub async fn connect(addr: SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(addr).await.unwrap(),
            buf: Vec::new(),
            seq: 1,
            sender: "RAW".into(),
            target: "NETGET".into(),
        }
    }
    /// Encode with an explicit sequence number (the caller advances `seq` when it wants to).
    pub fn encode(
        &self,
        msg_type: &str,
        seq: u32,
        extra_header: &[(u32, &str)],
        body: &[(u32, &str)],
    ) -> Vec<u8> {
        let mut f: Vec<(u32, String)> = vec![
            (35, msg_type.into()),
            (49, self.sender.clone()),
            (56, self.target.clone()),
            (34, seq.to_string()),
        ];
        f.extend(extra_header.iter().map(|(t, v)| (*t, v.to_string())));
        f.push((52, codec::timestamp()));
        f.extend(body.iter().map(|(t, v)| (*t, v.to_string())));
        codec::encode("FIX.4.4", &f).unwrap()
    }
    pub async fn send(&mut self, msg_type: &str, body: &[(u32, &str)]) {
        let bytes = self.encode(msg_type, self.seq, &[], body);
        self.seq += 1;
        self.stream.write_all(&bytes).await.unwrap();
    }
    pub async fn write(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.unwrap();
    }
    /// The next message, or None on EOF; panics after 10 s.
    pub async fn recv(&mut self) -> Option<Message> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match codec::frame(&self.buf) {
                    Frame::Message {
                        message, consumed, ..
                    } => {
                        self.buf.drain(..consumed);
                        return Some(message);
                    }
                    Frame::Garbled { reason, .. } => {
                        panic!("NetGet sent a garbled message: {reason}")
                    }
                    Frame::Incomplete => {
                        let mut chunk = [0u8; 4096];
                        let n = self.stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return None;
                        }
                        self.buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        })
        .await
        .expect("no FIX message within 10 s")
    }
    /// Receive until a message of `msg_type`, skipping heartbeats.
    pub async fn expect(&mut self, msg_type: &str) -> Message {
        loop {
            let m = self
                .recv()
                .await
                .unwrap_or_else(|| panic!("closed while waiting for {msg_type}"));
            if m.msg_type() == msg_type {
                return m;
            }
            assert_eq!(m.msg_type(), "0", "expected {msg_type}, got {:?}", m.fields);
        }
    }
    pub async fn logon(&mut self, heartbeat: &str) -> Message {
        self.send("A", &[(98, "0"), (108, heartbeat), (141, "Y")])
            .await;
        self.recv().await.expect("an answer to the Logon")
    }
}
