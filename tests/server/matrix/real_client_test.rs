//! NetGet's Matrix homeserver against **matrix-nio** (an independent Python client-server
//! API implementation that validates every answer against its own JSON schemas). Two nio
//! users log in, create a room with an invite, join, talk, and read what the model said
//! back through `/sync`. `NETGET_MATRIX_PYTHON` names a python with matrix-nio installed;
//! the test fails rather than skips without it. No LLM calls: a python policy is the model.
//! Then refusals and bounds over raw HTTP, and a handler failure answered fail-closed.
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[{'type':'matrix_accept'}]
if t=='matrix_room_message':
  b=e['content'].get('body','')
  if b=='forbidden': a=[{'type':'matrix_reject','errcode':'M_FORBIDDEN','error':'not in this room'}]
  elif e['sender'].startswith('@alice'): a=[{'type':'matrix_send','body':'echo: '+b}]
elif t=='matrix_create_room':
  a=[{'type':'matrix_send','body':'welcome to '+(e.get('name') or 'the room')}]
print(json.dumps({'actions':a}))"#;

pub fn python() -> String {
    let p = std::env::var("NETGET_MATRIX_PYTHON").unwrap_or_default();
    assert!(
        !p.is_empty() && std::path::Path::new(&p).exists(),
        "NETGET_MATRIX_PYTHON must name a python with matrix-nio (and matrix-synapse for the \
         client test): python3 -m venv v && v/bin/pip install matrix-nio==0.26.0 matrix-synapse==1.162.0"
    );
    p
}

async fn start(handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "matrix".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a homeserver".into()),
        startup_params: Some(json!({"user_passwords": {"alice": "wonderland", "bob": "builder"}})),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

async fn events(state: &AppState, id: ServerId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

fn policy() -> Option<Vec<Value>> {
    Some(vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ])
}

#[tokio::test]
async fn matrix_nio_users_talk_through_netget() {
    let (state, id, port) = start(policy()).await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/matrix/nio_session.py"
    );
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(python())
            .args([script, &format!("http://127.0.0.1:{port}"), "localhost"])
            .output(),
    )
    .await
    .expect("the nio session did not finish")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let steps: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l:?}\n{text}")))
        .collect();
    let step = |name: &str| {
        steps
            .iter()
            .find(|s| s["step"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name}: {text}"))
    };

    assert_eq!(step("bad_login")["type"], "LoginError", "{text}");
    assert_eq!(step("bad_login")["errcode"], "M_FORBIDDEN", "{text}");
    let login = step("login");
    assert_eq!(login["type"], "LoginResponse", "{text}");
    assert_eq!(login["user_id"], "@alice:localhost");
    let create = step("create");
    assert_eq!(create["type"], "RoomCreateResponse", "{text}");
    let room = create["room_id"].as_str().unwrap().to_string();
    assert_eq!(
        step("invite"),
        json!({"step": "invite", "seen": true, "room_name": "netget-test"}),
        "bob's /sync showed the invite with the room's name"
    );
    assert_eq!(step("join")["room_id"], room);
    assert_eq!(step("send")["type"], "RoomSendResponse", "{text}");
    assert_eq!(step("idempotent")["same"], true, "{text}");
    // What bob read through /sync: the model's greeting (sent when it accepted the room),
    // alice's message, the model's answer to it, and the idempotent send once.
    let seen: Vec<(String, String)> =
        serde_json::from_value(step("bob_saw")["messages"].clone()).unwrap();
    let expected = [
        ("@netget:localhost", "welcome to netget-test"),
        ("@alice:localhost", "hello"),
        ("@netget:localhost", "echo: hello"),
        ("@alice:localhost", "once"),
        ("@netget:localhost", "echo: once"),
    ];
    assert_eq!(
        seen.iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect::<Vec<_>>(),
        expected,
        "{text}"
    );
    let refused = step("refused");
    assert_eq!(refused["type"], "RoomSendError", "{text}");
    assert_eq!(refused["status_code"], "M_FORBIDDEN", "{text}");
    assert_eq!(refused["message"], "not in this room", "{text}");
    assert_eq!(
        step("members")["members"],
        json!(["@alice:localhost", "@bob:localhost", "@netget:localhost"])
    );
    assert_eq!(step("joined_rooms")["rooms"], json!([room]));
    // Newest first, and the refused message is nowhere.
    assert_eq!(
        step("history")["bodies"],
        json!([
            "echo: once",
            "once",
            "echo: hello",
            "hello",
            "welcome to netget-test"
        ])
    );
    assert_eq!(step("whoami")["user_id"], "@alice:localhost");
    assert_eq!(
        step("after_logout")["status_code"],
        "M_UNKNOWN_TOKEN",
        "{text}"
    );

    // What the model was shown.
    let created = &events(&state, id, "matrix_create_room").await[0];
    assert_eq!(
        created,
        &json!({"user_id": "@alice:localhost", "name": "netget-test", "invite": ["@bob:localhost"]})
    );
    let joins = events(&state, id, "matrix_join").await;
    assert_eq!(joins.len(), 1, "{joins:?}");
    assert_eq!(joins[0]["invited"], true);
    let messages = events(&state, id, "matrix_room_message").await;
    assert_eq!(
        messages.len(),
        3,
        "hello, once (asked once though sent twice) and forbidden: {messages:?}"
    );
    let hello = messages
        .iter()
        .find(|m| m["content"]["body"] == "hello")
        .unwrap();
    assert_eq!(hello["sender"], "@alice:localhost");
    assert_eq!(hello["room_name"], "netget-test");
    assert_eq!(hello["event_type"], "m.room.message");
    // Logins were checked against user_passwords, so the model was never asked.
    assert!(events(&state, id, "matrix_login").await.is_empty());
}

async fn http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Vec<u8>>,
) -> (u16, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut r = client.request(
        method.parse().unwrap(),
        format!("http://127.0.0.1:{port}{path}"),
    );
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    if let Some(b) = body {
        r = r.header("Content-Type", "application/json").body(b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn token(port: u16, user: &str, password: &str) -> String {
    let (s, b) = http(
        port,
        "POST",
        "/_matrix/client/v3/login",
        None,
        Some(
            json!({"type":"m.login.password","identifier":{"type":"m.id.user","user":user},"password":password})
                .to_string()
                .into_bytes(),
        ),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    b["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn refusals_bounds_and_long_poll() {
    let (_state, _id, port) = start(policy()).await;
    let (s, b) = http(port, "GET", "/_matrix/client/v3/joined_rooms", None, None).await;
    assert_eq!((s, b["errcode"].as_str()), (401, Some("M_MISSING_TOKEN")));
    let (s, b) = http(
        port,
        "GET",
        "/_matrix/client/v3/joined_rooms",
        Some("ngt_nope"),
        None,
    )
    .await;
    assert_eq!((s, b["errcode"].as_str()), (401, Some("M_UNKNOWN_TOKEN")));
    let t = token(port, "alice", "wonderland").await;
    let (s, b) = http(
        port,
        "POST",
        "/_matrix/client/v3/join/%21nope%3Alocalhost",
        Some(&t),
        Some(b"{}".to_vec()),
    )
    .await;
    assert_eq!((s, b["errcode"].as_str()), (404, Some("M_NOT_FOUND")));
    let (s, b) = http(
        port,
        "POST",
        "/_matrix/client/v3/createRoom",
        Some(&t),
        Some(b"not json".to_vec()),
    )
    .await;
    assert_eq!((s, b["errcode"].as_str()), (400, Some("M_NOT_JSON")));
    // One byte past the body bound.
    let big = vec![b' '; netget::server::matrix::actions::MAX_BODY_BYTES + 1];
    let (s, b) = http(
        port,
        "POST",
        "/_matrix/client/v3/createRoom",
        Some(&t),
        Some(big),
    )
    .await;
    assert_eq!((s, b["errcode"].as_str()), (413, Some("M_TOO_LARGE")));
    let (s, b) = http(port, "GET", "/_matrix/client/v3/nonsense", Some(&t), None).await;
    assert_eq!((s, b["errcode"].as_str()), (404, Some("M_UNRECOGNIZED")));

    // A sync with nothing new waits for its timeout, then answers with the same batch.
    let (_, first) = http(port, "GET", "/_matrix/client/v3/sync", Some(&t), None).await;
    let since = first["next_batch"].as_str().unwrap().to_string();
    let started = std::time::Instant::now();
    let (s, b) = http(
        port,
        "GET",
        &format!("/_matrix/client/v3/sync?since={since}&timeout=700"),
        Some(&t),
        None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(b["next_batch"], since.as_str());
    assert!(
        started.elapsed() >= Duration::from_millis(650),
        "{:?}",
        started.elapsed()
    );
    // And a sync that is waiting is woken by a new event, well before its timeout.
    let waiter = {
        let t = t.clone();
        let since = since.clone();
        tokio::spawn(async move {
            http(
                port,
                "GET",
                &format!("/_matrix/client/v3/sync?since={since}&timeout=20000"),
                Some(&t),
                None,
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    let (s, created) = http(
        port,
        "POST",
        "/_matrix/client/v3/createRoom",
        Some(&t),
        Some(br#"{"name":"woken"}"#.to_vec()),
    )
    .await;
    assert_eq!(s, 200, "{created}");
    let (s, woke) = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s, 200);
    assert!(started.elapsed() < Duration::from_secs(10));
    let room = created["room_id"].as_str().unwrap();
    let timeline = &woke["rooms"]["join"][room]["timeline"]["events"];
    assert_eq!(timeline[0]["type"], "m.room.create", "{woke}");
}

#[tokio::test]
async fn a_failed_handler_refuses_and_delivers_nothing() {
    // No handlers and an unreachable model: every decision fails.
    let (_state, _id, port) = start(None).await;
    let t = token(port, "alice", "wonderland").await;
    let (s, b) = http(
        port,
        "POST",
        "/_matrix/client/v3/createRoom",
        Some(&t),
        Some(br#"{"name":"x"}"#.to_vec()),
    )
    .await;
    assert!(s == 500 || s == 503, "{s} {b}");
    assert!(
        matches!(
            b["errcode"].as_str(),
            Some("M_UNKNOWN" | "M_LIMIT_EXCEEDED")
        ),
        "{b}"
    );
    let error = b["error"].as_str().unwrap();
    assert!(
        !error.contains("127.0.0.1") && !error.to_lowercase().contains("ollama"),
        "the peer gets a category, not the error: {error}"
    );
    let (_, rooms) = http(
        port,
        "GET",
        "/_matrix/client/v3/joined_rooms",
        Some(&t),
        None,
    )
    .await;
    assert_eq!(rooms["joined_rooms"], json!([]), "no room was created");
}
