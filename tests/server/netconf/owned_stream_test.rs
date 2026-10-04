//! The connection owner reaches russh's detached driver through the stream: dropping the
//! owner wakes a read parked on a silent socket and closes it for the peer.
use netget::server::netconf::owned_stream::OwnedStream;
use std::{future::poll_fn, io, pin::Pin, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

#[tokio::test]
async fn owner_drop_wakes_a_polled_silent_reader_and_closes_the_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let native = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let (mut stream, owner) = OwnedStream::new(native);
    let (observed, ready) = oneshot::channel();
    let driver = tokio::spawn(async move {
        let mut observed = Some(observed);
        let mut byte = [0u8; 1];
        let result = poll_fn(|cx| {
            let mut buffer = ReadBuf::new(&mut byte);
            let result = Pin::new(&mut stream).poll_read(cx, &mut buffer);
            if result.is_pending() {
                if let Some(observed) = observed.take() {
                    observed.send(()).unwrap();
                }
            }
            result
        })
        .await;
        drop(stream);
        result
    });
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    drop(owner);
    let error = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    let mut byte = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}
