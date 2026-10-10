//! Milter filter over raw packets built with NetGet's codec: negotiation, each stage's
//! decision, headers and body collected, the end-of-message verdict with modifications (only
//! those the MTA allowed), and the bounds and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::milter::wire;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{io::AsyncReadExt, net::TcpStream, sync::mpsc};

/// evil.example is refused at connect, a sender with "spammer" gets 550 5.7.1, spam@ recipients
/// are rejected; a message is tagged, its Subject prefixed and audit@ added, unless its Subject
/// is "quarantine me".
pub const FILTER_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[{'type':'milter_continue'}]
if t=='milter_connect' and e['hostname']=='evil.example': a=[{'type':'milter_reject'}]
elif t=='milter_mail' and 'spammer' in e['sender']: a=[{'type':'milter_reply','code':550,'xcode':'5.7.1','text':'Sender rejected by policy'}]
elif t=='milter_rcpt' and e['recipient'].startswith('<spam@'): a=[{'type':'milter_reject'}]
elif t=='milter_message':
  subj=[h['value'] for h in e['headers'] if h['name'].lower()=='subject']
  if subj and subj[0]=='quarantine me': a=[{'type':'milter_quarantine','reason':'asked to'},{'type':'milter_accept'}]
  else: a=[{'type':'milter_add_header','name':'X-NetGet','value':'checked'},{'type':'milter_change_header','name':'Subject','index':1,'value':'[netget] '+(subj[0] if subj else '')},{'type':'milter_add_rcpt','recipient':'<audit@example.com>'},{'type':'milter_accept'}]
print(json.dumps({'actions':a}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":FILTER_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "milter".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Filter mail".into()),
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

struct Mta {
    s: TcpStream,
}

impl Mta {
    async fn new(addr: SocketAddr, actions: u32) -> (Self, u32) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        wire::write(&mut s, wire::C_OPTNEG, &wire::optneg(6, actions, 0))
            .await
            .unwrap();
        let (cmd, d) = wire::read_packet(&mut s, Duration::from_secs(10))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cmd, wire::C_OPTNEG);
        (Self { s }, wire::parse_optneg(&d).unwrap().1)
    }
    async fn send(&mut self, cmd: u8, data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        wire::write(&mut self.s, cmd, data).await.unwrap();
        let mut out = Vec::new();
        loop {
            let (c, d) = wire::read_packet(&mut self.s, Duration::from_secs(20))
                .await
                .unwrap()
                .expect("a reply");
            let last = matches!(
                c,
                wire::R_CONTINUE
                    | wire::R_ACCEPT
                    | wire::R_REJECT
                    | wire::R_TEMPFAIL
                    | wire::R_DISCARD
                    | wire::R_REPLYCODE
            );
            out.push((c, d));
            if last {
                return out;
            }
        }
    }
}

#[tokio::test]
async fn stages_and_end_of_message() {
    let (_state, _id, addr) = start(handlers()).await;
    // The MTA allows only adding headers: the other modifications are not sent.
    let (mut m, agreed) = Mta::new(addr, wire::F_ADDHDRS).await;
    assert_eq!(agreed, wire::F_ADDHDRS);
    let r = m
        .send(
            wire::C_CONNECT,
            &wire::connect_body("client.example", '4', 4000, "192.0.2.10"),
        )
        .await;
    assert_eq!(r, vec![(wire::R_CONTINUE, vec![])]);
    assert_eq!(
        m.send(
            wire::C_MAIL,
            &wire::cstrings(&["<alice@example.com>", "SIZE=100"])
        )
        .await,
        vec![(wire::R_CONTINUE, vec![])]
    );
    assert_eq!(
        m.send(wire::C_RCPT, &wire::cstrings(&["<spam@example.net>"]))
            .await,
        vec![(wire::R_REJECT, vec![])]
    );
    assert_eq!(
        m.send(wire::C_RCPT, &wire::cstrings(&["<bob@example.net>"]))
            .await,
        vec![(wire::R_CONTINUE, vec![])]
    );
    assert_eq!(m.send(wire::C_DATA, &[]).await[0].0, wire::R_CONTINUE);
    assert_eq!(
        m.send(wire::C_HEADER, &wire::cstrings(&["Subject", "hello"]))
            .await[0]
            .0,
        wire::R_CONTINUE
    );
    assert_eq!(m.send(wire::C_EOH, &[]).await[0].0, wire::R_CONTINUE);
    assert_eq!(
        m.send(wire::C_BODY, b"Hi Bob\r\n").await[0].0,
        wire::R_CONTINUE
    );
    let r = m.send(wire::C_BODYEOB, &[]).await;
    assert_eq!(
        r,
        vec![
            (wire::R_ADDHEADER, wire::cstrings(&["X-NetGet", "checked"])),
            (wire::R_ACCEPT, vec![])
        ]
    );
    // With every action allowed, all of them arrive before the verdict.
    let (mut m, _) = Mta::new(addr, wire::ALL_ACTIONS).await;
    m.send(wire::C_MAIL, &wire::cstrings(&["<alice@example.com>"]))
        .await;
    m.send(wire::C_HEADER, &wire::cstrings(&["Subject", "hello"]))
        .await;
    let r = m.send(wire::C_BODYEOB, b"body").await;
    let kinds: Vec<u8> = r.iter().map(|(c, _)| *c).collect();
    assert_eq!(
        kinds,
        vec![
            wire::R_ADDHEADER,
            wire::R_CHGHEADER,
            wire::R_ADDRCPT,
            wire::R_ACCEPT
        ]
    );
    assert_eq!(
        wire::strings(&r[1].1[4..]),
        vec!["Subject", "[netget] hello"]
    );
    // The handler's own SMTP reply.
    let r = m
        .send(wire::C_MAIL, &wire::cstrings(&["<spammer@bad.example>"]))
        .await;
    assert_eq!(
        r,
        vec![(
            wire::R_REPLYCODE,
            wire::cstrings(&["550 5.7.1 Sender rejected by policy"])
        )]
    );
}

#[tokio::test]
async fn bounds_and_fail_closed() {
    let (_state, _id, addr) = start(handlers()).await;
    // An oversized packet length closes the connection without a reply.
    let mut s = TcpStream::connect(addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut s, &((wire::MAX_PACKET as u32) + 1).to_be_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
    // An unknown command closes it too.
    let (mut m, _) = Mta::new(addr, wire::ALL_ACTIONS).await;
    wire::write(&mut m.s, b'Z', &[]).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(10), m.s.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
    // No handler: every decision is tempfail — the sender retries; nothing is passed or lost.
    let (_state, _id, addr) = start(vec![]).await;
    let (mut m, _) = Mta::new(addr, wire::ALL_ACTIONS).await;
    assert_eq!(
        m.send(
            wire::C_CONNECT,
            &wire::connect_body("c", '4', 1, "192.0.2.1")
        )
        .await,
        vec![(wire::R_TEMPFAIL, vec![])]
    );
    assert_eq!(
        m.send(wire::C_BODYEOB, &[]).await,
        vec![(wire::R_TEMPFAIL, vec![])]
    );
}
