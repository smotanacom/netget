//! NetGet's ICAP client against c-icap 0.6.5's echo service — an independent server,
//! unchanged: OPTIONS, a RESPMOD echoed back whole, a REQMOD answered 204, and a previewed body.
//! Fails, never skips, when the peer is absent.
use crate::helpers::icap::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_speaks_options_respmod_reqmod_and_preview_to_c_icap() {
    let (mut child, port, _dir) = c_icap_server().await;
    let state = state();
    let cid = client_in(&state, format!("127.0.0.1:{port}")).await;
    let body = "x".repeat(3000);
    for a in [
        json!({"type":"icap_request","method":"OPTIONS","service":"echo"}),
        json!({"type":"icap_request","method":"RESPMOD","service":"echo","allow_204":false,"http_request":{"method":"GET","uri":"http://example.com/"},"http_response":{"status":200,"reason":"OK","headers":[["Content-Type","text/plain"],["Content-Length","11"]]},"body_text":"hello world"}),
        json!({"type":"icap_request","method":"REQMOD","service":"echo","http_request":{"method":"POST","uri":"http://example.com/upload","headers":[["Content-Length","4"]]},"body_text":"data"}),
        json!({"type":"icap_request","method":"RESPMOD","service":"echo","allow_204":false,"preview":1024,"http_response":{"status":200,"reason":"OK","headers":[["Content-Length","3000"]]},"body_text":body}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(15))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "icap_response",
        4,
    )
    .await;
    let r = |i: usize| &rows[i].request;
    assert_eq!(r(0)["status"], 200);
    assert!(
        r(0)["icap_headers"].to_string().contains("RESPMOD, REQMOD"),
        "{}",
        r(0)
    );
    assert_eq!(
        (r(1)["status"].as_u64(), r(1)["body_text"].as_str()),
        (Some(200), Some("hello world"))
    );
    assert_eq!(r(1)["http_response"]["status"], 200);
    // The echo service decides between 204 and echoing the request back; either is a valid
    // answer to Allow: 204, and an echo must carry the request unchanged.
    match r(2)["status"].as_u64() {
        Some(204) => {}
        Some(200) => assert_eq!(
            (
                r(2)["http_request"]["method"].as_str(),
                r(2)["body_text"].as_str()
            ),
            (Some("POST"), Some("data"))
        ),
        other => panic!("unexpected REQMOD status {other:?}"),
    }
    assert_eq!(r(3)["status"], 200);
    assert_eq!(
        r(3)["body_bytes"],
        3000,
        "the previewed body was continued and echoed whole"
    );
    assert_eq!(r(3)["continued"], true);
    state.remove_client(cid).await;
    let _ = child.kill().await;
}
