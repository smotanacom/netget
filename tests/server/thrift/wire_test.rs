//! The Thrift codecs and transports from the wire: every value type round-trips through binary
//! and compact, a short buffer is reported as incomplete rather than malformed, a nesting bomb is
//! refused without exhausting the stack, the IDL parser refuses what it does not support, and a
//! server bounds frames, answers a byte-at-a-time unframed peer, answers pipelined calls in order
//! and refuses an unknown method without asking the handler.
use crate::helpers::thrift::*;
use netget::server::thrift::{codec, codec::Message, codec::Protocol, codec::Tv, idl};
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn every_type() -> Tv {
    Tv::Struct(vec![
        (1, Tv::Bool(true)),
        (2, Tv::Bool(false)),
        (3, Tv::Byte(-7)),
        (4, Tv::I16(-300)),
        (5, Tv::I32(70_000)),
        (6, Tv::I64(-(1 << 40))),
        (7, Tv::Double(2.5)),
        (8, Tv::Bin("héllo".as_bytes().to_vec())),
        (9, Tv::Uuid(*b"0123456789abcdef")),
        (
            20,
            Tv::List(codec::T_BOOL, vec![Tv::Bool(true), Tv::Bool(false)]),
        ),
        (21, Tv::Set(codec::T_I32, vec![Tv::I32(1), Tv::I32(-1)])),
        (
            300,
            Tv::Map(
                codec::T_STRING,
                codec::T_STRUCT,
                vec![(
                    Tv::Bin(b"k".to_vec()),
                    Tv::Struct(vec![(1, Tv::I64(9)), (-2, Tv::Bool(true))]),
                )],
            ),
        ),
        (301, Tv::List(codec::T_I64, vec![])),
    ])
}

#[test]
fn values_round_trip_in_both_protocols() {
    for protocol in [Protocol::Binary, Protocol::Compact] {
        let m = Message {
            name: "everything".into(),
            kind: codec::CALL,
            seqid: 1 << 20,
            body: every_type(),
        };
        let bytes = codec::encode(&m, protocol);
        let (back, detected, used) = codec::decode(&bytes).unwrap();
        assert_eq!((back, detected, used), (m.clone(), protocol, bytes.len()));
        for cut in 0..bytes.len() {
            let e = codec::decode(&bytes[..cut]).unwrap_err();
            assert!(codec::is_incomplete(&e), "{protocol:?} cut at {cut}: {e}");
        }
    }
}

#[test]
fn nesting_bomb_is_refused() {
    // Compact CALL "a", then 10 000 nested struct fields (one byte each).
    let mut b = vec![0x82, 0x21, 0x01, 0x01, b'a'];
    b.extend(std::iter::repeat_n(0x1c, 10_000));
    b.extend(std::iter::repeat_n(0x00, 10_001));
    let e = codec::decode(&b).unwrap_err();
    assert!(
        !codec::is_incomplete(&e) && e.to_string().contains("nest"),
        "{e}"
    );
    // The same in strict binary: field header 0x0c 0x00 0x01 per level.
    let mut b = vec![0x80, 0x01, 0x00, 0x01, 0, 0, 0, 1, b'a', 0, 0, 0, 1];
    for _ in 0..10_000 {
        b.extend([0x0c, 0x00, 0x01]);
    }
    b.extend(std::iter::repeat_n(0x00, 10_001));
    let e = codec::decode(&b).unwrap_err();
    assert!(
        !codec::is_incomplete(&e) && e.to_string().contains("nest"),
        "{e}"
    );
}

#[test]
fn idl_parses_the_test_service_and_refuses_the_rest() {
    let parsed = idl::parse(&idl()).unwrap();
    let svc = parsed.service(Some("Users")).unwrap();
    let names: Vec<_> = svc.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["add", "get_user", "find", "touch", "ping"]);
    assert!(svc.functions[4].oneway);
    assert_eq!(parsed.enum_value("Role", "USER"), Some(2));
    for bad in [
        "include \"shared.thrift\"\nservice S { void f() }",
        "service S { Missing f() }",
        "struct A { 1: i32 x, 1: i32 y }",
        "service S { void f(1: list<i32 x) }",
    ] {
        assert!(idl::parse(bad).is_err(), "accepted: {bad}");
    }
    assert!(idl::parse(&"/* */".repeat(idl::MAX_IDL)).is_err());
}

async fn exchange(stream: &mut TcpStream, framed: bool, m: &Message, p: Protocol) -> Message {
    let mut out = codec::encode(m, p);
    if framed {
        let mut f = (out.len() as u32).to_be_bytes().to_vec();
        f.extend(out);
        out = f;
    }
    stream.write_all(&out).await.unwrap();
    read(stream, framed).await
}

async fn read(stream: &mut TcpStream, framed: bool) -> Message {
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the server closed the connection");
            buf.extend_from_slice(&chunk[..n]);
            let body = if framed {
                if buf.len() < 4 {
                    continue;
                }
                assert_eq!(
                    u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize,
                    buf.len() - 4
                );
                &buf[4..]
            } else {
                &buf[..]
            };
            if let Ok((m, _, used)) = codec::decode(body) {
                assert_eq!(used, body.len());
                return m;
            }
        }
    })
    .await
    .expect("no reply")
}

fn call(name: &str, seqid: i32, fields: Vec<(i16, Tv)>) -> Message {
    Message {
        name: name.into(),
        kind: codec::CALL,
        seqid,
        body: Tv::Struct(fields),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn server_bounds_frames_and_handles_every_transport_shape() {
    let state = state();
    let (sid, addr) = server_in(&state, policy()).await;
    let owner = AccessLogOwner::Server(sid.as_u32());

    // An unknown method is refused without a handler call.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let r = exchange(
        &mut s,
        true,
        &call("drop_tables", 5, vec![]),
        Protocol::Binary,
    )
    .await;
    assert_eq!(
        (r.kind, r.seqid, r.name.as_str()),
        (codec::EXCEPTION, 5, "drop_tables")
    );
    assert_eq!(r.body.field(2), Some(&Tv::I32(codec::UNKNOWN_METHOD)));

    // A frame declaring more than the bound closes the connection before anything is buffered.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&((codec::MAX_MESSAGE as u32) + 1).to_be_bytes())
        .await
        .unwrap();
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut b))
        .await
        .unwrap();
    assert!(matches!(n, Ok(0) | Err(_)), "{n:?}");

    // Unframed compact, one byte per write: still answered once the call is whole.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.set_nodelay(true).unwrap();
    let bytes = codec::encode(
        &call("add", 9, vec![(1, Tv::I32(20)), (2, Tv::I32(22))]),
        Protocol::Compact,
    );
    for byte in &bytes {
        s.write_all(&[*byte]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let r = read(&mut s, false).await;
    assert_eq!((r.kind, r.seqid), (codec::REPLY, 9));
    assert_eq!(r.body.field(0), Some(&Tv::I32(42)));

    // Two unframed binary calls in one write are answered in order on the same connection.
    let mut both = codec::encode(
        &call("add", 1, vec![(1, Tv::I32(1)), (2, Tv::I32(1))]),
        Protocol::Binary,
    );
    both.extend(codec::encode(
        &call("get_user", 2, vec![(1, Tv::I64(404))]),
        Protocol::Binary,
    ));
    s.write_all(&both).await.unwrap();
    let first = read(&mut s, false).await;
    assert_eq!((first.seqid, first.body.field(0)), (1, Some(&Tv::I32(2))));
    let second = read(&mut s, false).await;
    assert_eq!(second.seqid, 2);
    assert_eq!(
        second.body.field(1),
        Some(&Tv::Struct(vec![
            (1, Tv::Bin(b"no user 404".to_vec())),
            (2, Tv::I64(404))
        ]))
    );

    let calls = logs(&state, owner, "thrift_call", 3).await;
    assert!(calls.iter().all(|c| c.request["method"] != "drop_tables"));
    assert_eq!(calls[0].request["args"], json!({"a": 20, "b": 22}));
    state.remove_server(sid).await;
}
