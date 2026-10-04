//! nginx — an independent FastCGI client, unchanged — in front of NetGet's responder: GET with
//! a query and a header, a 100 KB POST (several STDIN records), a 200 000-byte answer (several
//! STDOUT records), a binary answer, a status with a STDERR line nginx writes to its error log,
//! all over one kept-alive upstream connection; then a handler-less responder failing closed.
//! Fails, never skips: a missing nginx names the package to install.
use crate::helpers::fastcgi::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn nginx_forwards_requests_to_netget_over_fastcgi() {
    let state = state();
    let (sid, addr) = server_in(&state, echo_policy(), json!({})).await;
    let nginx = nginx_in_front_of(addr).await.unwrap();
    let base = format!("http://{}", nginx.addr());
    let http = reqwest::Client::new();

    let r = http
        .get(format!("{base}/hello?x=1&y=two"))
        .header("X-Test", "abc")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["x-from"], "netget");
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v,
        json!({"method": "GET", "uri": "/hello?x=1&y=two", "query": "x=1&y=two", "x_test": "abc", "keep": true})
    );

    let body = "z".repeat(100_000);
    let r = http
        .post(format!("{base}/submit"))
        .header("Content-Type", "text/plain")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"got": 100_000, "encoding": "utf8", "type": "text/plain"})
    );

    let r = http.get(format!("{base}/big")).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "y".repeat(200_000));
    let r = http.get(format!("{base}/binary")).send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &[0x00, 0xff]);

    let r = http.get(format!("{base}/teapot")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 418);
    assert_eq!(r.text().await.unwrap(), "short and stout");
    nginx
        .wait_for_log(
            "FastCGI sent in stderr: \"teapot brewed\"",
            Duration::from_secs(10),
        )
        .await
        .unwrap();

    let rows = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "fastcgi_request",
        5,
    )
    .await;
    let conns: std::collections::BTreeSet<_> = rows.iter().map(|r| r.connection_id).collect();
    assert_eq!(
        conns.len(),
        1,
        "nginx reused one kept-alive connection: {conns:?}"
    );
    assert_eq!(rows[1].request["method"], "POST");
    assert_eq!(rows[0].request["headers"]["x-test"], "abc");
    drop(nginx);
    state.remove_server(sid).await;

    let state = crate::helpers::fastcgi::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let nginx = nginx_in_front_of(addr).await.unwrap();
    let r = reqwest::get(format!("http://{}/anything", nginx.addr()))
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        500,
        "no handler and no model: a 500, never invented content"
    );
    let text = r.text().await.unwrap();
    assert!(!text.is_empty() && !text.contains("127.0.0.1:1"), "{text}");
    drop(nginx);
    state.remove_server(sid).await;
}
