//! The D-Bus server's bounds and refusals from a raw socket, with messages built by hand where
//! NetGet's own encoder would refuse to build them: variants nested to exactly the depth bound
//! (answered) and one past it (connection closed, the server still serving), a message
//! announcing more than 1 MiB, an over-long SASL line, and the mechanisms each transport offers.
use super::real_client_test::start;
use netget::server::dbus::wire::{self, MAX_DEPTH, MAX_MESSAGE_BYTES};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct Raw {
    buf: Vec<u8>,
}

impl Raw {
    fn pad(&mut self, a: usize) {
        while !self.buf.len().is_multiple_of(a) {
            self.buf.push(0);
        }
    }
    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend(v.to_le_bytes());
    }
    fn field(&mut self, code: u8, sig: u8, value: &str) {
        self.pad(8);
        self.buf.extend([code, 1, sig, 0]);
        if sig == b'g' {
            self.buf.push(value.len() as u8);
            self.buf.extend(value.as_bytes());
            self.buf.push(0);
        } else {
            self.u32(value.len() as u32);
            self.buf.extend(value.as_bytes());
            self.buf.push(0);
        }
    }
}

/// A METHOD_CALL to /a Echo whose body is `body`, typed `signature`, as raw bytes.
fn raw_call(serial: u32, signature: &str, body: &[u8]) -> Vec<u8> {
    let mut m = Raw {
        buf: vec![b'l', 1, 0, 1],
    };
    m.u32(body.len() as u32);
    m.u32(serial);
    m.u32(0);
    let start = m.buf.len();
    m.field(1, b'o', "/a");
    m.field(3, b's', "Echo");
    m.field(8, b'g', signature);
    let len = (m.buf.len() - start) as u32;
    m.buf[12..16].copy_from_slice(&len.to_le_bytes());
    m.pad(8);
    m.buf.extend(body);
    m.buf
}

/// `levels` variants, each holding the next, the innermost holding the string "deep". The body
/// starts 8-aligned, so offsets within it are offsets within the message for alignment.
fn nested_variants(levels: usize) -> Vec<u8> {
    let mut b = Vec::new();
    for _ in 0..levels - 1 {
        b.extend([1, b'v', 0]);
    }
    b.extend([1, b's', 0]);
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    b.extend(4u32.to_le_bytes());
    b.extend(b"deep\0");
    b
}

async fn anonymous(port: u16) -> BufReader<TcpStream> {
    let mut s = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
    s.get_mut()
        .write_all(b"\0AUTH ANONYMOUS\r\n")
        .await
        .unwrap();
    let mut line = String::new();
    s.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("OK "), "{line}");
    s.get_mut().write_all(b"BEGIN\r\n").await.unwrap();
    s
}

async fn closed(s: &mut BufReader<TcpStream>) -> bool {
    let mut buf = [0u8; 64];
    matches!(
        tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

#[tokio::test]
async fn variant_depth_bound_and_message_size_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port) = start(&dir.path().join("bus"), json!({})).await;

    // Exactly the bound: decoded, handed to the policy, echoed back, and decoded again here.
    let mut s = anonymous(port).await;
    s.get_mut()
        .write_all(&raw_call(7, "v", &nested_variants(MAX_DEPTH)))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(20), wire::read_message(&mut s))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        (reply.kind, reply.reply_serial),
        (wire::METHOD_RETURN, Some(7)),
        "{reply:?}"
    );
    let mut v = &reply.body[0];
    for _ in 0..MAX_DEPTH - 1 {
        v = &v["value"];
    }
    assert_eq!(v, &json!({"signature": "s", "value": "deep"}));

    // One past it: the connection is closed, before anything reaches the model.
    let mut s = anonymous(port).await;
    s.get_mut()
        .write_all(&raw_call(8, "v", &nested_variants(MAX_DEPTH + 1)))
        .await
        .unwrap();
    assert!(
        closed(&mut s).await,
        "a variant nested past the bound was not refused"
    );

    // A header announcing more than the bound: closed without the body being sent at all.
    let mut s = anonymous(port).await;
    let mut huge = raw_call(9, "", &[]);
    huge[4..8].copy_from_slice(&(MAX_MESSAGE_BYTES as u32).to_le_bytes());
    s.get_mut().write_all(&huge).await.unwrap();
    assert!(
        closed(&mut s).await,
        "an over-size message was not refused from its header"
    );

    // And the server still serves.
    let mut s = anonymous(port).await;
    let mut ping = wire::Message::call("/a", Some("net.netget.Demo"), "Ping");
    ping.serial = 1;
    ping.signature = "s".into();
    ping.body = vec![json!("alive")];
    s.get_mut()
        .write_all(&ping.encode().unwrap())
        .await
        .unwrap();
    let reply = wire::read_message(&mut s).await.unwrap().unwrap();
    assert_eq!(reply.body, vec![json!("pong:alive")]);
}

#[tokio::test]
async fn sasl_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port) = start(&dir.path().join("bus"), json!({"allow_anonymous": false})).await;
    // Over TCP the peer's uid is unknown, so with ANONYMOUS off nothing is offered.
    let mut s = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
    s.get_mut()
        .write_all(b"\0AUTH EXTERNAL 30\r\n")
        .await
        .unwrap();
    let mut line = String::new();
    s.read_line(&mut line).await.unwrap();
    assert_eq!(line, "REJECTED \r\n");
    s.get_mut().write_all(b"AUTH ANONYMOUS\r\n").await.unwrap();
    line.clear();
    s.read_line(&mut line).await.unwrap();
    assert_eq!(line, "REJECTED \r\n");
    // A line longer than the bound ends the connection.
    let mut s = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
    let mut long = b"\0AUTH ".to_vec();
    long.extend(vec![b'A'; wire::MAX_AUTH_LINE + 1]);
    let _ = s.get_mut().write_all(&long).await;
    assert!(
        closed(&mut s).await,
        "an over-long SASL line was not refused"
    );

    // On the Unix socket EXTERNAL works for this process's own uid, and only that one.
    let socket = dir.path().join("bus");
    let mut u = BufReader::new(tokio::net::UnixStream::connect(&socket).await.unwrap());
    let other = hex::encode(((unsafe { libc::geteuid() }) + 1).to_string());
    u.get_mut()
        .write_all(format!("\0AUTH EXTERNAL {other}\r\n").as_bytes())
        .await
        .unwrap();
    line.clear();
    u.read_line(&mut line).await.unwrap();
    assert_eq!(line, "REJECTED EXTERNAL\r\n");
    let mine = hex::encode(unsafe { libc::geteuid() }.to_string());
    u.get_mut()
        .write_all(format!("AUTH EXTERNAL {mine}\r\n").as_bytes())
        .await
        .unwrap();
    line.clear();
    u.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("OK "), "{line}");
}

/// The encoder holds the same bound as the decoder: 64 nested variants marshal, 65 do not.
#[test]
fn encoder_depth_bound() {
    let nest = |levels: usize| {
        let mut v = json!("deep");
        for i in 0..levels {
            v = json!({"signature": if i == 0 { "s" } else { "v" }, "value": v});
        }
        v
    };
    wire::marshal("v", &[nest(MAX_DEPTH)]).expect("the bound itself marshals");
    assert!(
        wire::marshal("v", &[nest(MAX_DEPTH + 1)]).is_err(),
        "one past the bound marshalled"
    );
}
