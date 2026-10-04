//! NetGet's FastCGI client against flup 1.0.3 — an independent WSGI FastCGI server, unchanged:
//! GET with params and a header, a POST body, a 200 000-byte answer, FCGI_STDERR, 404, a
//! redirect, GET_VALUES (answered, though flup's Python 3 lookup leaves it empty), and a slow request the client aborts after its timeout (flup records
//! the abort and still ends the request). Fails, never skips.
use crate::helpers::fastcgi::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

#[tokio::test(flavor = "multi_thread")]
async fn client_sends_requests_to_flup() {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("flup did not start")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let port = first["port"].as_u64().unwrap();
    let state = state();
    let cid = client_in(
        &state,
        format!("127.0.0.1:{port}"),
        json!({"document_root": "/srv/app", "request_timeout_secs": 1}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    for a in [
        json!({"type":"fastcgi_request","path":"/echo","query":"x=1","headers":{"X-Test":"flup"},"params":{"NETGET_CUSTOM":"yes"}}),
        json!({"type":"fastcgi_request","method":"POST","path":"/echo","headers":{"Content-Type":"text/plain"},"body":"b".repeat(100_000)}),
        json!({"type":"fastcgi_request","path":"/big"}),
        json!({"type":"fastcgi_request","path":"/stderr"}),
        json!({"type":"fastcgi_request","path":"/missing"}),
        json!({"type":"fastcgi_request","path":"/redirect"}),
        json!({"type":"fastcgi_get_values"}),
        json!({"type":"fastcgi_request","path":"/slow"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(20))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "fastcgi_response", 7).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        (r[0]["status"].as_u64(), r[0]["headers"]["x-flup"].as_str()),
        (Some(200), Some("yes"))
    );
    let echo: Value = serde_json::from_str(r[0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(
        echo,
        json!({"method": "GET", "uri": "/echo?x=1", "query": "x=1", "script_filename": "/srv/app/echo", "content_type": null, "x_test": "flup", "custom": "yes", "body_length": 0, "body_head": ""})
    );
    let echo: Value = serde_json::from_str(r[1]["body"].as_str().unwrap()).unwrap();
    assert_eq!(
        (
            echo["method"].as_str(),
            echo["body_length"].as_u64(),
            echo["content_type"].as_str()
        ),
        (Some("POST"), Some(100_000), Some("text/plain"))
    );
    assert_eq!(r[2]["body"].as_str().unwrap().len(), 200_000);
    assert_eq!(
        (r[3]["body"].as_str(), r[3]["stderr"].as_str()),
        (Some("logged"), Some("flup stderr line\n"))
    );
    assert_eq!(
        (r[4]["status"].as_u64(), r[4]["body"].as_str()),
        (Some(404), Some("no such page"))
    );
    assert_eq!(
        (
            r[5]["status"].as_u64(),
            r[5]["headers"]["location"].as_str()
        ),
        (Some(302), Some("/elsewhere"))
    );
    assert_eq!(r[6]["aborted"], true, "{}", r[6]);
    assert_eq!(
        (r[6]["status"].as_u64(), r[6]["body"].as_str()),
        (Some(200), Some("slow done"))
    );
    assert_eq!(r[6]["protocol_status"], "request_complete");
    // flup answers FCGI_GET_VALUES_RESULT (not UNKNOWN_TYPE), so the management record was
    // understood; under Python 3 it compares each decoded name (bytes) with its str-keyed
    // capability table, so the result is always empty. The pair test covers populated values.
    let values = logs(&state, owner, "fastcgi_values", 1).await;
    assert_eq!(values[0].request, json!({"values": {}}));
    state.remove_client(cid).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}
