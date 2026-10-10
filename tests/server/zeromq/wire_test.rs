//! ZeroMQ server over raw ZMTP: the handshake and its refusals (a bad greeting, a security
//! mechanism other than NULL, an incompatible socket type), REP envelopes, ROUTER without
//! lockstep, PULL with no reply, PING/PONG, and the bounds and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::zeromq::wire::{self, Incoming};
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Replies ["ECHO", frames..., socket_type, peer_socket_type, identity]; a PULL socket ignores.
pub const ECHO_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nif e['socket_type']=='PULL':\n  a={'type':'zmq_ignore'}\nelse:\n  a={'type':'zmq_reply','frames':['ECHO']+e['frames']+[e['socket_type'],e['peer_socket_type'],e.get('peer_identity') or '-']}\nprint(json.dumps({'actions':[a]}))";

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"zmq_message","handler":{"type":"script","language":"python","code":ECHO_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "zeromq".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer messages".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

pub async fn logged(state: &AppState, id: ServerId, needle: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .any(|e| e.contains(needle))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

async fn peer(addr: SocketAddr, socket_type: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let (theirs, _) = wire::handshake(&mut s, socket_type, None).await.unwrap();
    assert!(wire::compatible(socket_type, &theirs));
    s
}

async fn recv(s: &mut TcpStream) -> Incoming {
    wire::read_incoming(s, Duration::from_secs(20))
        .await
        .unwrap()
        .expect("a message")
}

fn strings(frames: &[&str]) -> Vec<Vec<u8>> {
    frames.iter().map(|f| f.as_bytes().to_vec()).collect()
}

async fn closed(s: &mut TcpStream) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return all,
            Ok(Ok(n)) => all.extend_from_slice(&buf[..n]),
            Err(_) => panic!("the connection stayed open"),
        }
    }
}

#[tokio::test]
async fn rep_router_pull_and_heartbeats() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let mut s = peer(addr, "REQ").await;
    s.write_all(&wire::encode_message(&strings(&["", "hello", "world"])))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut s).await,
        Incoming::Message(strings(&["", "ECHO", "hello", "world", "REP", "REQ", "-"]))
    );
    s.write_all(&wire::encode_command("PING", b"\x00\x0aabc"))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut s).await,
        Incoming::Command("PONG".into(), b"abc".to_vec())
    );
    // Binary frames reach the handler as hex and come back as the same bytes.
    s.write_all(&wire::encode_message(&[Vec::new(), vec![0xff, 0x00]]))
        .await
        .unwrap();
    match recv(&mut s).await {
        Incoming::Message(f) => {
            assert_eq!(&f[..3], &[Vec::new(), b"ECHO".to_vec(), b"ff00".to_vec()])
        }
        other => panic!("{other:?}"),
    }
    // A REP request without its delimiter is a protocol error.
    s.write_all(&wire::encode_message(&strings(&["no delimiter"])))
        .await
        .unwrap();
    closed(&mut s).await;
    state.remove_server(id).await;

    let (state, id, addr) = start(handlers(), json!({"socket_type": "router"})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    wire::handshake(&mut s, "DEALER", Some(b"worker-7"))
        .await
        .unwrap();
    // Without lockstep: two messages before any reply.
    s.write_all(&wire::encode_message(&strings(&["one"])))
        .await
        .unwrap();
    s.write_all(&wire::encode_message(&strings(&["two"])))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut s).await,
        Incoming::Message(strings(&["ECHO", "one", "ROUTER", "DEALER", "worker-7"]))
    );
    assert_eq!(
        recv(&mut s).await,
        Incoming::Message(strings(&["ECHO", "two", "ROUTER", "DEALER", "worker-7"]))
    );
    state.remove_server(id).await;

    let (state, id, addr) = start(handlers(), json!({"socket_type": "pull"})).await;
    let mut s = peer(addr, "PUSH").await;
    s.write_all(&wire::encode_message(&strings(&["job", "42"])))
        .await
        .unwrap();
    assert!(
        logged(&state, id, r#""frames":["job","42"]"#).await,
        "the pushed message reached the handler"
    );
    let mut buf = [0u8; 16];
    assert!(
        tokio::time::timeout(Duration::from_millis(500), s.read(&mut buf))
            .await
            .is_err(),
        "PULL sends nothing back"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn refusals_bounds_and_failures() {
    let (state, id, addr) = start(handlers(), json!({"idle_timeout_secs": 1})).await;
    // Incompatible socket type: ERROR, then close.
    let mut s = TcpStream::connect(addr).await.unwrap();
    assert!(wire::handshake(&mut s, "PUB", None).await.is_err());
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&wire::greeting()).await.unwrap();
    s.write_all(&wire::ready("PUB", None)).await.unwrap();
    let mut greeting = [0u8; wire::GREETING_LEN];
    s.read_exact(&mut greeting).await.unwrap();
    let rest = closed(&mut s).await;
    assert!(
        String::from_utf8_lossy(&rest).contains("REP cannot talk to PUB"),
        "{rest:?}"
    );
    // A mechanism other than NULL.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut g = wire::greeting();
    g[12..17].copy_from_slice(b"CURVE");
    s.write_all(&g).await.unwrap();
    let rest = closed(&mut s).await;
    assert!(
        String::from_utf8_lossy(&rest).contains("not supported"),
        "ERROR names the refusal"
    );
    // Not ZMTP at all.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&[0u8; wire::GREETING_LEN]).await.unwrap();
    closed(&mut s).await;
    // A frame announced over 1 MiB is refused before it is read.
    let mut s = peer(addr, "REQ").await;
    let mut big = vec![0x02];
    big.extend_from_slice(&(wire::MAX_MESSAGE_BYTES as u64 + 1).to_be_bytes());
    s.write_all(&big).await.unwrap();
    closed(&mut s).await;
    // 65 frames in one message.
    let mut s = peer(addr, "REQ").await;
    let many: Vec<Vec<u8>> = (0..=wire::MAX_FRAMES).map(|_| b"x".to_vec()).collect();
    s.write_all(&wire::encode_message(&many)).await.unwrap();
    closed(&mut s).await;
    // A silent peer is closed after idle_timeout_secs.
    let mut s = peer(addr, "REQ").await;
    closed(&mut s).await;
    state.remove_server(id).await;
    // A REP request the handler cannot answer closes the connection; it never fabricates one.
    let (state, id, addr) = start(vec![], json!({})).await;
    let mut s = peer(addr, "REQ").await;
    s.write_all(&wire::encode_message(&strings(&["", "hello"])))
        .await
        .unwrap();
    assert!(closed(&mut s).await.is_empty());
    state.remove_server(id).await;
}
