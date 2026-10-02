//! Virtual UDP uses in-memory channels; these checks need no runtime or sockets.
use futures::executor::block_on;
use netget_tokio_wasm::net::UdpSocket;
use std::io::ErrorKind;

#[test]
fn try_receive_reads_queued_datagrams_without_a_readable_call() {
    block_on(async {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(b"queued", receiver.local_addr().unwrap())
            .await
            .unwrap();
        let mut buffer = [0; 32];
        let (n, from) = receiver.try_recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"queued");
        assert_eq!(from, sender.local_addr().unwrap());
        assert_eq!(
            receiver.try_recv_from(&mut buffer).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
    });
}

#[test]
fn try_receive_consumes_peeked_bytes_before_later_datagrams() {
    block_on(async {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for data in [b"first".as_slice(), b"second"] {
            sender
                .send_to(data, receiver.local_addr().unwrap())
                .await
                .unwrap();
        }
        let mut buffer = [0; 32];
        receiver.peek_from(&mut buffer).await.unwrap();
        for expected in [b"first".as_slice(), b"second"] {
            let (n, _) = receiver.try_recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..n], expected);
        }
    });
}

#[test]
fn connected_try_receive_filters_other_peers_like_async_receive() {
    block_on(async {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buffer = [0; 32];
        assert_eq!(
            receiver.try_recv(&mut buffer).unwrap_err().kind(),
            ErrorKind::NotConnected
        );
        receiver.connect(peer.local_addr().unwrap()).await.unwrap();
        other
            .send_to(b"other", receiver.local_addr().unwrap())
            .await
            .unwrap();
        peer.send_to(b"peer", receiver.local_addr().unwrap())
            .await
            .unwrap();
        let n = receiver.try_recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"peer");
    });
}

#[test]
fn simultaneous_pending_peeks_cannot_overwrite_or_skip_a_datagram() {
    block_on(async {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut a = [0; 16];
        let mut b = [0; 16];
        let (a_result, b_result, _) = futures::join!(
            receiver.peek_from(&mut a),
            receiver.peek_from(&mut b),
            async {
                sender
                    .send_to(b"first", receiver.local_addr().unwrap())
                    .await
                    .unwrap();
                sender
                    .send_to(b"second", receiver.local_addr().unwrap())
                    .await
                    .unwrap();
            }
        );
        assert_eq!(&a[..a_result.unwrap().0], b"first");
        assert_eq!(&b[..b_result.unwrap().0], b"first");
        for expected in [b"first".as_slice(), b"second"] {
            let (n, _) = receiver.recv_from(&mut a).await.unwrap();
            assert_eq!(&a[..n], expected);
        }
    });
}

#[test]
fn connected_recv_from_and_peek_also_filter_non_peers() {
    block_on(async {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        receiver.connect(peer.local_addr().unwrap()).await.unwrap();
        other
            .send_to(b"other", receiver.local_addr().unwrap())
            .await
            .unwrap();
        peer.send_to(b"peer", receiver.local_addr().unwrap())
            .await
            .unwrap();
        let mut buf = [0; 16];
        let (n, from) = receiver.peek_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"peer");
        assert_eq!(from, peer.local_addr().unwrap());
        let (n, _) = receiver.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"peer");
    });
}

#[test]
fn udp_overflow_drops_new_arrivals_and_recovers_after_draining() {
    use netget_tokio_wasm::net::UDP_QUEUE_CAPACITY;
    block_on(async {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..UDP_QUEUE_CAPACITY + 20 {
            sender
                .send_to(b"x", receiver.local_addr().unwrap())
                .await
                .unwrap();
        }
        let mut buf = [0; 1];
        for _ in 0..UDP_QUEUE_CAPACITY {
            assert_eq!(receiver.try_recv_from(&mut buf).unwrap().0, 1);
        }
        assert_eq!(
            receiver.try_recv_from(&mut buf).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        sender
            .send_to(b"", receiver.local_addr().unwrap())
            .await
            .unwrap();
        assert_eq!(
            receiver.try_recv_from(&mut buf).unwrap().0,
            0,
            "zero-byte datagrams are real packets"
        );
    });
}
