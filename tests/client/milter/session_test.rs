//! The Milter client against NetGet's own filter (a small policy script; the server suite's lives
//! in another test target): a handler-driven connect → helo → mail → rcpt → message chain, an
//! injected refused recipient, local refusals and the follow-up depth bound.
use netget::{
    cli::management::{ClientForm, ServerForm},
    server::milter::wire,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// Each stage that continues leads to the next; the message carries one Subject header.
pub const CHAIN: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; a=[]
nxt={'connect':{'type':'milter_helo','name':'client.example'},
     'helo':{'type':'milter_mail','sender':'<alice@example.com>'},
     'mail':{'type':'milter_rcpt','recipient':'<bob@example.net>'},
     'rcpt':{'type':'milter_message','headers':[{'name':'Subject','value':'hello'},{'name':'From','value':'Alice <alice@example.com>'}],'body':'Hi Bob\r\n'}}
if e['decision']=='continue' and e['stage'] in nxt: a=[nxt[e['stage']]]
print(json.dumps({'actions':a}))"#;

pub fn chain_handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"milter_negotiated","handler":{"type":"static","actions":[
            {"type":"milter_connect","hostname":"client.example","address":"192.0.2.10","port":40000}]}}),
        json!({"event_pattern":"milter_reply","handler":{"type":"script","language":"python","code":CHAIN}}),
    ]
}

pub async fn client(remote: String, handlers: Vec<Value>) -> (AppState, ClientId) {
    let (state, id, _status) = client_with_status(remote, handlers).await;
    (state, id)
}

/// The client and the status lines it emits.
async fn client_with_status(
    remote: String,
    handlers: Vec<Value>,
) -> (AppState, ClientId, mpsc::UnboundedReceiver<String>) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, rx) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "milter".into(),
        remote_addr: Some(remote),
        instruction: Some("Pass a message through the filter".into()),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, rx)
}

/// Every event of this type the client raised, newest first.
pub async fn events(state: &AppState, id: ClientId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

/// The first milter_reply for this stage.
pub async fn wait_for(state: &AppState, id: ClientId, stage: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(r) = events(state, id, "milter_reply")
                .await
                .into_iter()
                .rev()
                .find(|r| r["stage"] == stage)
            {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {stage} reply"))
}

pub async fn send(state: &AppState, id: ClientId, action: Value) -> ClientSendOutcome {
    state
        .send_to_client(id, action, Duration::from_secs(30))
        .await
        .unwrap()
}

pub fn executed(o: ClientSendOutcome) -> Value {
    match o {
        ClientSendOutcome::Executed { detail } => serde_json::from_str(&detail).unwrap(),
        other => panic!("{other:?}"),
    }
}

/// A filter: spam@ recipients rejected, "spammer" senders answered 550, and at end of message a
/// header added, the Subject changed and audit@ added.
const FILTER: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[{'type':'milter_continue'}]
if t=='milter_mail' and 'spammer' in e['sender']: a=[{'type':'milter_reply','code':550,'xcode':'5.7.1','text':'NetGet refuses spammers'}]
elif t=='milter_rcpt' and e['recipient'].startswith('<spam@'): a=[{'type':'milter_reject'}]
elif t=='milter_message':
  subj=[h['value'] for h in e['headers'] if h['name'].lower()=='subject']
  a=[{'type':'milter_add_header','name':'X-NetGet','value':'checked'},{'type':'milter_change_header','name':'Subject','index':1,'value':'[netget] '+subj[0]},{'type':'milter_add_rcpt','recipient':'<audit@example.com>'},{'type':'milter_accept'}]
print(json.dumps({'actions':a}))"#;

async fn netget_filter() -> String {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let sid = ServerForm {
        protocol: "milter".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Filter".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":FILTER}})]),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The server lives as long as the process; its state is leaked deliberately.
    std::mem::forget(state);
    format!("127.0.0.1:{port}")
}

#[tokio::test]
async fn against_netget_filter() {
    let (state, id) = client(netget_filter().await, chain_handlers()).await;
    for stage in ["connect", "helo", "mail", "rcpt"] {
        let r = wait_for(&state, id, stage).await;
        assert_eq!(
            (&r["decision"], &r["implicit"]),
            (&json!("continue"), &json!(false)),
            "{r}"
        );
    }
    let negotiated = events(&state, id, "milter_negotiated").await;
    assert_eq!(negotiated[0]["version"], 6, "{negotiated:?}");
    assert_eq!(
        negotiated[0]["actions"],
        json!([
            "add_header",
            "change_body",
            "add_rcpt",
            "del_rcpt",
            "change_header",
            "quarantine"
        ])
    );
    assert_eq!(negotiated[0]["protocol"], json!([]));
    let m = wait_for(&state, id, "message").await;
    assert_eq!(m["decision"], "accept", "{m}");
    assert_eq!(
        m["modifications"],
        json!([
            {"kind":"add_header","name":"X-NetGet","value":"checked"},
            {"kind":"change_header","index":1,"name":"Subject","value":"[netget] hello"},
            {"kind":"add_rcpt","recipient":"<audit@example.com>"},
        ])
    );
    // A second transaction: a refused recipient, abandoned, then a refused sender.
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_mail","sender":"<alice@example.com>"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "continue", "{r}");
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_rcpt","recipient":"<spam@example.net>"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "reject", "{r}");
    assert!(matches!(
        send(&state, id, json!({"type":"milter_abort"})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_mail","sender":"<spammer@bad.example>"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "replycode", "{r}");
    assert_eq!(r["text"], "550 5.7.1 NetGet refuses spammers");
    // Refused locally, before anything is written.
    for bad in [
        json!({"type":"milter_connect","hostname":"h","address":"not-an-ip"}),
        json!({"type":"milter_helo","name":"two\r\nlines"}),
        json!({"type":"milter_message","headers":"x","body":""}),
    ] {
        match send(&state, id, bad.clone()).await {
            ClientSendOutcome::Rejected { .. } => {}
            other => panic!("{bad}: {other:?}"),
        }
    }
    // Still usable afterwards, and abort is accepted.
    assert!(matches!(
        send(&state, id, json!({"type":"milter_abort"})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_helo","name":"again.example"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "continue");
    assert!(matches!(
        send(&state, id, json!({"type":"disconnect"})).await,
        ClientSendOutcome::Disconnected
    ));
}

#[tokio::test]
async fn followup_chain_is_bounded() {
    // A handler that answers every reply with another HELO: the chain stops after 8 follow-ups.
    let loop_handlers = vec![
        json!({"event_pattern":"*","handler":{"type":"static","actions":[
        {"type":"milter_helo","name":"loop.example"}]}}),
    ];
    let (state, id, mut status) = client_with_status(netget_filter().await, loop_handlers).await;
    // The ninth HELO is the one dropped, and the client says so: every reply before it is logged.
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = status.recv().await {
            if line.contains("handler chain stopped after 8 follow-ups") {
                return;
            }
        }
        panic!("status channel closed");
    })
    .await
    .expect("the chain to be stopped");
    assert_eq!(events(&state, id, "milter_reply").await.len(), 8);
}

/// A hand-driven filter asking for HELO to be left out, MAIL not answered and SKIP allowed:
/// every command it receives, in order.
async fn shortcut_filter() -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let t = Duration::from_secs(30);
        let (cmd, d) = wire::read_packet(&mut s, t).await.unwrap().unwrap();
        assert_eq!(cmd, wire::C_OPTNEG);
        let (_, _, offered) = wire::parse_optneg(&d).unwrap();
        let want = wire::P_NOHELO | wire::P_NR_MAIL | wire::P_SKIP;
        assert_eq!(
            offered & want,
            want,
            "the MTA must offer what this filter asks for"
        );
        wire::write(
            &mut s,
            wire::C_OPTNEG,
            &wire::optneg(6, wire::F_ADDHDRS, want),
        )
        .await
        .unwrap();
        let mut seen = Vec::new();
        while let Ok(Some((cmd, _))) = wire::read_packet(&mut s, t).await {
            seen.push(cmd);
            let reply: &[(u8, &[&str])] = match cmd {
                wire::C_MAIL | wire::C_ABORT => &[],
                wire::C_BODY => &[(wire::R_SKIP, &[])],
                wire::C_BODYEOB => &[
                    (wire::R_ADDHEADER, &["X-Shortcut", "yes"]),
                    (wire::R_ACCEPT, &[]),
                ],
                wire::C_QUIT => break,
                _ => &[(wire::R_CONTINUE, &[])],
            };
            for (code, fields) in reply {
                wire::write(&mut s, *code, &wire::cstrings(fields))
                    .await
                    .unwrap();
            }
        }
        let _ = tx.send(seen);
    });
    (addr, rx)
}

#[tokio::test]
async fn honours_the_filters_protocol_steps() {
    let (addr, seen) = shortcut_filter().await;
    let quiet = vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})];
    let (state, id) = client(addr, quiet).await;
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_connect","hostname":"c.example","address":"192.0.2.1"}),
        )
        .await,
    );
    assert_eq!(
        (&r["decision"], &r["implicit"]),
        (&json!("continue"), &json!(false))
    );
    // Left out and not answered: both continue without the filter saying so.
    for a in [
        json!({"type":"milter_helo","name":"c.example"}),
        json!({"type":"milter_mail","sender":"<a@example.com>"}),
    ] {
        let r = executed(send(&state, id, a).await);
        assert_eq!(
            (&r["decision"], &r["implicit"]),
            (&json!("continue"), &json!(true)),
            "{r}"
        );
    }
    executed(
        send(
            &state,
            id,
            json!({"type":"milter_rcpt","recipient":"<b@example.net>"}),
        )
        .await,
    );
    // A three-chunk body: the filter skips after the first, and end of message follows.
    let body = "x".repeat(150 * 1024);
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_message","headers":[{"name":"Subject","value":"s"}],"body":body}),
        )
        .await,
    );
    assert_eq!(r["decision"], "accept", "{r}");
    assert_eq!(
        r["modifications"],
        json!([{"kind":"add_header","name":"X-Shortcut","value":"yes"}])
    );
    assert!(matches!(
        send(&state, id, json!({"type":"disconnect"})).await,
        ClientSendOutcome::Disconnected
    ));
    let seen = tokio::time::timeout(Duration::from_secs(10), seen)
        .await
        .unwrap()
        .unwrap();
    let got: String = seen.iter().map(|c| *c as char).collect();
    assert_eq!(
        got, "CMRTLNBEQ",
        "no HELO, one body chunk, end of message, then QUIT"
    );
}
