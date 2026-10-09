//! The RCON client against a scripted fixture asserting every packet (login with id 0,
//! each command followed by the sentinel, output in several packets reassembled, a stale
//! packet skipped), against NetGet's own server for split output, and a refused login.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

pub async fn client(
    remote: String,
    params: Value,
    connected: Value,
) -> Result<(AppState, ClientId), String> {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "rcon".into(),
        remote_addr: Some(remote),
        instruction: Some("Run console commands".into()),
        startup_params: Some(params),
        event_handlers: Some(vec![
            json!({"event_pattern":"rcon_connected","handler":{"type":"static","actions":connected}}),
            json!({"event_pattern":"rcon_response","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .map_err(|e| format!("{e:#}"))?;
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state.has_client_handle(id).await {
                return Ok(());
            }
            let status = format!("{:?}", state.get_client(id).await.map(|c| c.status));
            if status.contains("Error") {
                return Err(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "client never connected".to_string())?;
    ready.map(|()| (state, id))
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(15), async {
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

fn packet(id: i32, kind: i32, body: &str) -> Vec<u8> {
    let mut out = ((body.len() + 10) as i32).to_le_bytes().to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

async fn read(s: &mut TcpStream) -> (i32, i32, String) {
    let mut size = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut size))
        .await
        .expect("fixture read deadline")
        .unwrap();
    let mut rest = vec![0u8; i32::from_le_bytes(size) as usize];
    s.read_exact(&mut rest).await.unwrap();
    (
        i32::from_le_bytes(rest[0..4].try_into().unwrap()),
        i32::from_le_bytes(rest[4..8].try_into().unwrap()),
        String::from_utf8(rest[8..rest.len() - 2].to_vec()).unwrap(),
    )
}

#[tokio::test]
async fn login_sentinel_and_reassembled_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        assert_eq!(
            read(&mut s).await,
            (0, 3, "secret".into()),
            "login with id 0"
        );
        s.write_all(&[packet(0, 0, ""), packet(0, 2, "")].concat())
            .await
            .unwrap();
        let (id, kind, body) = read(&mut s).await;
        assert_eq!((kind, body.as_str()), (2, "status"));
        let (sentinel, kind, body) = read(&mut s).await;
        assert_eq!(
            (sentinel, kind, body.as_str()),
            (id + 1, 0, ""),
            "sentinel follows the command"
        );
        // A stale packet for an earlier id, the output in two packets, then the mirror.
        s.write_all(
            &[
                packet(id - 1, 0, ""),
                packet(id, 0, "hostname: test\n"),
                packet(id, 0, "players: 3"),
                packet(sentinel, 0, ""),
            ]
            .concat(),
        )
        .await
        .unwrap();
        let (id, _, body) = read(&mut s).await;
        assert_eq!(body, "injected");
        let (sentinel, _, _) = read(&mut s).await;
        s.write_all(&[packet(id, 0, "ok"), packet(sentinel, 0, "")].concat())
            .await
            .unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    });
    let (state, id) = client(
        address.to_string(),
        json!({"password":"secret"}),
        json!([{"type":"rcon_command","command":"status"}]),
    )
    .await
    .unwrap();
    let event = wait_log(&state, id, "players: 3").await;
    assert!(
        event.contains(r#""output":"hostname: test\nplayers: 3""#)
            && event.contains(r#""packets":2"#),
        "{event}"
    );
    assert!(
        event.contains(r#""output":"hostname: test\nplayers: 3""#)
            || event.contains("hostname: test\\nplayers: 3"),
        "{event}"
    );
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"rcon_command","command":""}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(rejected, ClientSendOutcome::Rejected { .. }),
        "{rejected:?}"
    );
    let sent = state
        .send_to_client(
            id,
            json!({"type":"rcon_command","command":"injected"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    wait_log(&state, id, r#""output":"ok""#).await;
    let quit = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(quit, ClientSendOutcome::Disconnected), "{quit:?}");
    fixture.await.unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn a_refused_password_fails_the_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        read(&mut s).await;
        s.write_all(&[packet(0, 0, ""), packet(-1, 2, "")].concat())
            .await
            .unwrap();
    });
    let error = client(address.to_string(), json!({"password":"wrong"}), json!([]))
        .await
        .err()
        .expect("refused login fails");
    assert!(error.contains("refused the password"), "{error}");
}

#[tokio::test]
async fn netget_pair_reassembles_split_output() {
    let server = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let sid = ServerForm {
        protocol: "rcon".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("console".into()),
        startup_params: Some(json!({"password":"pw"})),
        event_handlers: Some(vec![json!({"event_pattern":"rcon_command","handler":{"type":"static","actions":[{"type":"rcon_response","output":"z".repeat(9000)}]}})]),
        ..Default::default()
    }
    .create(&server, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = server.get_server(sid).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (state, id) = client(
        format!("127.0.0.1:{port}"),
        json!({"password":"pw"}),
        json!([{"type":"rcon_command","command":"dump"}]),
    )
    .await
    .unwrap();
    let event = wait_log(&state, id, r#""packets":3"#).await;
    assert!(
        event.contains(&"z".repeat(9000)),
        "9000 bytes reassembled from three packets"
    );
    state.remove_client(id).await;
    server.remove_server(sid).await;
}
