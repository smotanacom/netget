//! Headless Chrome's WebTransport API against NetGet's self-signed server, trusted the way a
//! page trusts a development server: serverCertificateHashes with the SHA-256 the server
//! publishes. A stream, a unidirectional stream answered on one the server opens, a datagram, a
//! refused path. Needs NETGET_CHROME and Node 22+ on PATH; fails, never skips.
use crate::helpers::webtransport::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn chrome_against_netget() {
    std::env::var("NETGET_CHROME").expect(
        "NETGET_CHROME must name a Chrome or Chromium binary for the browser interop check; this evidence never skips",
    );
    let state = state();
    let (sid, addr) = server_in(&state, json!({})).await;
    let hash = certificate_sha256(&state, sid).await;
    let run = tokio::process::Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/server/webtransport/browser.mjs"
        ))
        .args([addr.port().to_string(), hash])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .expect("the browser check did not finish")
        .expect("node (22 or newer) must be on PATH for the browser interop check");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let r: Value =
        serde_json::from_str(stdout.lines().last().unwrap_or_default()).unwrap_or_else(|e| {
            panic!(
                "browser.mjs printed no result ({e}):\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["biText"], "pong:browser-bi", "{r}");
    assert_eq!(r["uniText"], "uni:browser-uni", "{r}");
    assert_eq!(r["datagram"], "dg:browser-datagram", "{r}");
    assert_eq!(r["refused"], "WebTransportError", "{r}");

    let owner = AccessLogOwner::Server(sid.as_u32());
    let request = wait_for(&state, owner, "webtransport_session_request", |e| {
        e["path"] == "/echo"
    })
    .await;
    assert!(
        request["origin"]
            .as_str()
            .is_some_and(|o| o.starts_with("http://127.0.0.1:")),
        "{request}"
    );
    // Chrome negotiates draft-02, the version wtransport speaks, and says so in the request.
    assert_eq!(
        request["headers"]["sec-webtransport-http3-draft02"], "1",
        "{request}"
    );
    assert!(
        r["userAgent"]
            .as_str()
            .is_some_and(|u| u.contains("Chrome")),
        "{r}"
    );
    state.remove_server(sid).await;
}
