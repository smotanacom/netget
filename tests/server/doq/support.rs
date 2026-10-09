#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    llm::OllamaClient,
    state::{AccessLogOwner, AppState, ClientId, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub struct Fixture {
    pub state: AppState,
    pub dir: tempfile::TempDir,
    pub cert: rustls::pki_types::CertificateDer<'static>,
    pub server: ServerId,
    pub addr: SocketAddr,
    pub status_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
}
impl Fixture {
    pub async fn new(mut params: Value, handler: Option<Value>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.path().join("cert.pem"), issued.cert.pem()).unwrap();
        std::fs::write(
            dir.path().join("key.pem"),
            issued.signing_key.serialize_pem(),
        )
        .unwrap();
        params["cert_path"] = json!(dir.path().join("cert.pem"));
        params["key_path"] = json!(dir.path().join("key.pem"));
        let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
        state
            .set_llm_client(OllamaClient::new("http://127.0.0.1:1"))
            .await;
        let (tx, rx) = mpsc::unbounded_channel();
        let server = ServerForm {
            protocol: "doq".into(),
            host: Some("127.0.0.1".into()),
            port: Some(0),
            instruction: Some("Answer DNS queries".into()),
            startup_params: Some(params),
            event_handlers: handler
                .map(|handler| vec![json!({"event_pattern":"doq_query","handler":handler})]),
            ..Default::default()
        }
        .create(&state, tx)
        .await
        .unwrap();
        let addr = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let s = state.get_server(server).await.unwrap();
                if let Some(addr) = s.local_addr {
                    break addr;
                }
                if let netget::state::ServerStatus::Error(e) = s.status {
                    panic!("server failed: {e}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        Self {
            state,
            dir,
            cert: issued.cert.der().clone(),
            server,
            addr,
            status_rx: tokio::sync::Mutex::new(rx),
        }
    }
    pub async fn decision(&self, expected: &str) {
        let mut rx = self.status_rx.lock().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line = rx.recv().await.expect("status channel must remain open");
                if line.contains(expected) {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing {expected}"));
    }
    pub async fn standard() -> Self {
        Self::new(json!({}),Some(json!({"type":"script","language":"python","code":"import json,sys\nevent=json.load(sys.stdin)['event']\ndef respond(actions):\n    print(json.dumps({'actions':actions}))\nkind=event['query_type']\nif kind == 'A':\n    respond([{'type':'send_dns_a_response','domain':event['domain'],'query_id':42,'ip':'192.0.2.19','ttl':17}])\nelif kind == 'AAAA':\n    respond([{'type':'send_dns_aaaa_response','domain':event['domain'],'ip':'2001:db8::19','ttl':23}])\nelse:\n    respond([{'type':'send_dns_nxdomain','domain':event['domain'],'query_type':kind}])"}))).await
    }
    pub fn endpoint(&self, alpn: &[u8]) -> quinn::Endpoint {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.cert.clone()).unwrap();
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![alpn.to_vec()];
        let config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
        ));
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(config);
        endpoint
    }
    pub async fn peer(&self) -> (quinn::Endpoint, quinn::Connection) {
        let endpoint = self.endpoint(b"doq");
        let connection = endpoint
            .connect(self.addr, "localhost")
            .unwrap()
            .await
            .unwrap();
        (endpoint, connection)
    }
    pub async fn client(&self, handlers: Vec<Value>, extra: Value) -> ClientId {
        let mut params =
            json!({"ca_cert_path":self.dir.path().join("cert.pem"),"server_name":"localhost"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (tx, _) = mpsc::unbounded_channel();
        let id = ClientForm {
            protocol: "doq".into(),
            remote_addr: Some(self.addr.to_string()),
            instruction: Some("Query DNS".into()),
            startup_params: Some(params),
            event_handlers: Some(handlers),
            ..Default::default()
        }
        .create(&self.state, OllamaClient::new("http://127.0.0.1:1"), tx)
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.state.has_client_handle(id).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        id
    }
    pub async fn close(&self) {
        self.state.remove_server(self.server).await;
    }
}
pub fn empty_handler() -> Value {
    json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})
}
pub fn query(domain: &str, kind: &str) -> hickory_proto::op::Message {
    netget::client::doq::build_query(&json!({"domain":domain,"query_type":kind})).unwrap()
}
pub async fn wait_log(state: &AppState, owner: AccessLogOwner, needle: &str) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if state
                .list_access_logs_for(Some(owner), None)
                .await
                .iter()
                .any(|v| serde_json::to_string(v).unwrap().contains(needle))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no log containing {needle}"));
}
