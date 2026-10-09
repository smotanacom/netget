//! aioquic 1.3.0 (Python, independent, unchanged) against NetGet's WebTransport server:
//! extended CONNECT admission, bidirectional and unidirectional streams, datagrams, a stream the
//! server opens and the client answers, hex for binary data, a stream past the 1 MiB bound, an
//! unanswered stream, the handler closing the session; refusals by status, a silent handler,
//! a failed handler and a plain HTTP/3 request. Fails, never skips.
use crate::helpers::webtransport::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn aioquic_against_netget() {
    let dir = tempfile::tempdir().unwrap();
    make_cert(dir.path()).await;
    let cafile = dir.path().join("cert.pem");
    let params = json!({
        "cert_path": cafile.to_str().unwrap(),
        "key_path": dir.path().join("key.pem").to_str().unwrap(),
    });
    let state = state();
    let (sid, addr) = server_in(&state, params).await;
    let port = addr.port().to_string();
    let ca = cafile.to_str().unwrap();

    let r = peer(&["client", &port, ca, "full"]).await;
    assert_eq!(r["status"], "200", "{r}");
    assert_eq!(r["response_headers"]["x-netget"], "yes", "{r}");
    assert_eq!(r["bi"], "pong:ping", "{r}");
    assert_eq!(r["uni"], "uni:uni-hello", "{r}");
    assert_eq!(r["datagram"], "dg:dg", "{r}");
    assert_eq!(r["question"], "question?", "{r}");
    assert_eq!(r["after_answer"], "got answer!", "{r}");
    assert_eq!(r["binary"], "009fff", "{r}");
    assert!(
        matches!(
            r["oversized"].as_str(),
            Some("StreamReset" | "StopSendingReceived")
        ),
        "{r}"
    );
    assert_eq!(r["silent"], "", "{r}");
    assert_eq!(r["closed"], json!({"code": 7, "reason": "done"}), "{r}");

    let owner = AccessLogOwner::Server(sid.as_u32());
    let request = wait_for(&state, owner, "webtransport_session_request", |e| {
        e["path"] == "/echo"
    })
    .await;
    assert_eq!(request["origin"], "https://peer.example", "{request}");
    assert_eq!(request["headers"]["sec-webtransport-http3-draft02"], "1");
    let binary = wait_for(&state, owner, "webtransport_stream", |e| {
        e["encoding"] == "hex"
    })
    .await;
    assert_eq!(binary["data"], "009fff");
    let reply = wait_for(&state, owner, "webtransport_stream_reply", |_| true).await;
    assert_eq!(
        (reply["request"].clone(), reply["data"].clone()),
        (json!("question?"), json!("answer!"))
    );
    // The oversized stream never reached the handler.
    assert!(state
        .list_access_logs_for(Some(owner), None)
        .await
        .iter()
        .all(|e| e.request["data"].as_str().is_none_or(|d| d.len() < 1024)));

    for (path, status) in [("/forbidden", "403"), ("/busy", "429"), ("/nowhere", "404")] {
        let r = peer(&["client", &port, ca, &format!("path:{path}")]).await;
        assert_eq!(r["status"], status, "{path}: {r}");
    }
    // A plain HTTP/3 request is not a session and never reaches the handler.
    let r = peer(&["client", &port, ca, "get"]).await;
    assert!(r.get("terminated").is_some() || r["status"] != "200", "{r}");
    assert!(state
        .list_access_logs_for(Some(owner), None)
        .await
        .iter()
        .all(|e| e.request["path"] != "/"));
    state.remove_server(sid).await;

    // No handler and no model: the session is refused (429), never admitted.
    let (sid, addr) = server_with(
        &state,
        None,
        json!({
            "cert_path": ca,
            "key_path": dir.path().join("key.pem").to_str().unwrap(),
        }),
    )
    .await;
    let r = peer(&["client", &addr.port().to_string(), ca, "path:/echo"]).await;
    assert_eq!(r["status"], "429", "{r}");
    state.remove_server(sid).await;
}
