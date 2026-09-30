//! `client::http_fetch::FetchClient` — the request API the HTTP-family clients share — driven
//! natively through both of its backends against a recording HTTP/1.1 peer.
//!
//! The transport backend is what the HTTP-family clients use in the browser build, where
//! reqwest cannot run; the reqwest backend is what they use natively. Each test sends the same
//! request through both and asserts what reached the peer, so the browser path is pinned to what
//! reqwest puts on the wire:
//!
//! - `basic_auth`, `header`, `query` and `json` arrive as reqwest writes them (the same
//!   `Authorization`, the same encoded query, `Content-Type: application/json`, the same body).
//! - `form` arrives as `application/x-www-form-urlencoded`, identically encoded.
//! - A binary body comes back byte for byte through `bytes()`, and through `chunk()` as the
//!   whole body followed by the end; `json()` parses; `status()` and `headers()` read the head.
//! - The transport's body bound refuses an answer one byte over it, and `https://` is refused
//!   with the reason before anything is dialled.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features http,tcp --test client -- http::fetch_client --test-threads=100

use std::time::Duration;

use netget::client::http_fetch::transport::HTTPS_UNSUPPORTED;
use netget::client::http_fetch::FetchClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One request as the peer read it: the request line, the headers (names lowercased) and the
/// body.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A peer that reads one request, answers `status` with `body` as `content_type`, and hands
/// back what it read.
async fn one_shot_peer(
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
) -> (u16, tokio::task::JoinHandle<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(
                n > 0,
                "the client closed before sending a whole request head"
            );
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap().to_string();
        let headers: Vec<(String, String)> = lines
            .filter(|l| !l.is_empty())
            .map(|l| {
                let (n, v) = l.split_once(':').unwrap();
                (n.trim().to_ascii_lowercase(), v.trim().to_string())
            })
            .collect();
        let length: usize = headers
            .iter()
            .find(|(n, _)| n == "content-length")
            .map(|(_, v)| v.parse().unwrap())
            .unwrap_or(0);
        let mut request_body = buf[head_end..].to_vec();
        while request_body.len() < length {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed mid-body");
            request_body.extend_from_slice(&chunk[..n]);
        }
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
             X-Peer: recorded\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        stream.write_all(&response).await.unwrap();
        let _ = stream.shutdown().await;
        Seen {
            request_line,
            headers,
            body: request_body,
        }
    });
    (port, task)
}

/// The two backends, labelled.
fn backends() -> Vec<(&'static str, FetchClient)> {
    vec![
        (
            "reqwest",
            FetchClient::from_reqwest(
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .no_proxy()
                    .build()
                    .unwrap(),
            ),
        ),
        ("transport", FetchClient::transport(Duration::from_secs(10))),
    ]
}

#[tokio::test]
async fn json_query_header_and_basic_auth_reach_the_peer_as_reqwest_writes_them() {
    let mut seen = Vec::new();
    for (name, client) in backends() {
        let (port, peer) = one_shot_peer(
            "200 OK",
            "application/json",
            br#"{"result":42,"error":null}"#.to_vec(),
        )
        .await;
        let response = client
            .post(&format!("http://127.0.0.1:{port}/rpc?fixed=1"))
            .query(&[("q", "two words"), ("n", "5&6")])
            .header("X-Api-Key", "k-123")
            .basic_auth("alice", Some("s3cret:x"))
            .json(&serde_json::json!({"method": "getblockcount", "params": []}))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: send: {e:#}"));
        assert_eq!(response.status().as_u16(), 200, "{name}");
        assert_eq!(response.headers()["x-peer"], "recorded", "{name}");
        let json: serde_json::Value = response.json().await.unwrap();
        assert_eq!(json["result"], 42, "{name}");
        let request = peer.await.unwrap();
        seen.push((name, request));
    }

    let (_, reqwest_seen) = &seen[0];
    let (_, transport_seen) = &seen[1];
    assert_eq!(
        reqwest_seen.request_line,
        "POST /rpc?fixed=1&q=two+words&n=5%266 HTTP/1.1"
    );
    for (name, request) in &seen {
        assert_eq!(request.request_line, reqwest_seen.request_line, "{name}");
        for header in ["authorization", "x-api-key", "content-type"] {
            assert_eq!(
                request.header(header),
                reqwest_seen.header(header),
                "{name}: {header}"
            );
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["method"], "getblockcount", "{name}");
    }
    // `alice:s3cret:x`, base64.
    assert_eq!(
        transport_seen.header("authorization"),
        Some("Basic YWxpY2U6czNjcmV0Ong=")
    );
    assert_eq!(
        transport_seen.header("content-type"),
        Some("application/json")
    );
}

#[tokio::test]
async fn a_form_body_is_encoded_as_reqwest_encodes_it() {
    let mut seen = Vec::new();
    for (name, client) in backends() {
        let (port, peer) = one_shot_peer("201 Created", "text/plain", b"ok".to_vec()).await;
        let response = client
            .post(&format!("http://127.0.0.1:{port}/token"))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", "read write"),
            ])
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: send: {e:#}"));
        assert_eq!(response.status().as_u16(), 201, "{name}");
        assert_eq!(response.text().await.unwrap(), "ok", "{name}");
        seen.push((name, peer.await.unwrap()));
    }
    for (name, request) in &seen {
        assert_eq!(
            request.header("content-type"),
            Some("application/x-www-form-urlencoded"),
            "{name}"
        );
        assert_eq!(
            request.body, b"grant_type=client_credentials&scope=read+write",
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_binary_body_comes_back_byte_for_byte() {
    // Not UTF-8, with a NUL and a bencode-looking prefix: what a tracker or a tarball returns.
    let binary: Vec<u8> = b"d5:peers6:"
        .iter()
        .copied()
        .chain([0x7f, 0x00, 0x00, 0x01, 0x1a, 0xe1, 0xff, 0xfe])
        .chain(*b"e")
        .collect();
    for (name, client) in backends() {
        let (port, peer) =
            one_shot_peer("200 OK", "application/octet-stream", binary.clone()).await;
        let bytes = client
            .get(&format!("http://127.0.0.1:{port}/announce"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: send: {e:#}"))
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), binary.as_slice(), "{name}");
        peer.await.unwrap();

        let (port, peer) =
            one_shot_peer("200 OK", "application/octet-stream", binary.clone()).await;
        let mut response = client
            .get(&format!("http://127.0.0.1:{port}/tarball.tgz"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.content_length(),
            Some(binary.len() as u64),
            "{name}"
        );
        let mut streamed = Vec::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            streamed.extend_from_slice(&chunk);
        }
        assert_eq!(streamed, binary, "{name}");
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn the_transport_body_bound_refuses_one_byte_over() {
    let body = vec![b'x'; 65];
    let (port, peer) = one_shot_peer("200 OK", "text/plain", body.clone()).await;
    let err = FetchClient::transport(Duration::from_secs(10))
        .with_max_body(64)
        .get(&format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .err()
        .expect("65 bytes against a 64-byte bound");
    assert!(err.to_string().contains("64-byte limit"), "{err:#}");
    peer.await.unwrap();

    let (port, peer) = one_shot_peer("200 OK", "text/plain", body[..64].to_vec()).await;
    let text = FetchClient::transport(Duration::from_secs(10))
        .with_max_body(64)
        .get(&format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("exactly at the bound")
        .text()
        .await
        .unwrap();
    assert_eq!(text.len(), 64);
    peer.await.unwrap();
}

#[tokio::test]
async fn the_transport_refuses_https_with_the_reason() {
    let err = FetchClient::transport(Duration::from_secs(5))
        .get("https://registry.npmjs.org/left-pad")
        .send()
        .await
        .err()
        .expect("https on the transport");
    assert!(err.to_string().contains(HTTPS_UNSUPPORTED), "{err:#}");
}
