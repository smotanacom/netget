//! `TcpStream::peek` on the virtual loopback.
//!
//! Thirty hyper-based servers under `src/server/` wait for the peer's first byte before
//! handing the socket to `serve_connection`, and they do it with `stream.peek(&mut [0u8; 1])`
//! — unchanged on wasm32, where `tokio` means this crate. A duplex pipe has no way to look at
//! a byte without taking it, so `peek` here reads and pushes back. These tests pin the four
//! properties that makes it a `peek` rather than a read:
//!
//! 1. a peek followed by a read returns the peeked bytes first and exactly once;
//! 2. two peeks in a row see the same bytes;
//! 3. a peek on a closed stream returns `Ok(0)` rather than hanging — callers use it as an
//!    end-of-stream test, and hanging there would hold a connection task open forever;
//! 4. every `AsyncRead` path drains the pushback before touching the pipe, including when the
//!    reader's buffer is smaller than what was peeked.
//!
//! They run on the host, not in a browser: the virtual network is plain in-memory duplexes and
//! needs no executor beyond something to poll the future.

use futures::executor::block_on;
use netget_tokio_wasm::io::{AsyncReadExt, AsyncWriteExt};
use netget_tokio_wasm::net::{TcpListener, TcpStream};

/// A connected pair on the virtual network: (what the server accepted, the client's end).
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let client = TcpStream::connect(addr).await.expect("connect");
    let (server, _peer) = listener.accept().await.expect("accept");
    (server, client)
}

#[test]
fn peek_then_read_returns_the_peeked_bytes_first_and_once() {
    block_on(async {
        let (mut server, mut client) = pair().await;
        client
            .write_all(b"GET / HTTP/1.1\r\n")
            .await
            .expect("write");

        let mut first = [0u8; 1];
        assert_eq!(server.peek(&mut first).await.expect("peek"), 1);
        assert_eq!(&first, b"G");

        // The whole request line is still there, starting with the peeked byte.
        let mut got = vec![0u8; 16];
        server.read_exact(&mut got).await.expect("read_exact");
        assert_eq!(&got, b"GET / HTTP/1.1\r\n");

        // And exactly once: nothing is replayed after it has been read.
        client.write_all(b"X").await.expect("write");
        let mut next = [0u8; 1];
        assert_eq!(server.read(&mut next).await.expect("read"), 1);
        assert_eq!(&next, b"X");
    });
}

#[test]
fn two_peeks_in_a_row_see_the_same_bytes() {
    block_on(async {
        let (mut server, mut client) = pair().await;
        client.write_all(b"hello").await.expect("write");

        let mut a = [0u8; 5];
        let mut b = [0u8; 5];
        let n_a = server.peek(&mut a).await.expect("first peek");
        let n_b = server.peek(&mut b).await.expect("second peek");
        assert_eq!(n_a, 5);
        assert_eq!(n_b, 5);
        assert_eq!(a, b);
        assert_eq!(&a, b"hello");

        // A third look through a read sees the same bytes again, still unconsumed.
        let mut read = [0u8; 5];
        server.read_exact(&mut read).await.expect("read_exact");
        assert_eq!(&read, b"hello");
    });
}

#[test]
fn peek_on_a_closed_stream_returns_zero() {
    block_on(async {
        let (mut server, client) = pair().await;
        drop(client);

        // Ok(0), not a hang and not an error: this is how the first-byte guards spell "the
        // peer connected and said nothing".
        let mut buf = [0u8; 1];
        assert_eq!(server.peek(&mut buf).await.expect("peek at eof"), 0);
        // Still Ok(0) when asked again, as a socket at end of stream is.
        assert_eq!(server.peek(&mut buf).await.expect("peek at eof"), 0);
        assert_eq!(server.read(&mut buf).await.expect("read at eof"), 0);
    });
}

#[test]
fn a_closed_stream_still_yields_what_was_sent_before_the_close() {
    block_on(async {
        let (mut server, mut client) = pair().await;
        client.write_all(b"hi").await.expect("write");
        drop(client);

        let mut buf = [0u8; 2];
        assert_eq!(server.peek(&mut buf).await.expect("peek"), 2);
        assert_eq!(&buf, b"hi");
        let mut all = Vec::new();
        server.read_to_end(&mut all).await.expect("read_to_end");
        assert_eq!(all, b"hi");
    });
}

#[test]
fn a_read_smaller_than_the_peek_drains_the_pushback_in_order() {
    block_on(async {
        let (mut server, mut client) = pair().await;
        client.write_all(b"abcdef").await.expect("write");

        let mut peeked = [0u8; 4];
        assert_eq!(server.peek(&mut peeked).await.expect("peek"), 4);
        assert_eq!(&peeked, b"abcd");

        // Two bytes at a time out of a four-byte pushback: the remainder stays, ahead of
        // whatever is still in the pipe.
        let mut two = [0u8; 2];
        assert_eq!(server.read(&mut two).await.expect("read"), 2);
        assert_eq!(&two, b"ab");
        assert_eq!(server.read(&mut two).await.expect("read"), 2);
        assert_eq!(&two, b"cd");

        // Only now does a read reach the pipe again, and it finds the rest intact.
        let mut rest = [0u8; 2];
        server.read_exact(&mut rest).await.expect("read_exact");
        assert_eq!(&rest, b"ef");
    });
}

#[test]
fn peek_returns_what_is_there_rather_than_waiting_to_fill_the_buffer() {
    block_on(async {
        let (server, mut client) = pair().await;
        client.write_all(b"ab").await.expect("write");

        // A 64-byte buffer against two bytes on the wire: `peek` resolves on what is
        // available, as tokio's does. Waiting for a full buffer would deadlock the
        // first-byte guards, which pass a one-byte buffer but meet peers that send more.
        let mut buf = [0u8; 64];
        assert_eq!(server.peek(&mut buf).await.expect("peek"), 2);
        assert_eq!(&buf[..2], b"ab");
    });
}

#[test]
fn splitting_carries_the_pushback_with_the_read_half() {
    block_on(async {
        let (server, mut client) = pair().await;
        client.write_all(b"PING\r\n").await.expect("write");

        let mut first = [0u8; 1];
        assert_eq!(server.peek(&mut first).await.expect("peek"), 1);
        assert_eq!(&first, b"P");

        // A server that peeks and then splits — or hands the whole stream to hyper, which
        // splits it — must not lose the byte it peeked.
        let (mut read, _write) = server.into_split();
        let mut got = vec![0u8; 6];
        read.read_exact(&mut got).await.expect("read_exact");
        assert_eq!(&got, b"PING\r\n");
    });
}

#[test]
fn a_borrowed_read_half_can_peek_too() {
    block_on(async {
        let (mut server, mut client) = pair().await;
        client.write_all(b"xyz").await.expect("write");

        let (mut read, _write) = server.split();
        let mut buf = [0u8; 1];
        assert_eq!(read.peek(&mut buf).await.expect("peek"), 1);
        assert_eq!(&buf, b"x");
        let mut got = [0u8; 3];
        read.read_exact(&mut got).await.expect("read_exact");
        assert_eq!(&got, b"xyz");
    });
}
