//! NetGet's portmapper client against the system **rpcbind** (libtirpc) on 127.0.0.1:111,
//! read back with **rpcinfo**. Fails rather than skips when rpcbind is not running. No LLM
//! calls: a python chain is the model.
//!
//! The chain: dump; register a program in the user range (0x20000000+) with PMAP v2 SET;
//! look it up with PMAP v2 GETPORT and RPCBIND v4 GETADDR. rpcinfo then shows the
//! registration rpcbind holds; injected unsets remove it, and rpcinfo shows that too.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
P=PROGRAM
a=[]
if t=='sunrpc_connected':
  a=[{'type':'sunrpc_dump','version':4}]
elif t=='sunrpc_reply' and e['ok']:
  op=e['operation']
  if op=='sunrpc_dump':
    a=[{'type':'sunrpc_set','program':P,'program_version':1,'protocol':'tcp','port':4242}]
  elif op=='sunrpc_set':
    a=[{'type':'sunrpc_getport','program':P,'program_version':1,'protocol':'tcp'}]
  elif op=='sunrpc_getport':
    a=[{'type':'sunrpc_getaddr','program':P,'program_version':1,'protocol':'tcp','version':4}]
print(json.dumps({'actions':a}))"#;

async fn wait_event(state: &AppState, id: ClientId, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let hit = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .find(|e| pred(e))
                .map(|e| e["request"].clone());
            if let Some(e) = hit {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no matching event")
}

fn reply(state_events: &Value, op: &str) -> bool {
    state_events["event_type"] == "sunrpc_reply" && state_events["request"]["operation"] == op
}

async fn rpcinfo(args: &[&str]) -> String {
    let out = tokio::process::Command::new("rpcinfo")
        .args(args)
        .output()
        .await
        .expect("rpcinfo (apt-get install rpcbind)");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[tokio::test]
async fn netget_registers_with_rpcbind() {
    assert!(
        std::net::TcpStream::connect("127.0.0.1:111").is_ok(),
        "rpcbind must be running on 127.0.0.1:111: apt-get install rpcbind (or brew install rpcbind) and start it"
    );
    // A program number of this run's own, so concurrent runs do not collide.
    let program = 0x2000_0000u32 + 0x4e00 + std::process::id() % 0x100;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "sunrpc".into(),
        remote_addr: Some("127.0.0.1:111".into()),
        instruction: Some("Register and look up".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python",
            "code": CHAIN.replace("PROGRAM", &program.to_string())}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect("connect");

    let dump = wait_event(&state, id, |e| reply(e, "sunrpc_dump")).await;
    let listed = dump["result"].as_array().unwrap();
    assert!(
        listed.iter().any(|m| m["program"] == 100_000
            && m["version"] == 4
            && m["netid"] == "tcp"
            && m["port"] == 111
            && m["program_name"] == "portmapper"),
        "rpcbind lists itself: {dump}"
    );
    let set = wait_event(&state, id, |e| reply(e, "sunrpc_set")).await;
    assert_eq!(set["result"], true, "{set}");
    let port = wait_event(&state, id, |e| reply(e, "sunrpc_getport")).await;
    assert_eq!(port["result"], 4242, "{port}");
    let addr = wait_event(&state, id, |e| reply(e, "sunrpc_getaddr")).await;
    assert_eq!(addr["result"]["port"], 4242, "{addr}");
    assert!(
        addr["result"]["address"]
            .as_str()
            .unwrap()
            .ends_with(".16.146"),
        "4242 is 16.146 in a universal address: {addr}"
    );

    // rpcbind's own record of it.
    let text = rpcinfo(&["-p", "127.0.0.1"]).await;
    let row = format!("{program}    1   tcp   4242");
    assert!(
        text.lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>()
                == row.split_whitespace().collect::<Vec<_>>()),
        "{text}"
    );

    // Injected: a v4 registration on udp, then both removed.
    let send = |action: Value| {
        let state = state.clone();
        async move {
            state
                .send_to_client(id, action, Duration::from_secs(10))
                .await
                .unwrap()
        }
    };
    let executed = |o: &ClientSendOutcome| -> Value {
        match o {
            ClientSendOutcome::Executed { detail } => serde_json::from_str(detail).unwrap(),
            other => panic!("{other:?}"),
        }
    };
    let v4 = executed(
        &send(json!({"type":"sunrpc_set","version":4,"program":program,"program_version":2,"protocol":"udp","port":4343}))
            .await,
    );
    assert_eq!(v4["result"], true, "{v4}");
    let text = rpcinfo(&["-s", "127.0.0.1"]).await;
    let mine = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some(&program.to_string()))
        .unwrap_or_else(|| panic!("{text}"));
    let cols: Vec<&str> = mine.split_whitespace().collect();
    assert_eq!(cols[1], "2,1", "{text}");
    assert_eq!(cols[2], "udp,tcp", "{text}");

    let gone = executed(
        &send(
            json!({"type":"sunrpc_unset","program":program,"program_version":1,"protocol":"tcp"}),
        )
        .await,
    );
    assert_eq!(gone["result"], true, "{gone}");
    let gone = executed(
        &send(json!({"type":"sunrpc_unset","version":4,"program":program,"program_version":2,"protocol":""}))
            .await,
    );
    assert_eq!(gone["result"], true, "{gone}");
    let text = rpcinfo(&["-p", "127.0.0.1"]).await;
    assert!(
        !text.contains(&program.to_string()),
        "both registrations removed: {text}"
    );
    let none = executed(
        &send(json!({"type":"sunrpc_getport","program":program,"program_version":1})).await,
    );
    assert_eq!(none["result"], 0, "{none}");

    // Refused before anything is sent.
    let bad = send(json!({"type":"sunrpc_dump","version":5})).await;
    assert!(
        matches!(&bad, ClientSendOutcome::Rejected { error } if error.contains("version must be 2, 3 or 4")),
        "{bad:?}"
    );
}
