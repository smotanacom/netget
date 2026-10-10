//! NetGet's ActivityPub instance against **Fedify** (2.4.2, from npm): its CLI resolves and
//! parses NetGet's actor (WebFinger, and a JSON-LD round trip through `lookup -C`), and a
//! peer built on the Fedify library (`peer/peer.mjs`) follows NetGet's actor. Fedify signs
//! the Follow, NetGet verifies it, the model accepts and posts, and Fedify verifies NetGet's
//! signed Accept and Create before its inbox hands them on. `install_peers.py` prints
//! `NETGET_FEDIFY` and `NETGET_FEDIFY_PEER`; the tests fail rather than skip without them.
//! No LLM calls: a python policy is the model. Then refused signatures and bounds over raw
//! HTTP, and a failed handler.
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
a=[]
if e['type']=='Follow':
  a=[{'type':'activitypub_accept'},
     {'type':'activitypub_post','content':'hello '+e.get('actor_handle','follower')+'\nsecond line','public':True}]
print(json.dumps({'actions':a}))"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/activitypub/install_peers.py <dir> and export what it prints"
    );
    v
}

async fn start(handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "activitypub".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a small fediverse instance".into()),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

async fn events(state: &AppState, id: ServerId) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == "activitypub_activity")
        .map(|e| e["request"].clone())
        .collect()
}

async fn fedify(args: &[&str]) -> String {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(env("NETGET_FEDIFY"))
            .args(args)
            .output(),
    )
    .await
    .expect("fedify did not finish")
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    text
}

/// The Fedify peer in `mode`; its stdin (closing it ends the peer) and its lines.
async fn peer(
    args: &[&str],
) -> (
    tokio::process::Child,
    tokio::sync::mpsc::UnboundedReceiver<Value>,
) {
    let mut child = tokio::process::Command::new("node")
        .arg(env("NETGET_FEDIFY_PEER"))
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("node");
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(l)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(&l) {
                let _ = tx.send(v);
            }
        }
    });
    (child, rx)
}

async fn next_matching(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    seen: &mut Vec<Value>,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    if let Some(v) = seen.iter().find(|v| pred(v)) {
        return v.clone();
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let v = rx.recv().await.expect("the peer ended");
            seen.push(v.clone());
            if pred(&v) {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the peer never printed it: {seen:?}"))
}

fn policy() -> Option<Vec<Value>> {
    Some(vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ])
}

#[tokio::test]
async fn fedify_reads_and_follows_netget() {
    let (state, id, port) = start(policy()).await;
    let actor = format!("http://127.0.0.1:{port}/users/netget");

    let text = fedify(&["webfinger", "-p", &actor]).await;
    assert!(
        text.contains("WebFinger found") && text.contains(&actor),
        "{text}"
    );
    // lookup -C expands and re-compacts NetGet's actor through Fedify's JSON-LD processor.
    let text = fedify(&["lookup", "-p", "-C", &actor]).await;
    let json_start = text.find('{').unwrap_or_else(|| panic!("{text}"));
    let json_end = text.rfind('}').unwrap();
    let doc: Value = serde_json::from_str(&text[json_start..=json_end])
        .unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(doc["type"], "Person", "{doc}");
    assert_eq!(doc["preferredUsername"], "netget");
    assert_eq!(doc["publicKey"]["owner"], actor.as_str());
    assert!(doc["publicKey"]["publicKeyPem"]
        .as_str()
        .unwrap()
        .starts_with("-----BEGIN PUBLIC KEY-----"));

    // The Fedify peer follows NetGet's actor.
    let (mut child, mut rx) = peer(&["follow", &actor]).await;
    let mut seen = Vec::new();
    let me = next_matching(&mut rx, &mut seen, |v| {
        v["actor"].is_string() && v.get("received").is_none()
    })
    .await;
    let peer_actor = me["actor"].as_str().unwrap().to_string();
    let looked = next_matching(&mut rx, &mut seen, |v| v["looked_up"].is_string()).await;
    assert_eq!(looked["inbox"], format!("{actor}/inbox"), "{looked}");
    // Fedify verified NetGet's signature on each before its listener saw them.
    let accept = next_matching(&mut rx, &mut seen, |v| v["received"] == "Accept").await;
    assert_eq!(accept["actor"], actor.as_str(), "{accept}");
    assert_eq!(accept["object_type"], "Follow", "{accept}");
    assert_eq!(accept["follow_object"], actor.as_str(), "{accept}");
    let create = next_matching(&mut rx, &mut seen, |v| v["received"] == "Create").await;
    assert_eq!(create["object_type"], "Note", "{create}");
    assert_eq!(
        create["content"],
        format!(
            "<p>hello peer@127.0.0.1:{}</p><p>second line</p>",
            peer_actor
                .split(':')
                .nth(2)
                .unwrap()
                .split('/')
                .next()
                .unwrap()
        ),
        "{create}"
    );
    assert!(
        create["to"]
            .as_array()
            .unwrap()
            .contains(&json!("https://www.w3.org/ns/activitystreams#Public")),
        "{create}"
    );

    // NetGet's side: what the model was shown, and the follower it now lists.
    let follow = &events(&state, id).await[0];
    assert_eq!(follow["type"], "Follow");
    assert_eq!(follow["actor"], peer_actor.as_str());
    assert_eq!(follow["object_id"], actor.as_str());
    assert_eq!(follow["to_actor"], "netget");
    assert!(
        follow["actor_handle"]
            .as_str()
            .unwrap()
            .starts_with("peer@127.0.0.1:"),
        "{follow}"
    );
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let followers: Value = client
        .get(format!("{actor}/followers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        followers["orderedItems"],
        json!([peer_actor]),
        "{followers}"
    );
    let outbox: Value = client
        .get(format!("{actor}/outbox"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(outbox["totalItems"], 1);
    let note_id = create["object_id"].as_str().unwrap();
    let note: Value = client
        .get(note_id)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(note["attributedTo"], actor.as_str(), "{note}");
    drop(child.stdin.take());
}

#[tokio::test]
async fn unsigned_and_forged_requests_are_refused() {
    let (state, id, port) = start(policy()).await;
    let inbox = format!("http://127.0.0.1:{port}/users/netget/inbox");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let follow = json!({"@context": "https://www.w3.org/ns/activitystreams", "type": "Follow",
                        "id": "http://127.0.0.1:1/f", "actor": "http://127.0.0.1:1/users/x",
                        "object": format!("http://127.0.0.1:{port}/users/netget")});
    let r = client.post(&inbox).json(&follow).send().await.unwrap();
    assert_eq!(r.status(), 401, "unsigned");
    assert!(r.text().await.unwrap().contains("HTTP Signature"));

    // A real key, served by the Fedify peer, over a request whose signature is not its own.
    let (mut child, mut rx) = peer(&["serve"]).await;
    let mut seen = Vec::new();
    let me = next_matching(&mut rx, &mut seen, |v| v["actor"].is_string()).await;
    let peer_actor = me["actor"].as_str().unwrap();
    let mut forged = follow.clone();
    forged["actor"] = json!(peer_actor);
    let body = serde_json::to_vec(&forged).unwrap();
    let digest = netget::server::activitypub::sig::digest(&body);
    let date = netget::server::activitypub::sig::http_date();
    let sig = format!(
        "keyId=\"{peer_actor}#main-key\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date digest\",signature=\"{}\"",
        "AAAA".repeat(86)
    );
    let r = client
        .post(&inbox)
        .header("date", &date)
        .header("digest", &digest)
        .header("signature", &sig)
        .header("content-type", "application/activity+json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "a signature the key did not make");
    // A digest that is not the body's.
    let r = client
        .post(&inbox)
        .header("date", &date)
        .header(
            "digest",
            netget::server::activitypub::sig::digest(b"something else"),
        )
        .header("signature", &sig)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.text().await.unwrap().contains("Digest"));
    // One byte past the bound.
    let big = vec![b' '; netget::server::activitypub::instance::MAX_DOCUMENT + 1];
    let r = client.post(&inbox).body(big).send().await.unwrap();
    assert_eq!(r.status(), 413);
    let r = client
        .get(format!(
            "http://127.0.0.1:{port}/.well-known/webfinger?resource=acct:nobody@127.0.0.1:{port}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(
        events(&state, id).await.is_empty(),
        "nothing unverified reached the model"
    );
    drop(child.stdin.take());
}

#[tokio::test]
async fn a_failed_handler_accepts_nothing() {
    let (state, id, port) = start(None).await;
    let actor = format!("http://127.0.0.1:{port}/users/netget");
    let (mut child, mut rx) = peer(&["follow", &actor]).await;
    let mut seen = Vec::new();
    next_matching(&mut rx, &mut seen, |v| v["sent"] == "Follow").await;
    // The Follow was verified and offered to the model, which failed: nothing came back.
    tokio::time::timeout(Duration::from_secs(20), async {
        while events(&state, id).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let late = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
    assert!(late.is_err(), "no Accept without a decision: {late:?}");
    let followers: Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{actor}/followers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(followers["totalItems"], 0);
    drop(child.stdin.take());
}
