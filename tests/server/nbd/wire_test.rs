//! The NBD export model and server from the wire: reads, allocation runs and error regions;
//! NetGet's client against NetGet's server (structured replies, block status, an error region,
//! flush); a raw fixed-newstyle client getting simple replies, EPERM for writes, EINVAL past the
//! end, ERR_UNKNOWN for an unknown export, and a closed connection for an oversized option.
use crate::helpers::nbd::*;
use netget::server::nbd::wire::{self, Export, Piece};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[test]
fn export_model() {
    let e = Export::from_action(&json!({"size": 100, "extents": [{"offset": 10, "text": "abc"}, {"offset": 20, "length": 5, "fill": 7}], "errors": [{"offset": 50, "length": 10}]})).unwrap();
    assert_eq!(e.read(9, 5), b"\0abc\0");
    assert_eq!(e.read(19, 7), [0, 7, 7, 7, 7, 7, 0]);
    assert_eq!(
        e.runs(0, 100),
        vec![
            (0, 10, false),
            (10, 3, true),
            (13, 7, false),
            (20, 5, true),
            (25, 75, false)
        ]
    );
    assert_eq!(
        e.pieces(11, 4),
        vec![Piece::Data(11, b"bc".to_vec()), Piece::Hole(13, 2)]
    );
    assert_eq!(e.error_in(40, 20), Some((50, wire::EIO)));
    assert_eq!(e.error_in(0, 50), None);
    for bad in [
        json!({"size": 10, "extents": [{"offset": 8, "text": "abc"}]}),
        json!({"size": 10, "extents": [{"offset": 0}]}),
        json!({"size": 10, "errors": [{"offset": 0, "length": 4, "error": "EWHAT"}]}),
        json!({"size": 10, "block_size": {"minimum": 3}}),
        json!({"size": 2_u64 << 40}),
    ] {
        assert!(Export::from_action(&bad).is_err(), "{bad}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_server() {
    let state = state();
    let (sid, addr) = server_in(&state).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        json!({"export": "flaky", "list_exports": true}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let c = &logs(&state, owner, "nbd_connected", 1).await[0];
    assert_eq!(
        (
            c["size"].as_u64(),
            c["read_only"].as_bool(),
            c["structured_replies"].as_bool(),
            c["base_allocation"].as_bool()
        ),
        (Some(1 << 20), Some(true), Some(true), Some(true)),
        "{c}"
    );
    assert_eq!(c["exports"][0]["name"], "disk0");
    assert_eq!(c["block_size"]["preferred"], 4096);
    for a in [
        json!({"type": "nbd_read", "offset": 0, "length": 16}),
        json!({"type": "nbd_read", "offset": 8190, "length": 8}),
        json!({"type": "nbd_read", "offset": 520192, "length": 8192}),
        json!({"type": "nbd_read", "offset": 1048570, "length": 100}),
        json!({"type": "nbd_block_status", "offset": 0, "length": 8200}),
        json!({"type": "nbd_flush"}),
    ] {
        let r = state
            .send_to_client(cid, a.clone(), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    let reads = logs(&state, owner, "nbd_read_result", 4).await;
    assert_eq!(
        (
            reads[0]["data"].as_str(),
            reads[0]["data_encoding"].as_str()
        ),
        (Some("hello netget\0\0\0\0"), Some("utf8"))
    );
    assert_eq!(
        (
            reads[1]["data"].as_str(),
            reads[1]["data_encoding"].as_str()
        ),
        (Some("0000deadbeef0000"), Some("hex"))
    );
    assert_eq!(
        (
            reads[2]["error"].as_str(),
            reads[2]["error_offset"].as_u64()
        ),
        (Some("EIO"), Some(524288))
    );
    assert_eq!(reads[3]["error"], "EINVAL");
    let status = &logs(&state, owner, "nbd_block_status_result", 1).await[0];
    assert_eq!(
        status["extents"],
        json!([
            {"offset": 0, "length": 12, "hole": false, "zero": false},
            {"offset": 12, "length": 4084, "hole": true, "zero": true},
            {"offset": 4096, "length": 512, "hole": false, "zero": false},
            {"offset": 4608, "length": 3584, "hole": true, "zero": true},
            {"offset": 8192, "length": 4, "hole": false, "zero": false},
            {"offset": 8196, "length": 4, "hole": true, "zero": true}
        ])
    );
    assert_eq!(
        logs(&state, owner, "nbd_flush_result", 1).await[0]["error"],
        "OK"
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

async fn handshake(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut hello = [0u8; 18];
    s.read_exact(&mut hello).await.unwrap();
    assert_eq!(&hello[..8], b"NBDMAGIC");
    assert_eq!(u16::from_be_bytes([hello[16], hello[17]]), 3);
    s.write_all(&3u32.to_be_bytes()).await.unwrap();
    s
}

async fn option(s: &mut TcpStream, opt: u32, data: &[u8]) {
    let mut out = wire::IHAVEOPT.to_be_bytes().to_vec();
    out.extend(opt.to_be_bytes());
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(data);
    s.write_all(&out).await.unwrap();
}

async fn reply(s: &mut TcpStream) -> (u32, Vec<u8>) {
    let mut h = [0u8; 20];
    tokio::time::timeout(Duration::from_secs(20), s.read_exact(&mut h))
        .await
        .unwrap()
        .unwrap();
    let mut d = vec![0u8; u32::from_be_bytes(h[16..20].try_into().unwrap()) as usize];
    s.read_exact(&mut d).await.unwrap();
    (u32::from_be_bytes(h[12..16].try_into().unwrap()), d)
}

fn go(name: &str) -> Vec<u8> {
    let mut d = (name.len() as u32).to_be_bytes().to_vec();
    d.extend(name.as_bytes());
    d.extend(0u16.to_be_bytes());
    d
}

async fn request(
    s: &mut TcpStream,
    kind: u16,
    cookie: u64,
    offset: u64,
    length: u32,
    payload: &[u8],
) -> (u32, u64) {
    let mut r = wire::REQUEST_MAGIC.to_be_bytes().to_vec();
    r.extend(0u16.to_be_bytes());
    r.extend(kind.to_be_bytes());
    r.extend(cookie.to_be_bytes());
    r.extend(offset.to_be_bytes());
    r.extend(length.to_be_bytes());
    r.extend(payload);
    s.write_all(&r).await.unwrap();
    let mut h = [0u8; 16];
    tokio::time::timeout(Duration::from_secs(20), s.read_exact(&mut h))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        u32::from_be_bytes(h[..4].try_into().unwrap()),
        wire::SIMPLE_REPLY_MAGIC
    );
    (
        u32::from_be_bytes(h[4..8].try_into().unwrap()),
        u64::from_be_bytes(h[8..16].try_into().unwrap()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_negotiation_and_simple_replies() {
    let state = state();
    let (sid, addr) = server_in(&state).await;

    let mut s = handshake(addr).await;
    option(&mut s, wire::OPT_GO, &go("nope")).await;
    assert_eq!(reply(&mut s).await.0, wire::REP_ERR_UNKNOWN);
    option(&mut s, 42, &[]).await;
    assert_eq!(reply(&mut s).await.0, wire::REP_ERR_UNSUP);
    option(&mut s, wire::OPT_GO, &go("disk0")).await;
    let (kind, info) = reply(&mut s).await;
    assert_eq!(
        (kind, u64::from_be_bytes(info[2..10].try_into().unwrap())),
        (wire::REP_INFO, 1 << 20)
    );
    assert_eq!(reply(&mut s).await.0, wire::REP_ACK);
    // No structured replies were negotiated, so every answer is a simple reply.
    assert_eq!(request(&mut s, wire::CMD_READ, 7, 0, 5, &[]).await, (0, 7));
    let mut data = [0u8; 5];
    s.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"hello");
    assert_eq!(
        request(&mut s, wire::CMD_WRITE, 8, 0, 3, b"abc").await,
        (wire::EPERM, 8)
    );
    assert_eq!(
        request(&mut s, wire::CMD_READ, 9, (1 << 20) - 1, 2, &[]).await,
        (wire::EINVAL, 9)
    );
    assert_eq!(
        request(&mut s, wire::CMD_BLOCK_STATUS, 10, 0, 4096, &[]).await,
        (wire::EINVAL, 10)
    );
    assert_eq!(
        request(&mut s, wire::CMD_TRIM, 11, 0, 4096, &[]).await,
        (wire::EPERM, 11)
    );

    // An option declaring more than 64 KiB closes the connection before anything is read.
    let mut s = handshake(addr).await;
    let mut out = wire::IHAVEOPT.to_be_bytes().to_vec();
    out.extend(wire::OPT_GO.to_be_bytes());
    out.extend(((wire::MAX_OPTION + 1) as u32).to_be_bytes());
    s.write_all(&out).await.unwrap();
    let mut b = [0u8; 1];
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), s.read(&mut b))
            .await
            .unwrap(),
        Ok(0) | Err(_)
    ));
    state.remove_server(sid).await;
}
