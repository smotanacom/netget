//! Minecraft server over raw packets: status and ping/pong, the three legacy ping forms, a
//! login refused with the handler's reason, and the bounds and fail-closed paths — an
//! oversized packet, an over-long address, a silent client, a refusal, and a handler failure.
use netget::cli::management::ServerForm;
use netget::server::minecraft::wire;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Answers status with the address the client dialled in the MOTD, so the handshake is seen
/// to reach the handler.
pub const STATUS_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'minecraft_status','motd':'NetGet via '+str(e.get('server_address')),'online_players':3,'max_players':50,'version_name':'1.21.1','protocol':767,'sample':[{'name':'alice','id':'a6f39b1e-3a43-4e2b-9b0c-1d2f3a4b5c6d'},{'name':'bob'}]}\nif e['legacy']:\n  a['motd']='Legacy NetGet'\n  a.pop('protocol')\n  a['version_name']='1.6.4'\nprint(json.dumps({'actions':[a]}))";

/// Refuses every login, naming the player, so the Login Start is seen to reach the handler.
pub const LOGIN_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'minecraft_disconnect','reason':'Sorry '+e['username']+', whitelist only'}]}))";

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"minecraft_status_request","handler":{"type":"script","language":"python","code":STATUS_SCRIPT}}),
        json!({"event_pattern":"minecraft_login","handler":{"type":"script","language":"python","code":LOGIN_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "minecraft".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a Minecraft server".into()),
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

fn handshake(port: u16, next_state: i32) -> Vec<u8> {
    wire::encode_handshake(&wire::Handshake {
        protocol_version: 767,
        server_address: "play.example.test".into(),
        server_port: port,
        next_state,
    })
}

async fn packet(stream: &mut TcpStream) -> (i32, Vec<u8>) {
    wire::read_packet(
        stream,
        wire::MAX_CLIENTBOUND_PACKET,
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .expect("a packet, not EOF")
}

/// The server closed the connection: EOF (or a reset) within a few seconds, with no bytes.
async fn closed(stream: &mut TcpStream) {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("expected a close, read {n} bytes: {:?}", &buf[..n]),
        Err(_) => panic!("the connection stayed open"),
    }
}

async fn legacy(addr: SocketAddr, request: &[u8]) -> Value {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request).await.unwrap();
    assert_eq!(s.read_u8().await.unwrap(), 0xFF);
    let units = s.read_u16().await.unwrap() as usize;
    let mut text = vec![0u8; units * 2];
    s.read_exact(&mut text).await.unwrap();
    closed(&mut s).await;
    wire::decode_legacy_reply(&text).unwrap()
}

#[tokio::test]
async fn status_ping_legacy_and_login() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    // Handshake and status request in one write, as clients send them.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = handshake(addr.port(), wire::STATE_STATUS);
    out.extend_from_slice(&wire::frame(0x00, &[]));
    s.write_all(&out).await.unwrap();
    let (pid, body) = packet(&mut s).await;
    assert_eq!(pid, 0x00);
    let status: Value = serde_json::from_str(
        &wire::Reader::new(&body)
            .string(wire::MAX_JSON_CHARS)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        status,
        json!({
            "version": {"name": "1.21.1", "protocol": 767},
            "players": {"max": 50, "online": 3, "sample": [
                {"name": "alice", "id": "a6f39b1e-3a43-4e2b-9b0c-1d2f3a4b5c6d"},
                {"name": "bob", "id": "00000000-0000-0000-0000-000000000000"}
            ]},
            "description": {"text": "NetGet via play.example.test"}
        })
    );
    s.write_all(&wire::frame(0x01, &0x0102030405060708i64.to_be_bytes()))
        .await
        .unwrap();
    assert_eq!(
        packet(&mut s).await,
        (0x01, 0x0102030405060708i64.to_be_bytes().to_vec()),
        "pong echoes the payload"
    );
    closed(&mut s).await;

    // A ping without a status request is answered too.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = handshake(addr.port(), wire::STATE_STATUS);
    out.extend_from_slice(&wire::frame(0x01, &7i64.to_be_bytes()));
    s.write_all(&out).await.unwrap();
    assert_eq!(packet(&mut s).await, (0x01, 7i64.to_be_bytes().to_vec()));

    // Legacy: 1.6 (with MC|PingHost), 1.4 (FE 01) and Beta (bare FE).
    let reply = legacy(addr, &wire::encode_legacy_ping("legacy.test", 25565)).await;
    assert_eq!(
        reply,
        json!({"protocol":74,"version_name":"1.6.4","motd":"Legacy NetGet","online_players":3,"max_players":50})
    );
    let reply = legacy(addr, &[0xFE, 0x01, 0xFA]).await;
    assert_eq!(
        reply["motd"], "Legacy NetGet",
        "FE 01 FA with no MC|PingHost after it"
    );
    let reply = legacy(addr, &[0xFE, 0x01]).await;
    assert_eq!(
        (reply["protocol"].clone(), reply["motd"].clone()),
        (json!(74), json!("Legacy NetGet"))
    );
    let reply = legacy(addr, &[0xFE]).await;
    assert_eq!(
        reply,
        json!({"protocol":null,"version_name":null,"motd":"Legacy NetGet","online_players":3,"max_players":50})
    );

    // Login: refused with the handler's reason, naming the player from Login Start.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = handshake(addr.port(), wire::STATE_LOGIN);
    out.extend_from_slice(&wire::encode_login_start(767, "steve", &[7u8; 16]));
    s.write_all(&out).await.unwrap();
    let (pid, body) = packet(&mut s).await;
    assert_eq!(pid, 0x00, "login Disconnect");
    let reason: Value = serde_json::from_str(
        &wire::Reader::new(&body)
            .string(wire::MAX_JSON_CHARS)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reason, json!({"text": "Sorry steve, whitelist only"}));
    closed(&mut s).await;
    state.remove_server(id).await;
}

#[tokio::test]
async fn bounds_refusals_and_handler_failure() {
    let (state, id, addr) = start(handlers(), json!({"idle_timeout_secs": 1})).await;
    // A packet announcing one byte over the serverbound bound is refused before it is read.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = Vec::new();
    wire::put_varint(&mut out, wire::MAX_SERVERBOUND_PACKET as i32 + 1);
    s.write_all(&out).await.unwrap();
    closed(&mut s).await;
    // A handshake address over 255 characters.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&wire::encode_handshake(&wire::Handshake {
        protocol_version: 767,
        server_address: "a".repeat(wire::MAX_ADDRESS_CHARS + 1),
        server_port: 1,
        next_state: wire::STATE_STATUS,
    }))
    .await
    .unwrap();
    closed(&mut s).await;
    // A player name over 16 characters.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = handshake(addr.port(), wire::STATE_LOGIN);
    out.extend_from_slice(&wire::encode_login_start(767, &"n".repeat(17), &[0; 16]));
    s.write_all(&out).await.unwrap();
    closed(&mut s).await;
    // A client that connects and says nothing is closed after idle_timeout_secs.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let started = std::time::Instant::now();
    closed(&mut s).await;
    assert!(started.elapsed() < Duration::from_secs(4));
    // A second status request on one connection closes it.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = handshake(addr.port(), wire::STATE_STATUS);
    out.extend_from_slice(&wire::frame(0x00, &[]));
    s.write_all(&out).await.unwrap();
    packet(&mut s).await;
    s.write_all(&wire::frame(0x00, &[])).await.unwrap();
    closed(&mut s).await;
    state.remove_server(id).await;

    // A refusal closes both kinds of connection unanswered.
    let refuse = vec![
        json!({"event_pattern":"*","handler":{"type":"static","actions":[{"type":"minecraft_refuse"}]}}),
    ];
    // No handler at all: the model is unreachable, so the handler fails.
    for (handlers, login_reason) in [
        (refuse, None),
        (vec![], Some("netget: request could not be processed")),
    ] {
        let (state, id, addr) = start(handlers, json!({})).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut out = handshake(addr.port(), wire::STATE_STATUS);
        out.extend_from_slice(&wire::frame(0x00, &[]));
        s.write_all(&out).await.unwrap();
        closed(&mut s).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&[0xFE, 0x01]).await.unwrap();
        closed(&mut s).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut out = handshake(addr.port(), wire::STATE_LOGIN);
        out.extend_from_slice(&wire::encode_login_start(767, "steve", &[0; 16]));
        s.write_all(&out).await.unwrap();
        match login_reason {
            None => closed(&mut s).await,
            Some(expected) => {
                let (pid, body) = packet(&mut s).await;
                assert_eq!(pid, 0x00);
                let reason: Value = serde_json::from_str(
                    &wire::Reader::new(&body)
                        .string(wire::MAX_JSON_CHARS)
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    reason,
                    json!({"text": expected}),
                    "a generic reason, never the error"
                );
            }
        }
        state.remove_server(id).await;
    }
}
