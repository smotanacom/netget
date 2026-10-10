//! The Minecraft client against NetGet's own server (status, legacy status, a refused login)
//! and against a scripted fixture for what NetGet's server never sends: compression, a
//! compressed Login Success, a plugin and a cookie request the client must answer, an
//! Encryption Request, and the bounds on an oversized or lying packet.
use netget::{
    cli::management::{ClientForm, ServerForm},
    server::minecraft::wire,
    state::{app_state::AppState, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::{io::Write, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

const STATUS_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nif e['legacy']:\n  a={'type':'minecraft_status','motd':'Legacy NetGet','online_players':3,'max_players':50,'version_name':'1.6.4'}\nelse:\n  a={'type':'minecraft_status','motd':'NetGet via '+str(e.get('server_address')),'online_players':3,'max_players':50,'sample':[{'name':'alice'}]}\nprint(json.dumps({'actions':[a]}))";
const LOGIN_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'minecraft_disconnect','reason':'Sorry '+e['username']+', whitelist only'}]}))";

/// NetGet's own Minecraft server, scripted.
async fn netget_server() -> (AppState, netget::state::ServerId, std::net::SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "minecraft".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a Minecraft server".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"minecraft_status_request","handler":{"type":"script","language":"python","code":STATUS_SCRIPT}}),
            json!({"event_pattern":"minecraft_login","handler":{"type":"script","language":"python","code":LOGIN_SCRIPT}}),
        ]),
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
    (
        state,
        id,
        std::net::SocketAddr::from(([127, 0, 0, 1], addr.port())),
    )
}

pub async fn client(remote: String, ready: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "minecraft".into(),
        remote_addr: Some(remote),
        instruction: Some("Probe the server".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"minecraft_ready","handler":{"type":"static","actions":ready}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ]),
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
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(entry) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| e.contains(needle))
            {
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing {needle:?}"))
}

pub async fn send(state: &AppState, id: ClientId, action: Value) {
    state
        .send_to_client(id, action, Duration::from_secs(20))
        .await
        .unwrap();
}

#[tokio::test]
async fn status_legacy_and_refused_login_against_netget() {
    let (server_state, server_id, addr) = netget_server().await;
    let (state, id) = client(addr.to_string(), json!([{"type":"minecraft_status"}])).await;
    let event = wait_log(&state, id, "minecraft_status_response").await;
    for needle in [
        r#""motd":"NetGet via 127.0.0.1""#,
        r#""online_players":3"#,
        r#""max_players":50"#,
        r#""protocol":767"#,
        r#""name":"alice""#,
        r#""legacy":false"#,
    ] {
        assert!(event.contains(needle), "{needle} in {event}");
    }
    assert!(
        !event.contains(r#""latency_ms":null"#),
        "pong measured: {event}"
    );
    send(&state, id, json!({"type":"minecraft_legacy_status"})).await;
    let event = wait_log(&state, id, "Legacy NetGet").await;
    assert!(
        event.contains(r#""version_name":"1.6.4""#) && event.contains(r#""legacy":true"#),
        "{event}"
    );
    send(
        &state,
        id,
        json!({"type":"minecraft_login","username":"steve"}),
    )
    .await;
    let event = wait_log(&state, id, "minecraft_login_result").await;
    assert!(
        event.contains(r#""outcome":"disconnected""#)
            && event.contains(r#""reason":"Sorry steve, whitelist only""#),
        "{event}"
    );
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}

fn zlib(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

/// A compressed-layout packet: length, data length (0 = stored), then id + body.
fn compressed(id: i32, body: &[u8], deflate: bool) -> Vec<u8> {
    let mut inner = Vec::new();
    wire::put_varint(&mut inner, id);
    inner.extend_from_slice(body);
    let mut payload = Vec::new();
    if deflate {
        wire::put_varint(&mut payload, inner.len() as i32);
        payload.extend_from_slice(&zlib(&inner));
    } else {
        payload.push(0);
        payload.extend_from_slice(&inner);
    }
    let mut out = Vec::new();
    wire::put_varint(&mut out, payload.len() as i32);
    out.extend_from_slice(&payload);
    out
}

async fn read_compressed(s: &mut TcpStream) -> (i32, Vec<u8>) {
    let raw = wire::read_raw(s, 4096, Duration::from_secs(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw[0], 0, "the client's replies are stored, not deflated");
    wire::split_id(&raw[1..]).unwrap()
}

#[tokio::test]
async fn login_through_compression_plugin_and_cookie_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        // 1: compression, a plugin request, a cookie request, then a deflated Login Success.
        let (mut s, _) = listener.accept().await.unwrap();
        let (id, body) = wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap()
            .unwrap();
        let hs = wire::parse_handshake(&body).unwrap();
        assert_eq!((id, hs.next_state, hs.protocol_version), (0, 2, 767));
        let (id, body) = wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id, 0);
        let (name, uuid) = wire::parse_login_start(&body).unwrap();
        assert_eq!(
            (name.as_str(), uuid.as_deref()),
            ("alice", Some("01020304-0506-0708-090a-0b0c0d0e0f10"))
        );
        let mut threshold = Vec::new();
        wire::put_varint(&mut threshold, 64);
        s.write_all(&wire::frame(0x03, &threshold)).await.unwrap();
        let mut plugin = Vec::new();
        wire::put_varint(&mut plugin, 42);
        wire::put_string(&mut plugin, "velocity:player_info");
        plugin.push(1);
        s.write_all(&compressed(0x04, &plugin, false))
            .await
            .unwrap();
        assert_eq!(
            read_compressed(&mut s).await,
            (0x02, vec![42, 0]),
            "plugin request answered: not understood"
        );
        let mut cookie = Vec::new();
        wire::put_string(&mut cookie, "netget:session");
        s.write_all(&compressed(0x05, &cookie, false))
            .await
            .unwrap();
        let mut expected = Vec::new();
        wire::put_string(&mut expected, "netget:session");
        expected.push(0);
        assert_eq!(
            read_compressed(&mut s).await,
            (0x04, expected),
            "cookie request answered: none"
        );
        let mut success = vec![0xAB; 16];
        wire::put_string(&mut success, "alice");
        wire::put_varint(&mut success, 0);
        success.push(1);
        success.extend_from_slice(&[0u8; 80]); // padding past the threshold, so it is deflated
        s.write_all(&compressed(0x02, &success, true))
            .await
            .unwrap();
        // 2: an online-mode server asks for encryption.
        let (mut s, _) = listener.accept().await.unwrap();
        wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        let mut req = Vec::new();
        wire::put_string(&mut req, "");
        s.write_all(&wire::frame(0x01, &req)).await.unwrap();
        // 3: a status answer over the client's bound, announced and never sent.
        let (mut s, _) = listener.accept().await.unwrap();
        let mut len = Vec::new();
        wire::put_varint(&mut len, wire::MAX_CLIENTBOUND_PACKET as i32 + 1);
        s.write_all(&len).await.unwrap();
        let mut sink = [0u8; 256];
        let _ = s.read(&mut sink).await;
        // 4: a compressed packet whose declared size is a lie.
        let (mut s, _) = listener.accept().await.unwrap();
        wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        let mut threshold = Vec::new();
        wire::put_varint(&mut threshold, 0);
        s.write_all(&wire::frame(0x03, &threshold)).await.unwrap();
        let deflated = zlib(&[0x02; 300]);
        let mut payload = Vec::new();
        wire::put_varint(&mut payload, 20);
        payload.extend_from_slice(&deflated);
        let mut out = Vec::new();
        wire::put_varint(&mut out, payload.len() as i32);
        out.extend_from_slice(&payload);
        s.write_all(&out).await.unwrap();
        let _ = s.read(&mut sink).await;
        // 5: still up afterwards — a plain Disconnect.
        let (mut s, _) = listener.accept().await.unwrap();
        wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        wire::read_packet(&mut s, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        let mut reason = Vec::new();
        wire::put_string(
            &mut reason,
            r#"{"translate":"multiplayer.disconnect.server_full","extra":[{"text":"!"}]}"#,
        );
        s.write_all(&wire::frame(0x00, &reason)).await.unwrap();
    });
    let (state, id) = client(
        addr.to_string(),
        json!([{"type":"minecraft_login","username":"alice","uuid":"0102030405060708090a0b0c0d0e0f10"}]),
    )
    .await;
    let event = wait_log(&state, id, r#""outcome":"accepted""#).await;
    assert!(
        event.contains(r#""username":"alice""#)
            && event.contains(r#""uuid":"abababab-abab-abab-abab-abababababab""#)
            && event.contains(r#""compression_threshold":64"#),
        "{event}"
    );
    send(
        &state,
        id,
        json!({"type":"minecraft_login","username":"bob"}),
    )
    .await;
    wait_log(&state, id, r#""outcome":"encryption_required""#).await;
    let refused = state
        .send_to_client(
            id,
            json!({"type":"minecraft_status"}),
            Duration::from_secs(20),
        )
        .await;
    assert!(
        refused.is_err(),
        "an oversized status is refused: {refused:?}"
    );
    let refused = state
        .send_to_client(
            id,
            json!({"type":"minecraft_login","username":"carol"}),
            Duration::from_secs(20),
        )
        .await;
    assert!(
        refused.is_err(),
        "a lying compressed packet is refused: {refused:?}"
    );
    send(
        &state,
        id,
        json!({"type":"minecraft_login","username":"dave"}),
    )
    .await;
    let event = wait_log(&state, id, "multiplayer.disconnect.server_full!").await;
    assert!(event.contains(r#""outcome":"disconnected""#), "{event}");
    assert!(
        state
            .send_to_client(
                id,
                json!({"type":"minecraft_login","username":"seventeen_chars_x"}),
                Duration::from_secs(5)
            )
            .await
            .map(|o| format!("{o:?}"))
            .unwrap_or_default()
            .contains("Rejected"),
        "a name over 16 characters is rejected before anything is sent"
    );
    fixture.await.unwrap();
    state.remove_client(id).await;
}
