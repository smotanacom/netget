//! FastCGI records without peers: the codec, management records, roles, multiplexing, abort,
//! bounds and connection lifecycle on the wire, and the NetGet client/server pair.
use crate::helpers::fastcgi::*;
use netget::server::fastcgi::record::{self, Record};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[test]
fn codec_round_trips_and_parses_cgi() {
    let long = "v".repeat(300);
    let pairs = record::encode_pairs([("A", "1"), ("LONG", long.as_str()), ("EMPTY", "")]);
    assert_eq!(pairs[0..2], [1, 1]);
    assert_eq!(
        record::decode_pairs(&pairs).unwrap(),
        vec![
            ("A".into(), "1".into()),
            ("LONG".into(), long.clone()),
            ("EMPTY".into(), String::new())
        ]
    );
    assert!(
        record::decode_pairs(&[5, 0, b'A']).is_err(),
        "pair longer than its stream"
    );
    assert!(
        record::decode_pairs(&[0x80, 0, 0]).is_err(),
        "truncated 4-byte length"
    );
    assert!(
        record::decode_pairs(&[0xff, 0xff, 0xff, 0xff, 0]).is_err(),
        "huge declared length"
    );
    let one = record::encode(record::STDOUT, 7, b"abc");
    assert_eq!(one.len(), 8 + 8, "padded to a multiple of 8");
    assert_eq!(&one[..8], &[1, 6, 0, 7, 0, 3, 5, 0]);
    let stream = record::encode_stream(record::STDOUT, 1, &vec![b'x'; 140_000]);
    let lens: Vec<u16> = split(&stream)
        .iter()
        .map(|r| r.content.len() as u16)
        .collect();
    assert_eq!(lens, [65_528, 65_528, 8_944, 0]);
    let (status, headers, body) =
        record::parse_cgi_response(b"Content-Type: text/plain\nX-A: b\n\nhello").unwrap();
    assert_eq!(
        (status, headers["x-a"].as_str(), body.as_slice()),
        (200, Some("b"), &b"hello"[..])
    );
    assert_eq!(
        record::parse_cgi_response(b"Location: /x\r\n\r\n")
            .unwrap()
            .0,
        302
    );
    assert_eq!(
        record::parse_cgi_response(b"Status: 404 Not Found\r\n\r\n")
            .unwrap()
            .0,
        404
    );
    assert!(record::parse_cgi_response(b"Status: 99\r\n\r\n").is_err());
    assert!(record::parse_cgi_response(b"no blank line").is_err());
    let built = record::build_cgi_response(201, &[("X-A".into(), "b".into())], b"ok");
    assert_eq!(
        String::from_utf8(built).unwrap(),
        "Status: 201 Created\r\nX-A: b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nok"
    );
    assert!(record::check_headers(Some(&json!({"Bad Name": "x"}))).is_err());
    assert!(record::check_headers(Some(&json!({"X": "a\r\nInjected: 1"}))).is_err());
    assert_eq!(
        record::decode_body(&json!({"body": "00ff", "body_encoding": "hex"})).unwrap(),
        vec![0, 255]
    );
}

fn split(mut bytes: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let len = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        let pad = bytes[6] as usize;
        out.push(Record {
            kind: bytes[1],
            request_id: u16::from_be_bytes([bytes[2], bytes[3]]),
            content: bytes[8..8 + len].to_vec(),
        });
        bytes = &bytes[8 + len + pad..];
    }
    out
}

async fn next(s: &mut TcpStream) -> Option<Record> {
    tokio::time::timeout(Duration::from_secs(10), record::read_record(s))
        .await
        .expect("no record")
        .ok()
        .flatten()
}

fn begin(id: u16, role: u16, flags: u8) -> Vec<u8> {
    record::encode(
        record::BEGIN_REQUEST,
        id,
        &record::begin_request(role, flags),
    )
}

fn params(id: u16, uri: &str) -> Vec<u8> {
    record::encode_stream(
        record::PARAMS,
        id,
        &record::encode_pairs([("REQUEST_METHOD", "GET"), ("REQUEST_URI", uri)]),
    )
}

/// Read records for `id` until END_REQUEST: (stdout, protocol status).
async fn answer(s: &mut TcpStream, id: u16) -> (Vec<u8>, u8) {
    let mut out = Vec::new();
    loop {
        let r = next(s).await.expect("connection closed before END_REQUEST");
        assert_eq!(r.request_id, id);
        match r.kind {
            record::STDOUT => out.extend(r.content),
            record::END_REQUEST => return (out, r.content[4]),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn management_roles_multiplexing_abort_and_bounds_on_the_wire() {
    let state = state();
    let (sid, addr) = server_in(&state, echo_policy(), json!({})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&record::encode(
        record::GET_VALUES,
        0,
        &record::encode_pairs([("FCGI_MPXS_CONNS", ""), ("FCGI_MAX_REQS", ""), ("NOPE", "")]),
    ))
    .await
    .unwrap();
    let r = next(&mut s).await.unwrap();
    assert_eq!((r.kind, r.request_id), (record::GET_VALUES_RESULT, 0));
    let values = record::decode_pairs(&r.content).unwrap();
    assert_eq!(values[0], ("FCGI_MPXS_CONNS".into(), "0".into()));
    assert_eq!(values.len(), 2, "unknown names are left out");
    s.write_all(&record::encode(99, 0, &[])).await.unwrap();
    let r = next(&mut s).await.unwrap();
    assert_eq!((r.kind, r.content[0]), (record::UNKNOWN_TYPE, 99));

    s.write_all(&begin(1, record::ROLE_AUTHORIZER, record::KEEP_CONN))
        .await
        .unwrap();
    let r = next(&mut s).await.unwrap();
    assert_eq!(
        (r.kind, r.request_id, r.content[4]),
        (record::END_REQUEST, 1, record::UNKNOWN_ROLE)
    );

    s.write_all(&begin(2, record::ROLE_RESPONDER, record::KEEP_CONN))
        .await
        .unwrap();
    s.write_all(&begin(3, record::ROLE_RESPONDER, record::KEEP_CONN))
        .await
        .unwrap();
    let r = next(&mut s).await.unwrap();
    assert_eq!(
        (r.kind, r.request_id, r.content[4]),
        (record::END_REQUEST, 3, record::CANT_MPX_CONN)
    );
    s.write_all(&record::encode(record::ABORT_REQUEST, 2, &[]))
        .await
        .unwrap();
    let r = next(&mut s).await.unwrap();
    assert_eq!(
        (r.kind, r.request_id, r.content[4]),
        (record::END_REQUEST, 2, record::REQUEST_COMPLETE)
    );

    // A complete request on the same kept connection, then one without KEEP_CONN.
    let mut req = begin(4, record::ROLE_RESPONDER, record::KEEP_CONN);
    req.extend(params(4, "/hello"));
    req.extend(record::encode_stream(record::STDIN, 4, b""));
    s.write_all(&req).await.unwrap();
    let (out, status) = answer(&mut s, 4).await;
    assert_eq!(status, record::REQUEST_COMPLETE);
    let (code, headers, body) = record::parse_cgi_response(&out).unwrap();
    assert_eq!((code, headers["x-from"].as_str()), (200, Some("netget")));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["keep"],
        true
    );
    let mut req = begin(5, record::ROLE_RESPONDER, 0);
    req.extend(params(5, "/bye"));
    req.extend(record::encode_stream(record::STDIN, 5, b""));
    s.write_all(&req).await.unwrap();
    answer(&mut s, 5).await;
    assert!(
        next(&mut s).await.is_none(),
        "closed after a request without KEEP_CONN"
    );

    // Oversized PARAMS and STDIN are answered with 431 and 413, and the connection ends.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = begin(1, record::ROLE_RESPONDER, record::KEEP_CONN);
    let big = "p".repeat(60_000);
    let pairs = record::encode_pairs([("A", big.as_str())]);
    req.extend(record::encode(record::PARAMS, 1, &pairs));
    req.extend(record::encode(record::PARAMS, 1, &pairs));
    s.write_all(&req).await.unwrap();
    let (out, _) = answer(&mut s, 1).await;
    assert_eq!(record::parse_cgi_response(&out).unwrap().0, 431);
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = begin(1, record::ROLE_RESPONDER, record::KEEP_CONN);
    req.extend(params(1, "/upload"));
    let chunk = vec![b'u'; 65_528];
    for _ in 0..17 {
        req.extend(record::encode(record::STDIN, 1, &chunk));
    }
    s.write_all(&req).await.unwrap();
    let (out, _) = answer(&mut s, 1).await;
    assert_eq!(record::parse_cgi_response(&out).unwrap().0, 413);
    // A record of another version ends the connection.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut bad = begin(1, record::ROLE_RESPONDER, 0);
    bad[0] = 2;
    s.write_all(&bad).await.unwrap();
    assert!(next(&mut s).await.is_none());
    let rows = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "fastcgi_request",
        2,
    )
    .await;
    assert_eq!(
        rows.len(),
        2,
        "only the two complete requests reached the handler"
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_responder_agree() {
    let state = state();
    let (sid, addr) = server_in(&state, echo_policy(), json!({})).await;
    let cid = client_in(&state, addr.to_string(), json!({"document_root": "/srv"}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    for a in [
        json!({"type":"fastcgi_request","path":"/hello","query":"a=1","headers":{"X-Test":"pair"}}),
        json!({"type":"fastcgi_request","method":"POST","path":"/submit","headers":{"Content-Type":"application/json"},"body":"{\"k\":1}"}),
        json!({"type":"fastcgi_request","path":"/teapot"}),
        json!({"type":"fastcgi_request","path":"/binary"}),
        json!({"type":"fastcgi_get_values"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(15))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "fastcgi_response", 4).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(r[0]["status"], 200);
    let echo: Value = serde_json::from_str(r[0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(
        echo,
        json!({"method": "GET", "uri": "/hello?a=1", "query": "a=1", "x_test": "pair", "keep": true})
    );
    assert_eq!(r[1]["status"], 201);
    assert_eq!(
        serde_json::from_str::<Value>(r[1]["body"].as_str().unwrap()).unwrap(),
        json!({"got": 7, "encoding": "utf8", "type": "application/json"})
    );
    assert_eq!(
        (r[2]["status"].as_u64(), r[2]["stderr"].as_str()),
        (Some(418), Some("teapot brewed"))
    );
    assert_eq!(
        (r[3]["body"].as_str(), r[3]["body_encoding"].as_str()),
        (Some("00ff"), Some("hex"))
    );
    let values = logs(&state, owner, "fastcgi_values", 1).await;
    assert_eq!(values[0].request["values"]["FCGI_MPXS_CONNS"], "0");
    let seen = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "fastcgi_request",
        4,
    )
    .await;
    assert_eq!(seen[0].request["params"]["SCRIPT_FILENAME"], "/srv/hello");
    let conns: std::collections::BTreeSet<_> = seen.iter().map(|r| r.connection_id).collect();
    assert_eq!(conns.len(), 1, "KEEP_CONN reused one connection");
    for bad in [
        json!({"type":"fastcgi_request","path":"no-slash"}),
        json!({"type":"fastcgi_request","path":"/x","headers":{"X":"a\nb"}}),
        json!({"type":"fastcgi_request","path":"/x","body":"zz","body_encoding":"hex"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, bad, Duration::from_secs(5))
                .await
                .unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
    }
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
