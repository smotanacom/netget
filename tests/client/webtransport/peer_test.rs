//! NetGet's WebTransport client against an aioquic 1.3.0 server (Python, independent,
//! unchanged): certificate pinning, extended CONNECT with extra headers, a stream it opens and
//! the answer raised back, a unidirectional stream and a datagram echoed, a stream the server
//! opens and the client answers, an injected datagram, the handler closing; a refused path, a
//! wrong pin and a conflicting trust choice. Fails, never skips.
use crate::helpers::webtransport::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

const CLIENT_POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='webtransport_connected': out({'type':'webtransport_open_bi','data':'hello'},{'type':'webtransport_open_uni','data':'uni-hello'},{'type':'webtransport_send_datagram','data':'dg-hello'})
if k=='webtransport_stream' and e['direction']=='bidirectional': out({'type':'webtransport_reply','data':'client-answer to '+e['data']})
if k=='webtransport_stream_reply': out({'type':'webtransport_send_datagram','data':'got '+e['data']})
if k=='webtransport_datagram' and e['data']=='echo:injected': out({'type':'webtransport_close','code':0,'reason':'finished'})
out()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn netget_against_aioquic() {
    let dir = tempfile::tempdir().unwrap();
    let hash = make_cert(dir.path()).await;
    let mut server = EchoServer::start(dir.path()).await;
    let remote = format!("127.0.0.1:{}", server.port);
    let state = state();
    let id = client_in(
        &state,
        remote.clone(),
        script(CLIENT_POLICY),
        json!({"path": "/echo", "certificate_sha256": hash, "headers": {"origin": "https://netget.example"}}),
    )
    .await
    .unwrap();

    let request = server.wait_for(|v| v.get("request").is_some()).await;
    assert_eq!(request["request"][":path"], "/echo", "{request}");
    assert_eq!(
        request["request"]["origin"], "https://netget.example",
        "{request}"
    );
    server.wait_for(|v| v["data"] == "hello").await;
    server.wait_for(|v| v["data"] == "uni-hello").await;
    server.wait_for(|v| v["datagram"] == "dg-hello").await;
    server
        .wait_for(|v| v["answer"] == "client-answer to server-question")
        .await;
    server.wait_for(|v| v["datagram"] == "got echo:hello").await;

    let owner = AccessLogOwner::Client(id.as_u32());
    wait_for(&state, owner, "webtransport_stream_reply", |e| {
        e["data"] == "echo:hello"
    })
    .await;
    wait_for(&state, owner, "webtransport_stream", |e| {
        e["direction"] == "unidirectional" && e["data"] == "echo:uni-hello"
    })
    .await;
    wait_for(&state, owner, "webtransport_datagram", |e| {
        e["data"] == "echo:dg-hello"
    })
    .await;

    let sent = state
        .send_to_client(
            id,
            json!({"type": "webtransport_send_datagram", "data": "injected"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(sent, ClientSendOutcome::Sent { bytes_sent: 8 }),
        "{sent:?}"
    );
    let refused = state
        .send_to_client(
            id,
            json!({"type": "webtransport_reply", "data": "x"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(refused, ClientSendOutcome::Rejected { .. }),
        "{refused:?}"
    );
    server.wait_for(|v| v["datagram"] == "injected").await;
    let closed = server.wait_for(|v| v.get("terminated").is_some()).await;
    assert_eq!(
        closed["terminated"],
        json!({"code": 0, "reason": "finished"}),
        "{closed}"
    );

    let wrong_path = client_in(
        &state,
        remote.clone(),
        script(CLIENT_POLICY),
        json!({"path": "/forbidden", "certificate_sha256": hash}),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{wrong_path:#}").contains("rejected"),
        "{wrong_path:#}"
    );
    let wrong_pin = client_in(
        &state,
        remote.clone(),
        script(CLIENT_POLICY),
        json!({"path": "/echo", "certificate_sha256": "00".repeat(32)}),
    )
    .await
    .unwrap_err();
    assert!(!wrong_pin.to_string().is_empty());
    let both = client_in(
        &state,
        remote,
        script(CLIENT_POLICY),
        json!({"certificate_sha256": hash, "ca_cert_path": dir.path().join("cert.pem").to_str().unwrap()}),
    )
    .await
    .unwrap_err();
    assert!(format!("{both:#}").contains("not both"), "{both:#}");
}
