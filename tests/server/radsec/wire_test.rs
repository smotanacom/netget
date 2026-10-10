//! RadSec over raw TLS with the RADIUS codec: mutual TLS (a client without a certificate, or
//! with one from another CA, is refused at the handshake), accept and reject through RADIUS's
//! decision path with both signatures verified, pipelined requests on one connection, a forged
//! Message-Authenticator dropped, the length bound, and the fail-closed Access-Reject.
use crate::helpers::radsec_pki::{make, Pki};
use netget::cli::management::ServerForm;
use netget::client::radius::wire::{access_request, verify_reply, Credential};
use netget::server::radius::packet::{
    Attribute, RadiusPacket, ATTR_REPLY_MESSAGE, ATTR_USER_NAME, CODE_ACCESS_ACCEPT,
    CODE_ACCESS_REJECT, CODE_ACCESS_REQUEST,
};
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// alice/wonderland is accepted with a Reply-Message; everyone else is rejected.
pub const POLICY: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
if e.get('user_name')=='alice' and e.get('password')=='wonderland':
  a=[{'type':'send_access_accept','reply_message':'welcome alice'}]
else:
  a=[{'type':'send_access_reject','reply_message':'go away'}]
print(json.dumps({'actions':a}))"#;

pub fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"radius_access_request","handler":{"type":"script","language":"python","code":POLICY}}),
    ]
}

pub async fn start(
    pki: &Pki,
    handlers: Vec<Value>,
    mutual: bool,
) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let mut params =
        json!({"certificate_file": pki.server_cert, "private_key_file": pki.server_key});
    if mutual {
        params["ca_file"] = json!(pki.ca);
    }
    let id = ServerForm {
        protocol: "radsec".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Authenticate".into()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

/// A TLS connection as the client cert/key pair given (or none).
async fn connect(
    addr: SocketAddr,
    pki: &Pki,
    cert: Option<(&std::path::Path, &std::path::Path)>,
) -> std::io::Result<Tls> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(&pki.ca)?)) {
        roots.add(c?).unwrap();
    }
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let config = match cert {
        Some((c, k)) => {
            let chain =
                rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(c)?))
                    .collect::<std::io::Result<Vec<_>>>()?;
            let key =
                rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(k)?))?
                    .unwrap();
            builder.with_client_auth_cert(chain, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    let tcp = TcpStream::connect(addr).await?;
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await?;
    // TLS 1.3 reports a refused client certificate only after the handshake: a probe write and
    // read surface it here, where the caller expects it.
    tls.flush().await?;
    Ok(tls)
}

async fn read_reply(tls: &mut Tls) -> std::io::Result<Vec<u8>> {
    let mut head = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(20), tls.read_exact(&mut head))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no reply"))??;
    let len = u16::from_be_bytes([head[2], head[3]]) as usize;
    let mut out = head.to_vec();
    out.resize(len, 0);
    tls.read_exact(&mut out[4..]).await?;
    Ok(out)
}

fn request(id: u8, user: &str, password: &str) -> (Vec<u8>, [u8; 16]) {
    let ra: [u8; 16] = rand::random();
    let packet = access_request(
        id,
        &ra,
        Credential::Pap(password.as_bytes()),
        &[Attribute::text(ATTR_USER_NAME, user)],
        b"radsec",
    )
    .unwrap();
    (packet, ra)
}

fn reply_message(p: &RadiusPacket) -> String {
    String::from_utf8_lossy(p.first(ATTR_REPLY_MESSAGE).unwrap_or_default()).into_owned()
}

#[tokio::test]
async fn mutual_tls_decisions_and_pipelining() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    let (_state, _id, addr) = start(&pki, policy(), true).await;
    // Without a client certificate, and with one from another CA: refused.
    for cert in [
        None,
        Some((pki.stranger_cert.as_path(), pki.stranger_key.as_path())),
    ] {
        let refused = async {
            let mut tls = connect(addr, &pki, cert).await?;
            tls.write_all(&request(1, "alice", "wonderland").0).await?;
            read_reply(&mut tls).await
        }
        .await;
        assert!(
            refused.is_err(),
            "a client {} was served",
            if cert.is_some() {
                "from another CA"
            } else {
                "without a certificate"
            }
        );
    }
    // With ours: two requests pipelined on one connection, each answered and fully verified.
    let mut tls = connect(
        addr,
        &pki,
        Some((pki.client_cert.as_path(), pki.client_key.as_path())),
    )
    .await
    .unwrap();
    let (good, good_ra) = request(7, "alice", "wonderland");
    let (bad, bad_ra) = request(8, "mallory", "guess");
    tls.write_all(&[good, bad].concat()).await.unwrap();
    let mut seen = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let raw = read_reply(&mut tls).await.unwrap();
        let ra = if raw[1] == 7 { good_ra } else { bad_ra };
        let p = verify_reply(&raw, CODE_ACCESS_REQUEST, &ra, b"radsec")
            .expect("both signatures verify");
        seen.insert(p.identifier, (p.code, reply_message(&p)));
    }
    assert_eq!(seen[&7], (CODE_ACCESS_ACCEPT, "welcome alice".into()));
    assert_eq!(seen[&8], (CODE_ACCESS_REJECT, "go away".into()));
    // A forged Message-Authenticator is dropped; the connection goes on serving.
    let (mut forged, _) = request(9, "alice", "wonderland");
    let at = forged.len() - 16;
    forged[at] ^= 0xff;
    tls.write_all(&forged).await.unwrap();
    let (next, next_ra) = request(10, "alice", "wonderland");
    tls.write_all(&next).await.unwrap();
    let raw = read_reply(&mut tls).await.unwrap();
    assert_eq!(raw[1], 10, "the forged request 9 must get no reply");
    verify_reply(&raw, CODE_ACCESS_REQUEST, &next_ra, b"radsec").unwrap();
    // Had 9 been answered, its reply would be on the wire before 11's request is even sent.
    let (last, last_ra) = request(11, "mallory", "x");
    tls.write_all(&last).await.unwrap();
    let raw = read_reply(&mut tls).await.unwrap();
    assert_eq!(raw[1], 11, "the forged request 9 must get no reply");
    verify_reply(&raw, CODE_ACCESS_REQUEST, &last_ra, b"radsec").unwrap();
}

#[tokio::test]
async fn length_bound_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    // No handler, no reachable model: the RADIUS fail-closed Access-Reject, still signed.
    let (_state, _id, addr) = start(&pki, vec![], false).await;
    let mut tls = connect(addr, &pki, None).await.unwrap();
    let (req, ra) = request(3, "alice", "wonderland");
    tls.write_all(&req).await.unwrap();
    let raw = read_reply(&mut tls).await.unwrap();
    let p = verify_reply(&raw, CODE_ACCESS_REQUEST, &ra, b"radsec").unwrap();
    assert_eq!(p.code, CODE_ACCESS_REJECT);
    assert_eq!(
        reply_message(&p),
        "Access denied: no authorization decision was produced"
    );
    // A length of 4097, and of 19: the stream cannot be resynchronised, so it is closed.
    for len in [4097u16, 19] {
        let mut tls = connect(addr, &pki, None).await.unwrap();
        let mut bogus = vec![CODE_ACCESS_REQUEST, 1];
        bogus.extend(len.to_be_bytes());
        tls.write_all(&bogus).await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(Duration::from_secs(10), tls.read(&mut buf))
            .await
            .expect("closed, not left hanging")
            .unwrap_or(0);
        assert_eq!(n, 0, "length {len}: connection must close without a reply");
    }
}
