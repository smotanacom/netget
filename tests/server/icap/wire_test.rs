//! RFC 3507 framing without peers, server refusals, and the NetGet pair.
use crate::helpers::icap::*;
use netget::server::icap::wire;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

#[test]
fn encapsulated_offsets_must_be_ordered_and_end_in_a_body() {
    assert_eq!(
        wire::parse_encapsulated("req-hdr=0, res-hdr=45, res-body=100")
            .unwrap()
            .len(),
        3
    );
    assert!(wire::parse_encapsulated("null-body=0").is_ok());
    for bad in [
        "res-hdr=0",
        "res-body=0, res-hdr=10",
        "req-hdr=10, res-body=5",
        "junk=0",
        "req-hdr=x, null-body=1",
        "",
        "res-hdr=0, res-body=99999999",
    ] {
        assert!(wire::parse_encapsulated(bad).is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn chunks_are_bounded_before_allocation_and_preview_ends_are_recognized() {
    async fn chunks(raw: &[u8]) -> anyhow::Result<wire::Chunks> {
        wire::read_chunks(&mut BufReader::new(raw), 0).await
    }
    let c = chunks(b"5\r\nhello\r\n0; ieof\r\n\r\n").await.unwrap();
    assert_eq!((c.data.as_slice(), c.ieof), (&b"hello"[..], true));
    assert!(!chunks(b"0\r\n\r\n").await.unwrap().ieof);
    for bad in [
        &b"5\r\nhel"[..],
        b"zz\r\n",
        b"5\r\nhelloXX0\r\n\r\n",
        b"200000\r\n",
        b"5; x\r\nhello\r\n0\r\n\r\n",
        b"0\r\nX",
    ] {
        assert!(chunks(bad).await.is_err(), "{bad:?}");
    }
    assert_eq!(wire::chunked(b"abc"), b"3\r\nabc\r\n0\r\n\r\n");
    assert!(wire::render_head(&wire::HttpHead {
        start: ["GET".into(), "/".into(), "HTTP/1.1".into()],
        headers: vec![("X".into(), "a\r\nInjected: 1".into())]
    })
    .is_err());
}

async fn raw(addr: std::net::SocketAddr, request: &[u8]) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_refuses_with_icap_statuses_and_never_passes_on_failure() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        vec![],
        json!({"services":[{"name":"scan","methods":["RESPMOD"]}]}),
    )
    .await;
    assert!(
        raw(addr, b"OPTIONS icap://x/nope ICAP/1.0\r\nHost: x\r\n\r\n")
            .await
            .starts_with("ICAP/1.0 404")
    );
    assert!(
        raw(addr, b"OPTIONS icap://x/scan ICAP/2.0\r\nHost: x\r\n\r\n")
            .await
            .starts_with("ICAP/1.0 505")
    );
    assert!(raw(
        addr,
        b"REQMOD icap://x/scan ICAP/1.0\r\nHost: x\r\nEncapsulated: null-body=0\r\n\r\n"
    )
    .await
    .starts_with("ICAP/1.0 405"));
    assert!(raw(addr, b"GARBAGE\r\n\r\n")
        .await
        .starts_with("ICAP/1.0 400"));
    // No handler and no model: a server error, never a 204 that would let content through.
    let body = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n";
    let req = format!("RESPMOD icap://x/scan ICAP/1.0\r\nHost: x\r\nAllow: 204\r\nEncapsulated: res-hdr=0, res-body={}\r\n\r\n", body.len());
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(b"2\r\nhi\r\n0\r\n\r\n");
    assert!(raw(addr, &bytes).await.starts_with("ICAP/1.0 500"));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server_agree_on_every_verdict_and_preview() {
    let state = state();
    let (sid, addr) = server_in(&state, filter_policy(), json!({})).await;
    let cid = client_in(&state, addr.to_string()).await;
    let res = |body: &str| json!({"type":"icap_request","method":"RESPMOD","service":"netget","allow_204":false,"http_response":{"status":200,"reason":"OK"},"body_text":body});
    for a in [
        json!({"type":"icap_request","method":"OPTIONS","service":"netget"}),
        res("clean"),
        res("EICAR inside"),
        json!({"type":"icap_request","method":"REQMOD","service":"netget","http_request":{"method":"POST","uri":"http://h/u"},"body_text":"secret"}),
        json!({"type":"icap_request","method":"RESPMOD","service":"netget","allow_204":true,"preview":10,"http_response":{"status":200,"reason":"OK"},"body_text":"0123456789abcdefghij"}),
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
        5,
    )
    .await;
    let r = |i: usize| &rows[i].request;
    assert!(r(0)["icap_headers"].to_string().contains("REQMOD, RESPMOD"));
    assert_eq!(
        (r(1)["status"].as_u64(), r(1)["body_text"].as_str()),
        (Some(200), Some("clean")),
        "no 204 allowed: the original is echoed"
    );
    assert_eq!(
        (
            r(2)["http_response"]["status"].as_u64(),
            r(2)["body_text"].as_str()
        ),
        (Some(403), Some("Blocked by NetGet"))
    );
    assert_eq!(
        (
            r(3)["http_request"]["method"].as_str(),
            r(3)["body_text"].as_str()
        ),
        (Some("POST"), Some("[redacted]"))
    );
    assert_eq!(
        (r(4)["status"].as_u64(), r(4)["continued"].as_bool()),
        (Some(204), Some(true))
    );
    let seen = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "icap_request",
        4,
    )
    .await;
    assert!(
        seen.iter()
            .any(|s| s.request["body_text"] == "0123456789abcdefghij"),
        "the server decided on the whole previewed body"
    );
    for bad in [
        json!({"type":"icap_request","method":"PUT","service":"netget"}),
        json!({"type":"icap_request","method":"REQMOD","service":"netget"}),
        json!({"type":"icap_request","method":"OPTIONS","service":"a b"}),
        json!({"type":"icap_request","method":"RESPMOD","service":"netget","http_response":{"status":200,"headers":[["X","a\nb"]]}}),
    ] {
        assert!(
            matches!(
                state
                    .send_to_client(cid, bad.clone(), Duration::from_secs(5))
                    .await
                    .unwrap(),
                ClientSendOutcome::Rejected { .. }
            ),
            "{bad}"
        );
    }
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
