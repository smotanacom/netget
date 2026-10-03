use futures::{executor::block_on, FutureExt};
use netget_tokio_wasm::net::{TcpListener, TcpStream, TCP_BACKLOG};

#[test]
fn full_accept_queue_applies_backpressure_and_resumes_after_accept() {
    block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let mut clients = Vec::new();
        for _ in 0..TCP_BACKLOG {
            clients.push(TcpStream::connect(target).await.unwrap());
        }
        assert!(TcpStream::connect(target).now_or_never().is_none());
        let _accepted = listener.accept().await.unwrap();
        clients.push(
            TcpStream::connect(target)
                .now_or_never()
                .expect("accept freed backlog slot")
                .unwrap(),
        );
        let (result, _) = futures::join!(TcpStream::connect(target), async {
            drop(listener);
        });
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionRefused
        );
    });
}
