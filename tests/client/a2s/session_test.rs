//! The A2S client against a scripted UDP fixture (the challenge retry, split reassembly out of
//! order) and against NetGet's own server.
use netget::{
    cli::management::{ClientForm, ServerForm},
    server::a2s::wire::{self, Kind},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{net::UdpSocket, sync::mpsc};

pub async fn client(remote: String, ready: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "a2s".into(),
        remote_addr: Some(remote),
        instruction: Some("Query the server".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"a2s_ready","handler":{"type":"static","actions":ready}}),
            json!({"event_pattern":"a2s_response","handler":{"type":"static","actions":[]}}),
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

#[tokio::test]
async fn challenge_retry_and_out_of_order_split_reassembly() {
    let fixture = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = fixture.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        // Players: first the request for a challenge, then the retry carrying it.
        let (n, peer) = fixture.recv_from(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            wire::encode_request(Kind::Players, Some(wire::NO_CHALLENGE)).as_slice()
        );
        fixture
            .send_to(&wire::encode_challenge(0x1234_5678), peer)
            .await
            .unwrap();
        let (n, _) = fixture.recv_from(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            wire::encode_request(Kind::Players, Some(0x1234_5678)).as_slice()
        );
        let names: Vec<Value> = (0..60)
            .map(|i| json!({"name": format!("player-with-a-long-name-{i:02}"), "score": i}))
            .collect();
        let payload = wire::encode_players(&json!({"players": names})).unwrap();
        let mut packets = wire::packetize(&payload, 77).unwrap();
        assert!(packets.len() > 1);
        packets.reverse();
        for packet in packets {
            fixture.send_to(&packet, peer).await.unwrap();
        }
        // Info: no challenge needed.
        let (n, peer) = fixture.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], wire::encode_request(Kind::Info, None).as_slice());
        let info = wire::encode_info(&json!({"name":"Fixture","map":"ctf_2fort","folder":"tf","game":"Team Fortress","app_id":440,"steam_id":90071992547409920u64})).unwrap();
        fixture.send_to(&info, peer).await.unwrap();
    });
    let (state, id) = client(
        address.to_string(),
        json!([{"type":"a2s_query","query":"players"}]),
    )
    .await;
    let event = wait_log(&state, id, "player-with-a-long-name-59").await;
    assert!(event.contains("player-with-a-long-name-00"), "{event}");
    let sent = state
        .send_to_client(
            id,
            json!({"type":"a2s_query","query":"info"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let event = wait_log(&state, id, "ctf_2fort").await;
    assert!(event.contains(r#""steam_id":90071992547409920"#), "{event}");
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"a2s_query","query":"ping"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(rejected, ClientSendOutcome::Rejected { .. }),
        "{rejected:?}"
    );
    server.await.unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn netget_pair_with_info_challenge() {
    let server = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let sid = ServerForm {
        protocol: "a2s".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("game server".into()),
        startup_params: Some(json!({"info_challenge": true})),
        event_handlers: Some(vec![json!({"event_pattern":"a2s_query","handler":{"type":"static","actions":[{"type":"a2s_info","name":"Paired","map":"de_inferno","folder":"csgo","game":"Counter-Strike"}]}})]),
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
        json!([{"type":"a2s_query","query":"info"}]),
    )
    .await;
    wait_log(&state, id, "de_inferno").await;
    state.remove_client(id).await;
    server.remove_server(sid).await;
}
