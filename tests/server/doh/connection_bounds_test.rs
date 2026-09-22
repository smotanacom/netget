//! The connection cap on a real, running DoH server, driven from the wire.
//!
//! `TLS_HANDSHAKE_TIMEOUT` (10s) bounds how long *one* peer holds a connection before it has
//! handshaken, and after that hyper owns the HTTP/2 session. Neither bound says anything about
//! how many such peers there may be, and until September 2026 this accept loop admitted every
//! connection offered to it. DoH exists precisely so that a whole client population multiplexes
//! onto a few long-lived HTTP/2 connections, so the number of *sockets* a stranger can pin here
//! was the one thing nothing measured.
//!
//! Three claims:
//!
//! 1. `MAX_CONNECTIONS` peers are admitted — and here they are admitted *properly*, each
//!    completing a real TLS handshake. That is not decoration: a peer that merely opens a
//!    socket is evicted by `TLS_HANDSHAKE_TIMEOUT` after ten seconds, which would free slots on
//!    its own and let claim 3 below pass for a reason that has nothing to do with the permit.
//!    A handshaken connection is parked in hyper's HTTP/2 session instead, where nothing evicts
//!    it, so the cap is the only thing this test can be measuring.
//! 2. The next one is closed **with nothing written**, and that is the right answer rather than
//!    a shortcut. The three plaintext HTTP servers capped alongside this one answer 503 with
//!    `Retry-After`; here the peer is mid-`ClientHello` and feeds everything it reads to a TLS
//!    record parser, so `HTTP/1.1 503` is not a refusal but a malformed record, and the client
//!    records a TLS failure rather than backing off. Answering inside TLS would mean completing
//!    a handshake in order to refuse — a signature per refused connection — and RFC 8446 §6.2
//!    defines no alert meaning "at capacity". So the assertion is exactly "the handshake does
//!    not complete, promptly".
//! 3. Closing an admitted connection **frees exactly one slot**. A permit dropped before the
//!    connection ends un-caps the server silently; one never released wedges it shut after
//!    `MAX_CONNECTIONS` peers have ever connected.
//!
//! **How this was proved to fail without the cap**: replace the `accept_bounded` call in
//! `src/server/doh/mod.rs` with a bare `listener.accept().await` (and drop the permit from the
//! connection task). The over-cap peer's handshake then succeeds and the test fails on the
//! assertion that it must not.
//!
//! The server is model-free: an empty instruction really is model-free, where `None` is
//! replaced by a default one. No HTTP request is ever sent, so no model call is provoked.
//! Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features doh --test server -- doh::connection_bounds --test-threads=100

#![cfg(feature = "doh")]

use std::sync::Arc;
use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

/// `src/server/doh/mod.rs::MAX_CONNECTIONS`. Deliberately duplicated rather than imported: if
/// the constant moves, this test should be re-read rather than silently follow it.
const MAX_CONNECTIONS: usize = 256;

/// `src/server/doh/mod.rs::TLS_HANDSHAKE_TIMEOUT`, which is what the refusal has to beat for
/// "refused" and "admitted then evicted" to be distinguishable.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Accepts the server's self-signed certificate.
///
/// A copy of the one `dot`'s suite keeps, rather than a reuse of it: the two live behind
/// different feature gates, so a build with `doh` and without `dot` would not compile that
/// module at all.
#[derive(Debug)]
struct NoCertificateVerification;

impl rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("DoH server #{} never bound a port", id.as_u32());
}

async fn start_server(state: &AppState) -> (ServerId, u16) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "doh".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create doh server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port)
}

fn connector() -> TlsConnector {
    use rustls::crypto::CryptoProvider;
    let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let mut config = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    config
        .dangerous()
        .set_certificate_verifier(Arc::new(NoCertificateVerification));
    // RFC 8484 DoH is HTTP/2, and this server advertises `h2`. Offering it keeps the handshake
    // the one a real resolver performs rather than a degenerate variant of it.
    config.alpn_protocols = vec![b"h2".to_vec()];
    TlsConnector::from(Arc::new(config))
}

/// Open one DoH connection and complete its TLS handshake, or say why it did not.
///
/// A peer refused at the accept sees EOF before the ServerHello, which rustls reports as an
/// unexpected end of file — so `Err` here is the shape of the refusal, and `Ok` is the shape of
/// an admitted peer.
async fn handshake(port: u16, connector: &TlsConnector) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("static name");
    connector.connect(name, tcp).await
}

#[tokio::test]
async fn the_handshake_past_the_cap_is_refused_before_it_starts_and_the_slot_comes_back() {
    let state = new_state().await;
    let (_server_id, port) = start_server(&state).await;
    let connector = connector();

    let mut held = Vec::with_capacity(MAX_CONNECTIONS);
    for i in 0..MAX_CONNECTIONS {
        held.push(
            handshake(port, &connector)
                .await
                .unwrap_or_else(|e| panic!("handshake {i} of the cap failed: {e}")),
        );
    }

    let started = std::time::Instant::now();
    let refused = tokio::time::timeout(Duration::from_secs(20), handshake(port, &connector))
        .await
        .expect("the connection past the cap was neither refused nor handshaken — it hung");
    assert!(
        refused.is_err(),
        "the connection past the cap completed a TLS handshake, so it was admitted and there is \
         no cap"
    );
    // A refused peer is closed before the handshake begins; an admitted one that says nothing
    // is held for TLS_HANDSHAKE_TIMEOUT. Without this bound the assertion above would also pass
    // for a peer that was admitted and then evicted, which is the opposite outcome.
    assert!(
        started.elapsed() < TLS_HANDSHAKE_TIMEOUT / 3,
        "the refusal took {:?}, which is close enough to TLS_HANDSHAKE_TIMEOUT that this peer \
         may simply have been admitted and then evicted for saying nothing",
        started.elapsed()
    );

    // Every one of the 256 is a completed handshake parked in hyper's HTTP/2 session, so none
    // of them can have been evicted by the handshake deadline: whatever slot appears below came
    // from the permit and nothing else.
    assert_eq!(
        held.len(),
        MAX_CONNECTIONS,
        "the held connections must all still be open for the next assertion to mean anything"
    );
    drop(held.pop().expect("one held connection"));

    let mut admitted = false;
    for _ in 0..100 {
        match handshake(port, &connector).await {
            Ok(_stream) => {
                admitted = true;
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert!(
        admitted,
        "the cap never freed its slot after an admitted connection ended — the permit is being \
         held past the life of the connection, which wedges the server shut"
    );
}
