//! `src/server/accept_bounded.rs`: the connection cap every accept loop calls, and the two
//! read-deadline types the protocols whose loop belongs to a crate use instead.
//!
//! Each test here fails without the thing it is testing:
//!
//! * remove the `try_acquire` check in `accept_bounded` and the over-cap peer is admitted, so
//!   `cap_refuses_the_connection_past_the_limit_with_the_callers_own_bytes` never sees its
//!   refusal and never sees EOF;
//! * remove the `refuse` write and the same test reads zero bytes before EOF, which is the
//!   silent-drop this helper exists to prevent;
//! * drop the permit early — the mistake the module doc warns about — and
//!   `releasing_a_permit_readmits` passes for the wrong reason while
//!   `cap_counts_live_connections_not_accepts` fails;
//! * remove `IdleTimeoutReader`'s deadline and `idle_reader_fails_a_silent_peer` hangs past
//!   its own assertion window;
//! * make `IdleTimeoutReader` arm its deadline eagerly instead of lazily and
//!   `idle_reader_does_not_count_time_it_was_not_polled` fails — which is the whole
//!   live-but-slow-operation guarantee;
//! * drop the `in_flight` check from `ConnectionActivity::idle_for` and
//!   `busy_is_not_idle` fails.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test accept_bounded -- --test-threads=100

use std::sync::Arc;
use std::time::Duration;

use netget::server::accept_bounded::{
    accept_bounded, watch_idle, ConnectionActivity, ConnectionLimiter, IdleTimeoutReader,
    DEFAULT_MAX_CONNECTIONS,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BUSY: &[u8] = b"-ERR max number of clients reached\r\n";

/// Run an accept loop with `cap` slots that holds every admitted connection open until the
/// returned sender is dropped. Returns the port and a handle keeping admitted peers alive.
async fn bounded_server(cap: usize) -> (u16, tokio::sync::mpsc::UnboundedSender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (keep_tx, mut keep_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(cap);
        loop {
            let (socket, _peer, permit) =
                match accept_bounded(&listener, &limiter, BUSY, "TEST", None).await {
                    Ok(triple) => triple,
                    Err(_) => return,
                };
            tokio::spawn(async move {
                // The permit lives exactly as long as the connection task, which is what
                // makes the cap a cap on live connections.
                let _permit = permit;
                let _socket = socket;
                // Park forever; the test closes its own sockets to release these.
                let mut buf = [0u8; 1];
                let mut socket = _socket;
                let _ = socket.read(&mut buf).await;
            });
            if keep_rx.try_recv().is_ok() {
                return;
            }
        }
    });

    (port, keep_tx)
}

#[tokio::test]
async fn cap_refuses_the_connection_past_the_limit_with_the_callers_own_bytes() {
    let (port, _keep) = bounded_server(2).await;

    // Two admitted, held open.
    let _a = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("first");
    let _b = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("second");

    // The third is over the cap. It must read the refusal and then EOF, not a bare reset and
    // not silence: a client that cannot tell "full" from "crashed" retries forever.
    let mut third = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("third");
    let mut seen = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), third.read_to_end(&mut seen))
        .await
        .expect("refused connection should be closed promptly, not held open");
    read.expect("read the refusal");

    assert_eq!(
        seen,
        BUSY,
        "the refused peer must receive exactly the caller's refusal bytes, then EOF; got {:?}",
        String::from_utf8_lossy(&seen)
    );
}

#[tokio::test]
async fn cap_counts_live_connections_not_accepts() {
    let limiter = ConnectionLimiter::new(2);
    assert_eq!(limiter.in_use(), 0);
    let one = limiter.try_acquire().expect("first slot");
    let two = limiter.try_acquire().expect("second slot");
    assert_eq!(limiter.in_use(), 2);
    assert!(
        limiter.try_acquire().is_none(),
        "a third connection must be refused while two are live"
    );
    drop(one);
    drop(two);
    assert_eq!(limiter.in_use(), 0);
}

#[tokio::test]
async fn releasing_a_permit_readmits() {
    let (port, _keep) = bounded_server(1).await;

    let first = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("first");
    let mut refused = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("second");
    let mut seen = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), refused.read_to_end(&mut seen)).await;
    assert_eq!(seen, BUSY, "the second connection is over a cap of one");

    // Closing the first releases its permit when its task ends.
    drop(first);

    // The slot comes back.
    let mut admitted = None;
    for _ in 0..100 {
        let mut candidate = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("retry");
        let mut buf = [0u8; 8];
        match tokio::time::timeout(Duration::from_millis(100), candidate.read(&mut buf)).await {
            // Nothing written and nothing closed: this connection was admitted.
            Err(_) => {
                admitted = Some(candidate);
                break;
            }
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    assert!(
        admitted.is_some(),
        "the cap must free its slot when an admitted connection ends"
    );
}

#[tokio::test]
async fn default_cap_is_the_nfs_guards_number() {
    // Not a tautology: the point is that the one cap anyone argued for and the default every
    // other protocol inherits are the same number, and stay so.
    assert_eq!(DEFAULT_MAX_CONNECTIONS, 256);
}

#[tokio::test]
async fn idle_reader_fails_a_silent_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        let mut reader = IdleTimeoutReader::new(socket, Duration::from_millis(300));
        let mut buf = [0u8; 16];
        reader.read(&mut buf).await
    });

    // Connect and say nothing at all.
    let _peer = TcpStream::connect(("127.0.0.1", port)).await.expect("peer");

    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the reader must give up, not wait forever")
        .expect("task");
    let err = result.expect_err("a silent peer must produce a read error");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn idle_reader_does_not_count_time_it_was_not_polled() {
    // The guarantee that makes this safe for a protocol whose loop belongs to a crate: while
    // the crate is answering — an LLM round-trip, an event parked for a human — nothing polls
    // the reader, so no clock runs, and the next poll starts a fresh deadline. A reader that
    // reset its deadline only on a successful read would fail here, which is exactly the
    // live-transfer eviction this project learned about from TFTP.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        let mut reader = IdleTimeoutReader::new(socket, Duration::from_millis(300));
        let mut buf = [0u8; 16];
        let first = reader.read(&mut buf).await.expect("first read");

        // Stand in for the work that happens between reads. Far longer than the deadline.
        tokio::time::sleep(Duration::from_millis(900)).await;

        let second = reader.read(&mut buf).await.expect("second read");
        (first, second)
    });

    let mut peer = TcpStream::connect(("127.0.0.1", port)).await.expect("peer");
    peer.write_all(b"one").await.expect("write one");
    peer.flush().await.expect("flush");

    // Sent only after the server's pause is over, so the second read genuinely starts from a
    // fresh deadline rather than inheriting the elapsed one.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    peer.write_all(b"two").await.expect("write two");
    peer.flush().await.expect("flush");

    let (first, second) = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("no deadline should have fired")
        .expect("task");
    assert_eq!(first, 3);
    assert_eq!(second, 3);
}

#[tokio::test]
async fn idle_reader_uses_the_first_bound_until_the_peer_speaks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        // A short "has said nothing" bound and a long "is in a session" one, which is the
        // whois pair every protocol here declares.
        let mut reader = IdleTimeoutReader::with_first(
            socket,
            Duration::from_millis(250),
            Duration::from_secs(30),
        );
        let mut buf = [0u8; 16];
        reader.read(&mut buf).await
    });

    let _peer = TcpStream::connect(("127.0.0.1", port)).await.expect("peer");

    let err = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the short first bound must apply before the peer speaks")
        .expect("task")
        .expect_err("silent peer");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn busy_is_not_idle() {
    let activity = Arc::new(ConnectionActivity::new());
    assert!(
        activity.idle_for().is_some(),
        "a fresh connection with nothing in flight is idle"
    );

    {
        let _busy = activity.busy();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            activity.idle_for().is_none(),
            "a connection answering something is not idle, however long it takes — this is \
             what stops a request parked for a human being closed from under them"
        );
    }

    assert!(
        activity.idle_for().is_some(),
        "the guard must release the mark when the work ends"
    );
}

#[tokio::test]
async fn watch_idle_waits_for_the_work_to_finish_first() {
    let activity = Arc::new(ConnectionActivity::new());
    let busy = activity.busy();

    let watcher = tokio::spawn({
        let activity = Arc::clone(&activity);
        async move { watch_idle(activity, Duration::from_millis(200)).await }
    });

    // Well past the bound, but work is in flight, so the watchdog must not fire.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !watcher.is_finished(),
        "the idle watchdog fired while a request was still being answered"
    );

    drop(busy);
    tokio::time::timeout(Duration::from_secs(5), watcher)
        .await
        .expect("the watchdog must fire once the connection really is idle")
        .expect("task");
}
