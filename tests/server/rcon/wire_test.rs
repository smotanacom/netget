//! RCON server over raw packets: login in both dialects (a Rust-checked password and a
//! handler-decided one), command output split at 4086 bytes, the sentinel mirror, the
//! packet bounds, and closing rather than inventing output when the handler fails.
use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Echoes the command back, and answers "long" with 5000 characters.
pub const COMMAND_SCRIPT: &str = "import json,sys\nc=json.load(sys.stdin)['event']['command']\nout='x'*5000 if c=='long' else 'ran: '+c\nprint(json.dumps({'actions':[{'type':'rcon_response','output':out}]}))";

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"rcon_command","handler":{"type":"script","language":"python","code":COMMAND_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "rcon".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer as a game server console".into()),
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

fn packet(id: i32, kind: i32, body: &str) -> Vec<u8> {
    let mut out = ((body.len() + 10) as i32).to_le_bytes().to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

async fn read(s: &mut TcpStream) -> (i32, i32, String) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut size = [0u8; 4];
        s.read_exact(&mut size).await.unwrap();
        let mut rest = vec![0u8; i32::from_le_bytes(size) as usize];
        s.read_exact(&mut rest).await.unwrap();
        let id = i32::from_le_bytes(rest[0..4].try_into().unwrap());
        let kind = i32::from_le_bytes(rest[4..8].try_into().unwrap());
        assert_eq!(
            &rest[rest.len() - 2..],
            &[0, 0],
            "body and empty string both NUL-terminated"
        );
        (
            id,
            kind,
            String::from_utf8(rest[8..rest.len() - 2].to_vec()).unwrap(),
        )
    })
    .await
    .expect("RCON packet deadline")
}

async fn closed(s: &mut TcpStream) {
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut rest))
        .await
        .expect("server closes");
    assert!(rest.is_empty(), "nothing after close: {rest:?}");
}

#[tokio::test]
async fn source_login_commands_split_output_and_sentinel() {
    let (state, id, addr) = start(handlers(), json!({"password":"hunter2"})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&packet(7, 3, "wrong")).await.unwrap();
    assert_eq!(
        read(&mut s).await,
        (7, 0, String::new()),
        "srcds' empty response first"
    );
    assert_eq!(read(&mut s).await, (-1, 2, String::new()), "refused");
    s.write_all(&packet(8, 3, "hunter2")).await.unwrap();
    assert_eq!(read(&mut s).await, (8, 0, String::new()));
    assert_eq!(
        read(&mut s).await,
        (8, 2, String::new()),
        "accepted: the request id"
    );
    s.write_all(&packet(9, 2, "status")).await.unwrap();
    assert_eq!(read(&mut s).await, (9, 0, "ran: status".into()));
    // Output over 4086 bytes is split, and the sentinel's mirror marks its end.
    s.write_all(&packet(10, 2, "long")).await.unwrap();
    s.write_all(&packet(11, 0, "")).await.unwrap();
    let first = read(&mut s).await;
    let second = read(&mut s).await;
    assert_eq!((first.0, first.2.len()), (10, 4086));
    assert_eq!((second.0, second.2.len()), (10, 914));
    assert_eq!(
        read(&mut s).await,
        (11, 0, String::new()),
        "sentinel mirrored after the output"
    );
    drop(s);
    state.remove_server(id).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "stop releases the port"
    );
}

#[tokio::test]
async fn minecraft_dialect_and_a_handler_decided_login() {
    let mut handlers = handlers();
    handlers.push(json!({"event_pattern":"rcon_auth","handler":{"type":"script","language":"python","code":"import json,sys\np=json.load(sys.stdin)['event']['password']\nprint(json.dumps({'actions':[{'type':'rcon_auth_decision','allowed':p=='letmein'}]}))"}}));
    let (state, id, addr) = start(handlers, json!({"dialect":"minecraft"})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&packet(1, 3, "nope")).await.unwrap();
    assert_eq!(
        read(&mut s).await,
        (-1, 2, String::new()),
        "no empty packet in the minecraft dialect"
    );
    s.write_all(&packet(2, 3, "letmein")).await.unwrap();
    assert_eq!(read(&mut s).await, (2, 2, String::new()));
    s.write_all(&packet(3, 2, "list")).await.unwrap();
    assert_eq!(read(&mut s).await, (3, 0, "ran: list".into()));
    state.remove_server(id).await;
}

#[tokio::test]
async fn bounds_and_ordering_close_the_connection() {
    let (state, id, addr) = start(handlers(), json!({"password":"pw"})).await;
    // A command before login.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&packet(1, 2, "status")).await.unwrap();
    closed(&mut s).await;
    // A size field past 4096, refused before the packet is read.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&4097i32.to_le_bytes()).await.unwrap();
    closed(&mut s).await;
    // A size field below the 10-byte minimum.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&9i32.to_le_bytes()).await.unwrap();
    closed(&mut s).await;
    // Three failed logins.
    let mut s = TcpStream::connect(addr).await.unwrap();
    for n in 0..3 {
        s.write_all(&packet(n, 3, "bad")).await.unwrap();
        read(&mut s).await;
        read(&mut s).await;
    }
    closed(&mut s).await;
    state.remove_server(id).await;
}

#[tokio::test]
async fn handler_failure_closes_instead_of_inventing_output() {
    // No command handler and an unreachable model; no auth handler either.
    let (state, id, addr) = start(vec![], json!({"password":"pw"})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&packet(1, 3, "pw")).await.unwrap();
    read(&mut s).await;
    assert_eq!(read(&mut s).await, (1, 2, String::new()));
    s.write_all(&packet(2, 2, "status")).await.unwrap();
    closed(&mut s).await;
    state.remove_server(id).await;
    let (state, id, addr) = start(vec![], json!({})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&packet(1, 3, "anything")).await.unwrap();
    read(&mut s).await;
    assert_eq!(
        read(&mut s).await,
        (-1, 2, String::new()),
        "a failed auth decision refuses"
    );
    state.remove_server(id).await;
}
