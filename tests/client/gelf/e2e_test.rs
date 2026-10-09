use netget::cli::management::ClientForm;
use netget::state::{
    app_state::AppState, client_handles::ClientSendOutcome, ClientId, ClientStatus,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpListener, sync::mpsc};

pub(super) async fn start(remote: String, handler: Value, params: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "gelf".into(),
        remote_addr: Some(remote),
        instruction: Some("test".into()),
        startup_params: Some(params),
        event_handlers: Some(vec![
            json!({"event_pattern":"gelf_connected","handler":handler}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
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
    .expect("client handle");
    (state, id)
}
pub(super) async fn send(state: &AppState, id: ClientId, message: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"send_gelf_message","message":message}),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}

pub(super) fn message() -> Value {
    json!({"host":"emitter","short_message":"温度","timestamp":1700000000.25,"level":6,"additional_fields":{"service":"api","n":42}})
}
#[tokio::test]
async fn tcp_exact_structured_wire_atomic_rejection_disconnect_and_stop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"static","actions":[]}),
        json!({"transport":"tcp"}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    for invalid in [
        json!({"host":"x"}),
        json!({"host":"x","short_message":"x","level":8}),
        json!({"host":"x","short_message":"x","additional_fields":{"bad":[]}}),
        json!({"host":"x","short_message":"x","full_message":"x".repeat(256*1024)}),
    ] {
        assert!(matches!(
            send(&state, id, invalid).await,
            ClientSendOutcome::Rejected { .. }
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(75), peer.read(&mut [0; 1]))
            .await
            .is_err()
    );
    assert!(matches!(
        send(&state, id, message()).await,
        ClientSendOutcome::Sent { .. }
    ));
    let mut expected =
        netget::server::gelf::codec::encode_json(&serde_json::from_value(message()).unwrap())
            .unwrap();
    expected.push(0);
    let mut wire = vec![0; expected.len()];
    peer.read_exact(&mut wire).await.unwrap();
    assert_eq!(wire, expected);
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn udp_send_injection_during_manual_connect_and_disconnect_releases_local_socket() {
    let peer = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    let (state, id) = start(
        peer.local_addr().unwrap().to_string(),
        json!({"type":"manual","timeout_secs":300}),
        json!({"compression":"none"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        send(&state, id, message()).await,
        ClientSendOutcome::Sent { .. }
    ));
    let mut bytes = [0; 8192];
    let (n, local) = peer.recv_from(&mut bytes).await.unwrap();
    assert_eq!(
        netget::server::gelf::codec::parse_json(&bytes[..n])
            .unwrap()
            .short_message,
        "温度"
    );
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(id).await;
    assert!(state.list_intercepts().await.is_empty());
    // Disconnect acknowledges the command before its owner drops the writer, and
    // removing the client requests task cancellation without awaiting that drop.
    // Observe actual socket release; a retained socket must still fail this bound.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tokio::net::UdpSocket::bind(local).await {
                Ok(socket) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("cannot rebind GELF UDP address {local}: {error}"),
            }
        }
    })
    .await
    .expect("GELF UDP socket remained bound after disconnect and removal");
}
#[tokio::test]
async fn tcp_manual_connect_stays_injectable_stop_and_remote_eof_close_handle() {
    for stop in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, id) = start(
            listener.local_addr().unwrap().to_string(),
            json!({"type":"manual","timeout_secs":300}),
            json!({"transport":"tcp"}),
        )
        .await;
        let (mut peer, _) = listener.accept().await.unwrap();
        assert!(matches!(
            send(&state, id, message()).await,
            ClientSendOutcome::Sent { .. }
        ));
        let mut bytes = [0; 1];
        peer.read_exact(&mut bytes).await.unwrap();
        if stop {
            state.remove_client(id).await;
            let mut remaining = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), peer.read_to_end(&mut remaining))
                .await
                .unwrap()
                .unwrap();
        } else {
            drop(peer);
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.has_client_handle(id).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                state.get_client(id).await.unwrap().status,
                ClientStatus::Disconnected
            );
            state.remove_client(id).await;
        }
    }
}

#[tokio::test]
async fn script_connected_handler_updates_standard_client_memory_and_sends() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let code="import json,sys\nx=json.load(sys.stdin)\nassert x['client']['memory']==''\nprint(json.dumps({'actions':[{'type':'set_memory','value':'observed'},{'type':'send_gelf_message','message':{'host':'script','short_message':'sent'}}]}))";
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"script","language":"python","code":code}),
        json!({"transport":"tcp"}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut byte = [0; 1];
            peer.read_exact(&mut byte).await.unwrap();
            if byte[0] == 0 {
                break;
            }
            bytes.push(byte[0]);
        }
    })
    .await
    .unwrap();
    assert_eq!(
        netget::server::gelf::codec::parse_json(&bytes)
            .unwrap()
            .host,
        "script"
    );
    assert_eq!(state.get_client(id).await.unwrap().memory, "observed");
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
}
