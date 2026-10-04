//! NetGet's client against NetGet's server: the client pins the server's self-signed
//! certificate by the hash it publishes, both sides' handlers answer, and actions injected on
//! each side reach the other. Fails, never skips.
use crate::helpers::webtransport::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

const CLIENT_POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='webtransport_connected': out({'type':'webtransport_open_bi','data':'ping'},{'type':'webtransport_send_datagram','data':'dg'})
out()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server() {
    let state = state();
    let (sid, addr) = server_in(&state, json!({})).await;
    let hash = certificate_sha256(&state, sid).await;
    assert_eq!(hash.len(), 64);
    let id = client_in(
        &state,
        addr.to_string(),
        script(CLIENT_POLICY),
        json!({"path": "/pair", "certificate_sha256": hash}),
    )
    .await
    .unwrap();
    let client = AccessLogOwner::Client(id.as_u32());
    let server = AccessLogOwner::Server(sid.as_u32());
    wait_for(&state, client, "webtransport_stream_reply", |e| {
        e["data"] == "pong:ping"
    })
    .await;
    wait_for(&state, client, "webtransport_datagram", |e| {
        e["data"] == "dg:dg"
    })
    .await;
    wait_for(&state, server, "webtransport_session_request", |e| {
        e["path"] == "/pair"
    })
    .await;

    let conn = *state
        .get_server(sid)
        .await
        .unwrap()
        .connections
        .keys()
        .next()
        .unwrap();
    let sent = state
        .send_to_peer(
            sid,
            conn.as_u32(),
            json!({"type": "webtransport_open_uni", "data": "from the server"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    wait_for(&state, client, "webtransport_stream", |e| {
        e["direction"] == "unidirectional" && e["data"] == "from the server"
    })
    .await;

    // The client closes; the server's connection follows.
    state
        .send_to_client(
            id,
            json!({"type": "webtransport_close", "code": 0, "reason": "bye"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let open = state
                .get_server(sid)
                .await
                .unwrap()
                .connections
                .values()
                .any(|c| matches!(c.status, netget::state::server::ConnectionStatus::Active));
            if !open {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the server kept the session after the client closed");

    // A client that pins another certificate never opens a session.
    let wrong = client_in(
        &state,
        addr.to_string(),
        script(CLIENT_POLICY),
        json!({"path": "/pair", "certificate_sha256": "11".repeat(32)}),
    )
    .await;
    assert!(wrong.is_err());
    state.remove_server(sid).await;
}
