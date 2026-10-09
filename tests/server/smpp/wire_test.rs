//! SMPP SMSC over raw PDUs: binds with configured credentials and by the handler, submits
//! accepted with a receipt and a mobile-originated reply, rejected with the handler's status,
//! long text through message_payload, enquire_link, unbind, the bind-state rules, and the
//! bounds and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::smpp::wire::{self, Pdu};
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Accepts numbers starting 1555 with a DELIVRD receipt and a "Got: " reply, rejects the rest.
pub const SUBMIT_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nif e['destination_addr'].startswith('1555'):\n  a={'type':'smpp_accept','receipt':'DELIVRD','reply_text':'Got: '+(e.get('text') or '?')}\nelse:\n  a={'type':'smpp_reject','status':'ESME_RINVDSTADR'}\nprint(json.dumps({'actions':[a]}))";

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"smpp_submit","handler":{"type":"script","language":"python","code":SUBMIT_SCRIPT}}),
    ]
}

pub fn credentials() -> Value {
    json!({"esme_system_id": "esme1", "password": "secret"})
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "smpp".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be an SMSC".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
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

struct Esme {
    s: TcpStream,
    seq: u32,
}

impl Esme {
    async fn connect(addr: SocketAddr) -> Self {
        Esme {
            s: TcpStream::connect(addr).await.unwrap(),
            seq: 0,
        }
    }
    async fn send(&mut self, command_id: u32, body: Vec<u8>) -> u32 {
        self.seq += 1;
        self.s
            .write_all(&Pdu::new(command_id, 0, self.seq, body).encode())
            .await
            .unwrap();
        self.seq
    }
    async fn recv(&mut self) -> Pdu {
        wire::read_pdu(&mut self.s, Duration::from_secs(20))
            .await
            .unwrap()
            .expect("a PDU")
    }
    async fn bind(&mut self, command_id: u32, user: &str, pass: &str) -> Pdu {
        let b = wire::Bind {
            system_id: user.into(),
            password: pass.into(),
            system_type: String::new(),
            interface_version: 0x34,
            address_range: String::new(),
        };
        self.send(command_id, wire::encode_bind(&b)).await;
        self.recv().await
    }
    async fn submit(&mut self, to: &str, text: &str, receipt: bool) -> u32 {
        let (coding, payload) = wire::encode_text(text);
        let m = wire::Message {
            source_addr: "src".into(),
            destination_addr: to.into(),
            dest_ton: 1,
            dest_npi: 1,
            registered_delivery: u8::from(receipt),
            data_coding: coding,
            payload,
            ..Default::default()
        };
        self.send(wire::SUBMIT_SM, wire::encode_message(&m)).await
    }
    async fn quiet(&mut self) {
        let mut buf = [0u8; 16];
        assert!(
            tokio::time::timeout(Duration::from_millis(500), self.s.read(&mut buf))
                .await
                .is_err(),
            "nothing more was sent"
        );
    }
    async fn closed(&mut self) {
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_secs(5), self.s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(n)) => panic!("expected a close, read {n} bytes"),
            Err(_) => panic!("the connection stayed open"),
        }
    }
}

fn cstring(body: &[u8]) -> String {
    wire::Reader::new(body).cstring(65).unwrap()
}

#[tokio::test]
async fn bind_submit_receipt_reply_and_session_rules() {
    let (state, id, addr) = start(handlers(), credentials()).await;
    let mut e = Esme::connect(addr).await;
    // Before a bind nothing is served.
    let seq = e.submit("15551230001", "early", false).await;
    let r = e.recv().await;
    assert_eq!(
        (r.command_id, r.status, r.sequence),
        (wire::SUBMIT_SM | wire::RESP, wire::ESME_RINVBNDSTS, seq)
    );
    let r = e.bind(wire::BIND_TRANSCEIVER, "esme1", "secret").await;
    assert_eq!(
        (r.command_id, r.status, cstring(&r.body)),
        (wire::BIND_TRANSCEIVER | wire::RESP, 0, "NETGET".into())
    );
    assert_eq!(
        e.bind(wire::BIND_TRANSCEIVER, "esme1", "secret")
            .await
            .status,
        wire::ESME_RALYBND
    );
    // Accepted: response, then the receipt, then the reply.
    let seq = e.submit("15551230001", "hello", true).await;
    let r = e.recv().await;
    assert_eq!(
        (r.command_id, r.status, r.sequence, cstring(&r.body)),
        (wire::SUBMIT_SM | wire::RESP, 0, seq, "NG00000001".into())
    );
    let receipt = e.recv().await;
    assert_eq!(receipt.command_id, wire::DELIVER_SM);
    let m = wire::parse_message(&receipt.body).unwrap();
    assert_eq!(
        (
            m.esm_class,
            m.source_addr.as_str(),
            m.destination_addr.as_str()
        ),
        (0x04, "15551230001", "src")
    );
    let text = String::from_utf8(m.payload.clone()).unwrap();
    assert!(
        text.starts_with("id:NG00000001 sub:001 dlvrd:001 submit date:")
            && text.contains(" stat:DELIVRD err:000 text:hello"),
        "{text}"
    );
    assert!(m
        .tlvs
        .contains(&(wire::TLV_RECEIPTED_MESSAGE_ID, b"NG00000001\0".to_vec())));
    assert!(m.tlvs.contains(&(wire::TLV_MESSAGE_STATE, vec![2])));
    e.send(wire::DELIVER_SM | wire::RESP, vec![0]).await;
    let reply = wire::parse_message(&e.recv().await.body).unwrap();
    assert_eq!(
        (
            reply.esm_class,
            wire::decode_text(reply.data_coding, &reply.payload).unwrap()
        ),
        (0, "Got: hello".into())
    );
    // UCS-2 and message_payload text reach the handler decoded.
    let long = "é".repeat(300);
    e.submit("15551230002", &long, false).await;
    assert_eq!(e.recv().await.status, 0);
    let reply = wire::parse_message(&e.recv().await.body).unwrap();
    assert_eq!(
        wire::decode_text(reply.data_coding, &reply.payload).unwrap(),
        format!("Got: {long}"),
        "a 600-octet reply rides in message_payload"
    );
    // Rejected with the handler's status, and nothing follows.
    e.submit("44990000000", "no", true).await;
    let r = e.recv().await;
    assert_eq!((r.status, r.body.clone()), (wire::ESME_RINVDSTADR, vec![0]));
    e.quiet().await;
    // enquire_link, an unknown command, unbind.
    let seq = e.send(wire::ENQUIRE_LINK, vec![]).await;
    let r = e.recv().await;
    assert_eq!(
        (r.command_id, r.sequence),
        (wire::ENQUIRE_LINK | wire::RESP, seq)
    );
    e.send(0x0000_0099, vec![]).await;
    let r = e.recv().await;
    assert_eq!(
        (r.command_id, r.status),
        (wire::GENERIC_NACK, wire::ESME_RINVCMDID)
    );
    e.send(wire::UNBIND, vec![]).await;
    assert_eq!(e.recv().await.command_id, wire::UNBIND | wire::RESP);
    e.closed().await;
    // A transmitter gets no receipt; a receiver may not submit.
    let mut e = Esme::connect(addr).await;
    e.bind(wire::BIND_TRANSMITTER, "esme1", "secret").await;
    e.submit("15551230003", "tx", true).await;
    assert_eq!(e.recv().await.status, 0);
    e.quiet().await;
    let mut e = Esme::connect(addr).await;
    e.bind(wire::BIND_RECEIVER, "esme1", "secret").await;
    e.submit("15551230003", "rx", false).await;
    assert_eq!(e.recv().await.status, wire::ESME_RINVBNDSTS);
    // Wrong credentials are refused and closed.
    let mut e = Esme::connect(addr).await;
    assert_eq!(
        e.bind(wire::BIND_TRANSCEIVER, "esme1", "wrong")
            .await
            .status,
        wire::ESME_RINVPASWD
    );
    e.closed().await;
    let mut e = Esme::connect(addr).await;
    assert_eq!(
        e.bind(wire::BIND_TRANSCEIVER, "intruder", "secret")
            .await
            .status,
        wire::ESME_RINVSYSID
    );
    e.closed().await;
    let logged = state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|x| serde_json::to_string(x).unwrap())
        .any(|x| x.contains(&long));
    assert!(
        logged,
        "the handler saw the whole UCS-2 message_payload text"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn handler_binds_bounds_and_failures() {
    // Without configured credentials the handler decides the bind.
    let bind = json!({"event_pattern":"smpp_bind","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'smpp_bind_accept'} if e['system_id']=='model-ok' and e['mode']=='transceiver' else {'type':'smpp_bind_reject','status':'ESME_RINVPASWD'}\nprint(json.dumps({'actions':[a]}))"}});
    let mut h = handlers();
    h.push(bind);
    let (state, id, addr) = start(h, json!({"idle_timeout_secs": 1, "system_id": "SMSC9"})).await;
    let mut e = Esme::connect(addr).await;
    let r = e.bind(wire::BIND_TRANSCEIVER, "model-ok", "x").await;
    assert_eq!((r.status, cstring(&r.body)), (0, "SMSC9".into()));
    let mut e = Esme::connect(addr).await;
    assert_eq!(
        e.bind(wire::BIND_TRANSCEIVER, "someone", "x").await.status,
        wire::ESME_RINVPASWD
    );
    e.closed().await;
    // command_length over the bound, or under the header, closes the session unread.
    let mut e = Esme::connect(addr).await;
    e.s.write_all(&((wire::MAX_PDU + 1) as u32).to_be_bytes())
        .await
        .unwrap();
    e.closed().await;
    let mut e = Esme::connect(addr).await;
    e.s.write_all(&15u32.to_be_bytes()).await.unwrap();
    e.closed().await;
    // A silent session is closed after idle_timeout_secs.
    let mut e = Esme::connect(addr).await;
    e.closed().await;
    state.remove_server(id).await;
    // No handler: a submit is answered ESME_RSYSERR, a bind ESME_RBINDFAIL; never accepted.
    let (state, id, addr) = start(vec![], credentials()).await;
    let mut e = Esme::connect(addr).await;
    e.bind(wire::BIND_TRANSCEIVER, "esme1", "secret").await;
    e.submit("15551230001", "hello", true).await;
    assert_eq!(e.recv().await.status, wire::ESME_RSYSERR);
    e.quiet().await;
    state.remove_server(id).await;
    let (state, id, addr) = start(vec![], json!({})).await;
    let mut e = Esme::connect(addr).await;
    assert_eq!(
        e.bind(wire::BIND_TRANSCEIVER, "anyone", "x").await.status,
        wire::ESME_RBINDFAIL
    );
    state.remove_server(id).await;
}
