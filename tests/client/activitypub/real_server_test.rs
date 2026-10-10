//! NetGet's ActivityPub client against an actor built on the **Fedify** library
//! (`tests/server/activitypub/peer/peer.mjs serve`, Fedify 2.4.2 from npm). NetGet resolves
//! the peer, follows it with a signed Follow that Fedify verifies before its inbox sees it,
//! hears Fedify's signed Accept on its own inbox (NetGet verifies it), and answers it by
//! posting a note Fedify receives, fetches and parses. Then a WebFinger lookup Fedify serves,
//! and an action refused before it reaches the network. `install_peers.py` prints
//! `NETGET_FEDIFY_PEER`; the test fails rather than skips without it. No LLM calls: a python
//! policy is the model.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

/// Ready → follow whoever the client was opened at; Accept → post them a note.
const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='activitypub_ready': a=[{'type':'activitypub_follow','target':e['remote']}]
elif t=='activitypub_activity' and e['type']=='Accept': a=[{'type':'activitypub_post','content':'hi peer','to':[e['actor']],'public':False}]
print(json.dumps({'actions':a}))"#;

fn peer_script() -> String {
    let v = std::env::var("NETGET_FEDIFY_PEER").unwrap_or_default();
    assert!(
        !v.is_empty(),
        "NETGET_FEDIFY_PEER is required: python3 tests/server/activitypub/install_peers.py <dir> and export what it prints"
    );
    v
}

async fn wait_event(state: &AppState, id: ClientId, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(60), async {
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

#[tokio::test]
async fn netget_follows_and_posts_to_a_fedify_actor() {
    let mut child = tokio::process::Command::new("node")
        .arg(peer_script())
        .arg("serve")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("node");
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seen: Vec<Value> = Vec::new();
    let mut next = async |pred: &dyn Fn(&Value) -> bool, seen: &mut Vec<Value>| -> Value {
        if let Some(v) = seen.iter().find(|v| pred(v)) {
            return v.clone();
        }
        tokio::time::timeout(Duration::from_secs(60), async {
            while let Ok(Some(l)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&l) else {
                    continue;
                };
                seen.push(v.clone());
                if pred(&v) {
                    return v;
                }
            }
            panic!("the peer ended: {seen:?}");
        })
        .await
        .unwrap_or_else(|_| panic!("the peer never printed it: {seen:?}"))
    };
    let peer = next(&|v| v["actor"].is_string(), &mut seen).await["actor"]
        .as_str()
        .unwrap()
        .to_string();
    let host = reqwest::Url::parse(&peer).unwrap();
    let handle = format!("peer@127.0.0.1:{}", host.port().unwrap());

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "activitypub".into(),
        remote_addr: Some(peer.clone()),
        instruction: Some("Follow the peer and say hi once it accepts".into()),
        startup_params: Some(json!({"username": "bot"})),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");

    // Fedify's inbox only hands on what verified; the Follow names NetGet's actor.
    let follow = next(&|v| v["received"] == "Follow", &mut seen).await;
    let me = follow["actor"].as_str().unwrap().to_string();
    assert!(me.ends_with("/users/bot"), "{follow}");
    assert_eq!(
        next(&|v| v["sent"] == "Accept", &mut seen).await["to"],
        me.as_str()
    );

    // NetGet verified Fedify's signed Accept, and the handler answered it with a note.
    let accept = wait_event(&state, id, |e| {
        e["event_type"] == "activitypub_activity" && e["request"]["type"] == "Accept"
    })
    .await;
    assert_eq!(accept["actor"], peer.as_str(), "{accept}");
    let create = next(&|v| v["received"] == "Create", &mut seen).await;
    assert_eq!(create["actor"], me.as_str(), "{create}");
    assert_eq!(create["object_type"], "Note", "{create}");
    assert!(
        create["content"]
            .as_str()
            .unwrap_or_default()
            .contains("hi peer"),
        "{create}"
    );
    assert_eq!(create["to"], json!([peer]), "{create}");
    let posted = wait_event(&state, id, |e| {
        e["event_type"] == "activitypub_response" && e["request"]["operation"] == "activitypub_post"
    })
    .await;
    assert_eq!(posted["ok"], true, "{posted}");
    assert_eq!(
        posted["result"]["note_id"], create["object_id"],
        "{posted} vs {create}"
    );

    // The operator looks the peer up by handle: WebFinger and the actor document, from Fedify.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"activitypub_lookup","target":handle}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}");
    };
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["ok"], true, "{detail}");
    assert_eq!(detail["result"]["id"], peer.as_str(), "{detail}");
    assert_eq!(detail["result"]["preferred_username"], "peer", "{detail}");
    assert_eq!(detail["result"]["name"], "Fedify peer", "{detail}");

    // Refused locally: a client answers no Follow requests.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"activitypub_accept"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("answers no Follow")),
        "{outcome:?}"
    );
    drop(child.stdin.take());
}
