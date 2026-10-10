//! LMTP server sessions over a raw socket: the RFC 2033 state machine, one reply per
//! accepted recipient after DATA, dot-unstuffing, the declared bounds and the fail-closed
//! paths. Handlers are scripts and static rules; no model is consulted.
use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};

/// Accepts alice and bob, defers busy, refuses everyone else.
pub const RECIPIENT_SCRIPT: &str = "import json,sys\ni=json.load(sys.stdin)\nr=i['event']['recipient'].split('@')[0]\nif r in ('alice','bob'):\n  a={'type':'lmtp_recipient_reply','accept':True}\nelif r=='busy':\n  a={'type':'lmtp_recipient_reply','accept':False,'temporary':True,'reason':'Try later'}\nelse:\n  a={'type':'lmtp_recipient_reply','accept':False,'reason':'No such user'}\nprint(json.dumps({'actions':[a]}))";

/// Delivers to alice (echoing what the server parsed), refuses bob permanently.
pub const DELIVERY_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nres=[]\nfor r in e['recipients']:\n  if r.startswith('alice'):\n    body=e['body'].strip().replace('\\r\\n','|').replace('\\n','|')\n    res.append({'recipient':r,'delivered':True,'reason':'Delivered subject='+(e.get('subject') or '')+' body='+body})\n  else:\n    res.append({'recipient':r,'delivered':False,'reason':'Mailbox full'})\nprint(json.dumps({'actions':[{'type':'lmtp_delivery','results':res}]}))";

pub fn scripted_handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"lmtp_recipient","handler":{"type":"script","language":"python","code":RECIPIENT_SCRIPT}}),
        json!({"event_pattern":"lmtp_message","handler":{"type":"script","language":"python","code":DELIVERY_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "lmtp".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Deliver mail for the test domain".into()),
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

pub async fn line(r: &mut BufReader<TcpStream>) -> String {
    let mut s = String::new();
    tokio::time::timeout(Duration::from_secs(10), r.read_line(&mut s))
        .await
        .expect("LMTP reply deadline")
        .unwrap();
    s
}

async fn expect(r: &mut BufReader<TcpStream>, expected: &[&str]) {
    for want in expected {
        assert_eq!(line(r).await, format!("{want}\r\n"));
    }
}

#[tokio::test]
async fn pipelined_session_answers_each_recipient_after_data() {
    let (state, id, addr) = start(scripted_handlers(), json!({"hostname":"mx.test"})).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    expect(&mut r, &["220 mx.test LMTP NetGet ready"]).await;
    r.get_mut()
        .write_all(
            b"EHLO client.test\r\nMAIL FROM:<s@example.test>\r\nLHLO client.test\r\n\
              MAIL FROM:<s@example.test> SIZE=100 BODY=8BITMIME\r\nRCPT TO:<alice@example.test>\r\n\
              RCPT TO:<nobody@example.test>\r\nRCPT TO:<busy@example.test>\r\nRCPT TO:<bob@example.test>\r\nDATA\r\n",
        )
        .await
        .unwrap();
    expect(
        &mut r,
        &[
            "500 5.5.1 This is an LMTP server; use LHLO",
            "503 5.5.1 Send LHLO first",
            "250-mx.test",
            "250-PIPELINING",
            "250-ENHANCEDSTATUSCODES",
            "250-8BITMIME",
            "250 SIZE 10485760",
            "250 2.1.0 Sender OK",
            "250 2.1.5 <alice@example.test> recipient OK",
            "550 5.1.1 No such user",
            "450 4.2.1 Try later",
            "250 2.1.5 <bob@example.test> recipient OK",
            "354 End data with <CR><LF>.<CR><LF>",
        ],
    )
    .await;
    r.get_mut()
        .write_all(
            b"Subject: Greetings\r\nFrom: s@example.test\r\n\r\nHello\r\n..dot line\r\n.\r\n",
        )
        .await
        .unwrap();
    expect(
        &mut r,
        &[
            "250 2.0.0 Delivered subject=Greetings body=Hello|.dot line",
            "550 5.0.0 Mailbox full",
        ],
    )
    .await;
    r.get_mut()
        .write_all(b"RSET\r\nNOOP\r\nVRFY alice\r\nDATA\r\nQUIT\r\n")
        .await
        .unwrap();
    expect(
        &mut r,
        &[
            "250 2.0.0 Flushed",
            "250 2.0.0 OK",
            "252 2.5.0 Cannot VRFY user, but will accept message and attempt delivery",
            "503 5.5.1 Need MAIL before DATA",
            "221 2.0.0 mx.test closing connection",
        ],
    )
    .await;
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    state.remove_server(id).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "stop releases the port"
    );
}

#[tokio::test]
async fn declared_size_and_line_bounds_are_enforced() {
    let accept_all = vec![
        json!({"event_pattern":"lmtp_recipient","handler":{"type":"static","actions":[{"type":"lmtp_recipient_reply","accept":true}]}}),
        json!({"event_pattern":"lmtp_message","handler":{"type":"static","actions":[{"type":"lmtp_delivery","deliver_all":true}]}}),
    ];
    let (state, id, addr) = start(accept_all, json!({"max_message_bytes": 64})).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    line(&mut r).await;
    r.get_mut()
        .write_all(b"LHLO c\r\nMAIL FROM:<a@b> SIZE=65\r\nMAIL FROM:<a@b> FOO=1\r\nMAIL FROM:<a@b>\r\nRCPT TO:<x@y>\r\nRCPT TO:<z@y>\r\nDATA\r\n")
        .await
        .unwrap();
    for _ in 0..5 {
        line(&mut r).await;
    }
    expect(
        &mut r,
        &[
            "552 5.3.4 Message size exceeds fixed limit",
            "555 5.5.4 MAIL parameter not recognized",
            "250 2.1.0 Sender OK",
            "250 2.1.5 <x@y> recipient OK",
            "250 2.1.5 <z@y> recipient OK",
            "354 End data with <CR><LF>.<CR><LF>",
        ],
    )
    .await;
    // 100 bytes of body against a 64-byte limit: drained, never stored, refused per recipient.
    r.get_mut()
        .write_all(format!("{}\r\n.\r\n", "x".repeat(100)).as_bytes())
        .await
        .unwrap();
    expect(
        &mut r,
        &[
            "552 5.3.4 Message too big for system",
            "552 5.3.4 Message too big for system",
        ],
    )
    .await;
    // A command line past 1000 bytes is refused without being kept, and the session goes on.
    r.get_mut()
        .write_all(format!("NOOP {}\r\nNOOP\r\n", "y".repeat(2000)).as_bytes())
        .await
        .unwrap();
    expect(&mut r, &["500 5.5.2 Line too long", "250 2.0.0 OK"]).await;
    // Twenty consecutive refused commands close the session with 421.
    r.get_mut()
        .write_all(b"BOGUS\r\n".repeat(20).as_slice())
        .await
        .unwrap();
    for _ in 0..20 {
        assert_eq!(line(&mut r).await, "500 5.5.2 Command unrecognized\r\n");
    }
    expect(&mut r, &["421 4.7.0 Too many errors, closing"]).await;
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    state.remove_server(id).await;
}

#[tokio::test]
async fn idle_deadline_closes_with_421() {
    let (state, id, addr) = start(scripted_handlers(), json!({"idle_timeout_secs": 1})).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    line(&mut r).await;
    expect(&mut r, &["421 4.4.2 Idle timeout, closing"]).await;
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    state.remove_server(id).await;
}

#[tokio::test]
async fn handler_failure_is_a_temporary_failure_never_a_delivery() {
    // No recipient handler: the unreachable model fails, so RCPT is deferred, not accepted.
    // The message handler names only alice, so bob fails closed with 451 rather than 250.
    let partial = vec![
        json!({"event_pattern":"lmtp_message","handler":{"type":"static","actions":[
            {"type":"lmtp_delivery","results":[{"recipient":"alice@example.test","delivered":true}]}
        ]}}),
    ];
    let (state, id, addr) = start(partial, json!({})).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    line(&mut r).await;
    r.get_mut()
        .write_all(b"LHLO c\r\nMAIL FROM:<s@example.test>\r\nRCPT TO:<alice@example.test>\r\n")
        .await
        .unwrap();
    for _ in 0..6 {
        line(&mut r).await;
    }
    expect(
        &mut r,
        &["451 4.3.0 Temporary local failure, try again later"],
    )
    .await;
    state.remove_server(id).await;

    let accept_all = vec![
        json!({"event_pattern":"lmtp_recipient","handler":{"type":"static","actions":[{"type":"lmtp_recipient_reply","accept":true}]}}),
        json!({"event_pattern":"lmtp_message","handler":{"type":"static","actions":[
            {"type":"lmtp_delivery","results":[{"recipient":"ALICE@example.test","delivered":true}]}
        ]}}),
    ];
    let (state, id, addr) = start(accept_all, json!({})).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    line(&mut r).await;
    r.get_mut()
        .write_all(b"LHLO c\r\nMAIL FROM:<s@example.test>\r\nRCPT TO:<alice@example.test>\r\nRCPT TO:<bob@example.test>\r\nDATA\r\nhi\r\n.\r\n")
        .await
        .unwrap();
    for _ in 0..9 {
        line(&mut r).await;
    }
    expect(
        &mut r,
        &[
            "250 2.0.0 <alice@example.test> delivered",
            "451 4.3.0 Temporary local failure, try again later",
        ],
    )
    .await;
    state.remove_server(id).await;
}
