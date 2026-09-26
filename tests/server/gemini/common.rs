//! Helpers shared by the Gemini suites: an in-process server started through `ServerForm`, a
//! raw TLS peer that accepts any certificate (Gemini clients trust on first use), and a
//! recording TCP relay for the pcap oracle.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_rustls::TlsConnector;

pub const DEAD_LLM: &str = "http://127.0.0.1:1";

pub async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, DEAD_LLM.to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(DEAD_LLM.to_string()))
        .await;
    state
}

pub async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("Gemini server #{} never bound a port", id.as_u32());
}

/// Start a Gemini server with the given handlers and startup parameters, model-free.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "gemini".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params,
        event_handlers: if handlers.is_empty() {
            None
        } else {
            Some(handlers)
        },
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create gemini server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

/// A deterministic capsule, as a Python script handler.
///
/// * `/` — a gemtext page exercising every line type, including a *text* line that begins with
///   `=>` and a preformatted line that begins with three backticks, both of which must survive
///   as what they are rather than becoming a link or closing the block.
/// * `/search` — input (10) with no query; the decoded query echoed back once answered.
/// * `/secret` — sensitive input (11).
/// * `/old` — permanent redirect (31) to `/new`.
/// * `/slow` — 44 with a wait.
/// * anything else — 51.
pub const CAPSULE_SCRIPT: &str = r#"import json, sys
e = json.load(sys.stdin)['event']
p = e.get('path', '/')
q = e.get('query')
if p == '/':
    a = [{'type': 'send_gemtext', 'lang': 'en', 'lines': [
        {'type': 'heading1', 'text': 'NetGet capsule'},
        {'type': 'text', 'text': 'Plain text.\n=> this is text, not a link'},
        {'type': 'link', 'url': '/about', 'text': 'About this capsule'},
        {'type': 'heading2', 'text': 'List'},
        {'type': 'list', 'text': 'first item'},
        {'type': 'quote', 'text': 'quoted wisdom'},
        {'type': 'preformatted', 'alt': 'art', 'text': ' /\\_/\\\n```not the end'},
        {'type': 'heading3', 'text': 'End'}]}]
elif p == '/search' and q is None:
    a = [{'type': 'send_gemini_input', 'prompt': 'Search for'}]
elif p == '/search':
    a = [{'type': 'send_gemtext', 'lines': [{'type': 'text', 'text': 'You searched for: ' + q}]}]
elif p == '/secret':
    a = [{'type': 'send_gemini_input', 'prompt': 'Password', 'sensitive': True}]
elif p == '/old':
    a = [{'type': 'send_gemini_redirect', 'url': '/new', 'permanent': True}]
elif p == '/slow':
    a = [{'type': 'send_gemini_response', 'status': 44, 'meta': '30'}]
else:
    a = [{'type': 'send_gemini_response', 'status': 51, 'meta': 'No such page'}]
print(json.dumps({'actions': a}))
"#;

/// The exact body `/` renders to.
pub const HOME_PAGE: &str = "# NetGet capsule\n\
Plain text.\n \
=> this is text, not a link\n\
=> /about About this capsule\n\
## List\n\
* first item\n\
> quoted wisdom\n\
```art\n /\\_/\\\n \
```not the end\n\
```\n\
### End\n";

pub fn capsule_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "script", "language": "python", "code": CAPSULE_SCRIPT}
    })
}

#[derive(Debug)]
struct AcceptAnyCertificate;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn connector() -> TlsConnector {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

pub type Tls = tokio_rustls::client::TlsStream<TcpStream>;

pub async fn tls_connect(port: u16) -> Tls {
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    tokio::time::timeout(Duration::from_secs(10), connector().connect(name, tcp))
        .await
        .expect("TLS handshake within 10s")
        .expect("TLS handshake")
}

/// [`tls_connect`] that reports a refused or failed handshake instead of panicking.
pub async fn try_tls_connect(port: u16) -> Option<Tls> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    tokio::time::timeout(Duration::from_secs(10), connector().connect(name, tcp))
        .await
        .ok()?
        .ok()
}

/// Send `request` (CRLF appended) and read everything until the server closes. Returns the
/// raw response bytes.
pub async fn raw_request(port: u16, request: &[u8], secs: u64) -> Vec<u8> {
    let mut tls = tls_connect(port).await;
    tls.write_all(request).await.expect("write request");
    tls.write_all(b"\r\n").await.expect("write CRLF");
    tls.flush().await.expect("flush");
    read_all(&mut tls, secs).await
}

/// Read to EOF. A clean `close_notify` and a bare TCP close both end the read; the response
/// is what matters here.
pub async fn read_all(tls: &mut Tls, secs: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(secs), tls.read(&mut buf)).await {
            Err(_) => panic!(
                "the Gemini server neither finished its response nor closed within {secs}s; \
                 got {:?}",
                String::from_utf8_lossy(&out)
            ),
            Ok(Ok(0)) => return out,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return out,
            Ok(Err(e)) => panic!("read error after {:?}: {e}", String::from_utf8_lossy(&out)),
        }
    }
}

/// Split a response into header line (without CRLF) and body.
pub fn split_response(bytes: &[u8]) -> (String, Vec<u8>) {
    let pos = bytes
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or_else(|| panic!("no CRLF in response {:?}", String::from_utf8_lossy(bytes)));
    (
        String::from_utf8(bytes[..pos].to_vec()).expect("utf-8 header"),
        bytes[pos + 2..].to_vec(),
    )
}

pub fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = rx.try_recv() {
        out.push(line);
    }
    out
}

pub async fn wait_for_log(
    rx: &mut mpsc::UnboundedReceiver<String>,
    needle: &str,
    secs: u64,
) -> Vec<String> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(line)) => {
                let hit = line.contains(needle);
                seen.push(line);
                if hit {
                    return seen;
                }
            }
            _ => panic!("no log line containing {needle:?} within {secs}s; saw {seen:#?}"),
        }
    }
}

/// One direction of relayed bytes, in the order they crossed.
#[derive(Debug, Clone)]
pub enum Chunk {
    ToServer(Vec<u8>),
    FromServer(Vec<u8>),
}

/// A TCP relay in front of `upstream` that records the first connection it carries, byte for
/// byte, so the pcap oracle can read exactly what NetGet put on the wire under TLS.
pub struct Recorder {
    pub port: u16,
    pub chunks: Arc<Mutex<Vec<Chunk>>>,
    pub done: Arc<tokio::sync::Notify>,
}

impl Recorder {
    pub async fn start(upstream: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let port = listener.local_addr().unwrap().port();
        let chunks: Arc<Mutex<Vec<Chunk>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let (rec, fin) = (chunks.clone(), done.clone());
        tokio::spawn(async move {
            let mut first = true;
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let server = TcpStream::connect(("127.0.0.1", upstream))
                    .await
                    .expect("relay connect upstream");
                let record = first.then(|| (rec.clone(), fin.clone()));
                first = false;
                tokio::spawn(relay(client, server, record));
            }
        });
        Self { port, chunks, done }
    }

    /// Wait until the recorded connection has closed in both directions.
    pub async fn finished(&self, secs: u64) -> Vec<Chunk> {
        tokio::time::timeout(Duration::from_secs(secs), self.done.notified())
            .await
            .expect("the recorded connection never finished");
        self.chunks.lock().await.clone()
    }
}

/// Where a recorded relay puts its chunks, and how it says it has finished.
type Recording = (Arc<Mutex<Vec<Chunk>>>, Arc<tokio::sync::Notify>);

async fn relay(client: TcpStream, server: TcpStream, record: Option<Recording>) {
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let rec_up = record.as_ref().map(|(c, _)| c.clone());
    let rec_down = record.as_ref().map(|(c, _)| c.clone());
    let up = async move {
        let mut buf = [0u8; 8192];
        loop {
            match cr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(r) = &rec_up {
                        r.lock().await.push(Chunk::ToServer(buf[..n].to_vec()));
                    }
                    if sw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sw.shutdown().await;
    };
    let down = async move {
        let mut buf = [0u8; 8192];
        loop {
            match sr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(r) = &rec_down {
                        r.lock().await.push(Chunk::FromServer(buf[..n].to_vec()));
                    }
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = cw.shutdown().await;
    };
    tokio::join!(up, down);
    if let Some((_, done)) = record {
        done.notify_one();
    }
}
