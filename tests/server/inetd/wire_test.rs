//! The six inetd services over raw TCP and UDP: what each sends for a handler's answer, the
//! transport switch, Discard's byte limit and refusal, Chargen's pattern and limit, Time's
//! RFC 868 arithmetic, and silence (never fabricated output) when the handler fails.
pub use crate::helpers::inetd::{answer, start};
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};

/// Read a TCP reply to EOF.
pub async fn tcp_reply(addr: SocketAddr, send: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    if !send.is_empty() {
        // A server that has already closed (a failed handler) may reset the write; that is
        // the behaviour under test, not an error in the test.
        let _ = s.write_all(send).await;
        let _ = s.shutdown().await;
    }
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
        .await
        .expect("server closes");
    out
}

/// One UDP request; `None` when no reply arrives in 1.5 s.
pub async fn udp_reply(addr: SocketAddr, send: &[u8]) -> Option<Vec<u8>> {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    s.send_to(send, addr).await.unwrap();
    let mut buf = vec![0u8; 9000];
    match tokio::time::timeout(Duration::from_millis(1500), s.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

const UPPERCASE: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'echo_reply','data':e['data'].upper(),'encoding':e['encoding']}]}))";

#[tokio::test]
async fn echo_verbatim_binary_and_handler_rewritten() {
    let (state, id, addr) = start(
        "echo",
        answer("echo_request", json!({"type":"echo_reply"})),
        json!({}),
    )
    .await;
    assert_eq!(tcp_reply(addr, b"hello echo").await, b"hello echo");
    assert_eq!(tcp_reply(addr, &[0, 1, 2, 0xff]).await, [0, 1, 2, 0xff]);
    assert_eq!(
        udp_reply(addr, b"datagram").await.as_deref(),
        Some(&b"datagram"[..])
    );
    state.remove_server(id).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "stop releases the port"
    );
    let rewrite = vec![
        json!({"event_pattern":"echo_request","handler":{"type":"script","language":"python","code":UPPERCASE}}),
    ];
    let (state, id, addr) = start("echo", rewrite, json!({"transport":"udp"})).await;
    assert_eq!(
        udp_reply(addr, b"shout").await.as_deref(),
        Some(&b"SHOUT"[..])
    );
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "udp only binds no TCP listener"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn discard_reads_silently_and_honours_the_limit_and_refusal() {
    let (state, id, addr) = start(
        "discard",
        answer(
            "discard_request",
            json!({"type":"discard_reply","max_bytes":10}),
        ),
        json!({"transport":"tcp"}),
    )
    .await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&[b'x'; 25]).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
        .await
        .expect("closed after the limit")
        .unwrap();
    assert!(out.is_empty(), "discard never answers");
    state.remove_server(id).await;
    let (state, id, addr) = start(
        "discard",
        answer(
            "discard_request",
            json!({"type":"discard_refuse","reason":"closed for maintenance"}),
        ),
        json!({}),
    )
    .await;
    assert!(tcp_reply(addr, b"").await.is_empty());
    assert_eq!(
        udp_reply(addr, b"dropped").await,
        None,
        "udp discard answers nothing"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn daytime_qotd_and_time_answer_over_both_transports() {
    let (state, id, addr) = start(
        "daytime",
        answer(
            "daytime_request",
            json!({"type":"daytime_reply","text":"Saturday, January 1, 2000 12:00:00-UTC"}),
        ),
        json!({}),
    )
    .await;
    assert_eq!(
        tcp_reply(addr, b"").await,
        b"Saturday, January 1, 2000 12:00:00-UTC\r\n"
    );
    assert_eq!(
        udp_reply(addr, b"\n").await.as_deref(),
        Some(&b"Saturday, January 1, 2000 12:00:00-UTC\r\n"[..])
    );
    state.remove_server(id).await;
    let (state, id, addr) = start(
        "daytime",
        answer("daytime_request", json!({"type":"daytime_reply"})),
        json!({}),
    )
    .await;
    let clock = String::from_utf8(tcp_reply(addr, b"").await).unwrap();
    let year = chrono::Utc::now().format("%Y").to_string();
    assert!(
        clock.contains(&year) && clock.ends_with("-UTC\r\n"),
        "server clock: {clock:?}"
    );
    state.remove_server(id).await;

    let (state, id, addr) = start(
        "qotd",
        answer(
            "qotd_request",
            json!({"type":"qotd_reply","quote":"line one\nline two"}),
        ),
        json!({}),
    )
    .await;
    assert_eq!(tcp_reply(addr, b"").await, b"line one\r\nline two\r\n");
    assert_eq!(
        udp_reply(addr, b"\n").await.as_deref(),
        Some(&b"line one\r\nline two\r\n"[..])
    );
    state.remove_server(id).await;

    // 2000-01-01T12:00:00Z is 946728000 Unix seconds, 3155716800 seconds since 1900.
    let (state, id, addr) = start(
        "time",
        answer(
            "time_request",
            json!({"type":"time_reply","iso8601":"2000-01-01T12:00:00Z"}),
        ),
        json!({}),
    )
    .await;
    assert_eq!(tcp_reply(addr, b"").await, 3_155_716_800u32.to_be_bytes());
    assert_eq!(
        udp_reply(addr, b"\n").await,
        Some(3_155_716_800u32.to_be_bytes().to_vec())
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn chargen_streams_the_rfc_pattern_until_its_limit() {
    let (state, id, addr) = start(
        "chargen",
        answer(
            "chargen_request",
            json!({"type":"chargen_reply","max_bytes":200}),
        ),
        json!({}),
    )
    .await;
    let stream = String::from_utf8(tcp_reply(addr, b"").await).unwrap();
    assert_eq!(stream.len(), 200);
    let charset: String = (0x20u8..=0x7e).map(char::from).collect();
    assert_eq!(&stream[..74], format!("{}\r\n", &charset[..72]));
    assert_eq!(&stream[74..148], format!("{}\r\n", &charset[1..73]));
    let datagram = udp_reply(addr, b"\n").await.unwrap();
    assert_eq!(datagram.len(), 74, "one 72-character line per datagram");
    state.remove_server(id).await;
    let (state, id, addr) = start(
        "chargen",
        answer(
            "chargen_request",
            json!({"type":"chargen_reply","charset":"abc","line_length":4,"max_bytes":18}),
        ),
        json!({}),
    )
    .await;
    assert_eq!(tcp_reply(addr, b"").await, b"abca\r\nbcab\r\ncabc\r\n");
    state.remove_server(id).await;
}

#[tokio::test]
async fn chargen_keeps_streaming_after_the_client_half_closes() {
    // netcat -N half-closes at once. RFC 864 throws client input away, so the end of that
    // input must not end the stream. Two megabytes cannot be written in one poll, which is
    // what makes the end of input race the generator rather than lose to it.
    let (state, id, addr) = start(
        "chargen",
        answer(
            "chargen_request",
            json!({"type":"chargen_reply","max_bytes":2_000_000}),
        ),
        json!({"transport":"tcp"}),
    )
    .await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.shutdown().await.unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut received))
        .await
        .expect("stream ends at its limit")
        .unwrap();
    assert_eq!(
        received.len(),
        2_000_000,
        "half-closing the request side ended the stream"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn a_deliberate_refusal_sends_nothing() {
    for (protocol, event, refuse) in [
        ("echo", "echo_request", "echo_refuse"),
        ("daytime", "daytime_request", "daytime_refuse"),
        ("time", "time_request", "time_refuse"),
    ] {
        let (state, id, addr) =
            start(protocol, answer(event, json!({"type": refuse})), json!({})).await;
        assert!(
            tcp_reply(addr, b"x").await.is_empty(),
            "{protocol} answered after refusing"
        );
        assert_eq!(
            udp_reply(addr, b"x").await,
            None,
            "{protocol} answered a refused datagram"
        );
        state.remove_server(id).await;
    }
}

#[tokio::test]
async fn a_failed_handler_sends_nothing() {
    for protocol in ["echo", "daytime", "qotd", "chargen", "time"] {
        let (state, id, addr) = start(protocol, vec![], json!({})).await;
        assert!(
            tcp_reply(addr, b"x").await.is_empty(),
            "{protocol} sent output it was not given"
        );
        assert_eq!(
            udp_reply(addr, b"x").await,
            None,
            "{protocol} answered a datagram it was not given"
        );
        state.remove_server(id).await;
    }
}

#[tokio::test]
async fn oversized_datagrams_are_dropped() {
    let (state, id, addr) = start(
        "echo",
        answer("echo_request", json!({"type":"echo_reply"})),
        json!({"transport":"udp"}),
    )
    .await;
    assert_eq!(udp_reply(addr, &[b'a'; 8193]).await, None);
    assert_eq!(
        udp_reply(addr, &[b'a'; 8192]).await.map(|r| r.len()),
        Some(8192)
    );
    state.remove_server(id).await;
}
