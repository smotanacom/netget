//! The connection bounds of a hyper-based server, driven from the wire.
//!
//! Eleven package-registry and cloud-API servers here are the same shape — `hyper::server::conn::http1`
//! over a plain `TcpStream` — and they got the same pair of bounds: a first-byte deadline applied
//! with `TcpStream::peek` before hyper sees the socket, a longer idle deadline over
//! `ConnectionActivity`, and `accept_bounded`'s connection cap. The three checks below are what
//! prove each one is wired rather than merely declared, and every protocol's
//! `connection_bounds_test.rs` calls them with its own numbers.
//!
//! **Each fails without the thing it tests.** Remove the `peek` deadline and
//! [`HttpBounds::silent_peer_is_closed_at_the_first_byte_bound`] hangs until its own assertion
//! window expires. Collapse the pair onto one short number and
//! [`HttpBounds::an_answered_connection_outlives_the_first_byte_bound`] fails, because a client
//! that made one request and paused is closed at the first-byte bound. Remove the `accept_bounded`
//! call — or drop the permit early — and
//! [`HttpBounds::the_connection_past_the_cap_is_refused_and_the_slot_comes_back`] sees the peer
//! over the cap served instead of refused.
//!
//! No mock backend: the LLM endpoint is a dead port, so every request is answered on the
//! protocol's own fail-closed path. That is deliberate — these are assertions about *deadlines*,
//! and a fail-closed reply proves a connection is alive exactly as well as a real answer would.
//! Loopback only.

#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// One protocol's declared bounds, as its own `mod.rs` states them.
///
/// The numbers are **copied** rather than imported: if a protocol changes a bound, this test
/// should be re-read and re-argued, not silently follow it.
pub struct HttpBounds {
    /// The id `ServerForm` starts the server under (`npm`, `oci-registry`, …).
    pub protocol: &'static str,
    /// What to call it in an assertion message.
    pub label: &'static str,
    /// `FIRST_BYTE_READ_TIMEOUT` in that protocol's `mod.rs`.
    pub first_byte: Duration,
    /// `IDLE_BETWEEN_REQUESTS_TIMEOUT` in that protocol's `mod.rs`.
    pub idle: Duration,
    /// `MAX_CONNECTIONS` in that protocol's `mod.rs`.
    pub max_connections: usize,
    /// A request line this server will answer *something* to. The status does not matter.
    pub probe: &'static str,
}

impl HttpBounds {
    async fn state() -> AppState {
        // A dead LLM endpoint: nothing here asserts on an answer's content, and a request that
        // fails closed proves the connection is alive just as well.
        let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
        state
            .set_llm_client(netget::llm::OllamaClient::new(
                "http://127.0.0.1:1".to_string(),
            ))
            .await;
        state
    }

    async fn start(&self) -> (AppState, ServerId, u16) {
        let state = Self::state().await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let server_id = ServerForm {
            protocol: self.protocol.to_string(),
            port: Some(0),
            // An empty instruction is genuinely model-free; `None` is replaced by a default one.
            instruction: Some(String::new()),
            ..Default::default()
        }
        .create(&state, tx)
        .await
        .unwrap_or_else(|e| panic!("create {} server: {e}", self.label));

        for _ in 0..300 {
            if let Some(s) = state.get_server(server_id).await {
                if let Some(addr) = s.local_addr {
                    return (state, server_id, addr.port());
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!("{} server never bound a port", self.label);
    }

    /// A peer that connects and never sends a byte holds a socket, a connection task and an
    /// `AppState` row. Without the first-byte bound it holds them forever.
    pub async fn silent_peer_is_closed_at_the_first_byte_bound(&self) {
        let (state, server_id, port) = self.start().await;
        let mut peer = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");

        let started = std::time::Instant::now();
        let mut sink = Vec::new();
        // Generous against the declared bound so an ordinary scheduling delay under
        // --test-threads=100 is not mistaken for a missing deadline; what is being asserted is
        // that the read ends at all.
        let read = tokio::time::timeout(
            self.first_byte + Duration::from_secs(30),
            peer.read_to_end(&mut sink),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            read.is_ok(),
            "a {} peer that connected and sent nothing was still holding the socket, the \
             connection task and its AppState entry after {}s — the first-byte deadline is not \
             being applied",
            self.label,
            elapsed.as_secs()
        );
        read.unwrap().expect("read to EOF");
        assert!(
            sink.is_empty(),
            "HTTP is client-speaks-first and {} has nothing to say to a peer that asked \
             nothing; it should close, not write. Got {:?}",
            self.label,
            String::from_utf8_lossy(&sink)
        );
        assert!(
            elapsed >= self.first_byte / 2,
            "closed after only {}ms — that is not the declared {}s bound, it is something else \
             tearing the connection down",
            elapsed.as_millis(),
            self.first_byte.as_secs()
        );

        let _ = state.remove_server(server_id).await;
    }

    /// The point of the pair: "has said nothing at all" and "has gone quiet between requests"
    /// are different claims and get different answers. One number applied to both would close a
    /// client that is mid-install while it unpacks what it just fetched.
    pub async fn an_answered_connection_outlives_the_first_byte_bound(&self) {
        assert!(
            self.idle > self.first_byte + Duration::from_secs(20),
            "{}: this check is only meaningful when the idle bound is materially longer than \
             the first-byte one",
            self.label
        );

        let (state, server_id, port) = self.start().await;
        let mut peer = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");

        let first = self.exchange(&mut peer).await;
        assert!(
            first.starts_with("HTTP/1."),
            "{} did not answer the first request at all (got {:?})",
            self.label,
            first
        );

        // Go quiet for longer than the first-byte bound. On a collapsed pair the connection is
        // gone by the time this returns.
        tokio::time::sleep(self.first_byte + Duration::from_secs(8)).await;

        let second = self.exchange(&mut peer).await;
        assert!(
            second.starts_with("HTTP/1."),
            "a {} connection that had already been answered was closed after {}s of silence. A \
             registry or SDK client is legitimately idle between requests while it does local \
             work: the first-byte bound must not be applied to a connection that has spoken. \
             (got {:?})",
            self.label,
            (self.first_byte + Duration::from_secs(8)).as_secs(),
            second
        );

        let _ = state.remove_server(server_id).await;
    }

    /// The cap, and the two ways it fails silently: a permit released early (no cap at all) and
    /// a permit never released (the server wedges shut after `MAX_CONNECTIONS` peers have *ever*
    /// connected, which is worse than no cap).
    pub async fn the_connection_past_the_cap_is_refused_and_the_slot_comes_back(&self) {
        let (state, server_id, port) = self.start().await;

        // Fill the cap with peers that say nothing — the cheapest way to hold a slot, and the
        // reason a cap is needed at all. They are well inside the first-byte bound for the few
        // seconds this takes.
        let mut held = Vec::with_capacity(self.max_connections);
        for i in 0..self.max_connections {
            held.push(
                TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap_or_else(|e| {
                        panic!("{}: connection {i} of the cap refused: {e}", self.label)
                    }),
            );
        }

        // `connect` still succeeds — the listen backlog completes the handshake before the
        // accept loop sees it — so the refusal has to be observable as bytes plus a close.
        let mut over = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect past the cap");
        let mut refusal = Vec::new();
        let read =
            tokio::time::timeout(Duration::from_secs(20), over.read_to_end(&mut refusal)).await;
        assert!(
            read.is_ok(),
            "{}: the connection past the cap of {} was neither refused nor closed. Unbounded, \
             it would simply have been served.",
            self.label,
            self.max_connections
        );
        read.unwrap().expect("read the refusal");
        let refusal = String::from_utf8_lossy(&refusal).to_string();
        assert!(
            refusal.contains("503") && refusal.contains("Retry-After"),
            "{}: a cap that drops the socket silently is worse than no cap — the client cannot \
             tell a full server from a crashed one. Expected a 503 with Retry-After, got {:?}",
            self.label,
            refusal
        );
        assert!(
            !refusal.contains("netget") && !refusal.contains("LLM"),
            "{}: the refusal leaked netget's own internals onto the wire: {:?}",
            self.label,
            refusal
        );

        // Give a slot back and take it again, with a real request, which also proves the server
        // is still serving — a cap that killed the listener would satisfy the assertion above
        // perfectly.
        drop(held.pop().expect("the cap is not zero"));
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut answered = None;
        while std::time::Instant::now() < deadline {
            let mut fresh = match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let reply = self.exchange(&mut fresh).await;
            if reply.starts_with("HTTP/1.") {
                answered = Some(reply);
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            answered.is_some(),
            "{}: releasing one of {} connections did not free a slot. Either the permit is never \
             released — in which case the server wedges shut after {} peers have ever connected — \
             or the server stopped serving.",
            self.label,
            self.max_connections,
            self.max_connections
        );

        let _ = state.remove_server(server_id).await;
    }

    /// Send the probe request and read the whole reply, using a quiet period to decide the
    /// response is complete. Returns "" if the connection was closed instead.
    async fn exchange(&self, peer: &mut TcpStream) -> String {
        if peer.write_all(self.probe.as_bytes()).await.is_err() {
            return String::new();
        }
        if peer.flush().await.is_err() {
            return String::new();
        }
        let mut out = Vec::new();
        let overall = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let mut buf = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(500), peer.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Ok(Err(_)) => break,
                // A quiet moment after something arrived means the reply is complete; a quiet
                // moment before anything arrived just means the server is still thinking.
                Err(_) => {
                    if !out.is_empty() || std::time::Instant::now() > overall {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }
}
