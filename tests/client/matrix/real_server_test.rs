//! NetGet's Matrix client against **Synapse**, the reference homeserver, with a
//! **matrix-nio** user (bob) on the other side reading what NetGet said. Synapse is started
//! from `NETGET_MATRIX_PYTHON` (a python with matrix-synapse and matrix-nio); the test fails
//! rather than skips without it. No LLM calls: a python chain is the model.
//!
//! The chain: on login NetGet creates a room inviting bob and says hello in it; when bob
//! joins it welcomes him; when he says "ping" it answers "pong". Then the operator injects a
//! message, a send into a room NetGet is not in (Synapse's refusal comes back as an event),
//! and a history read that returns Synapse's own record of the conversation.
use crate::helpers::real_server::{InstallHint, RealServer};
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

const HINT: InstallHint = InstallHint {
    brew: "python@3.13 (then pip install matrix-synapse==1.162.0 matrix-nio==0.26.0 in a venv)",
    apt: "python3-venv (then pip install matrix-synapse==1.162.0 matrix-nio==0.26.0 in a venv)",
};

const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='matrix_connected':
  a=[{'type':'matrix_create_room','name':'netget-room','invite':['@bob:localhost']}]
elif t=='matrix_response' and e['operation']=='matrix_create_room' and e['status']==200:
  a=[{'type':'matrix_send','room_id':e['result']['room_id'],'body':'hello bob'}]
elif t=='matrix_member' and e['membership']=='join':
  a=[{'type':'matrix_send','room_id':e['room_id'],'body':'welcome, '+e['user_id']}]
elif t=='matrix_message' and e['content'].get('body')=='ping':
  a=[{'type':'matrix_send','room_id':e['room_id'],'body':'pong'}]
print(json.dumps({'actions':a}))"#;

fn python() -> String {
    let p = std::env::var("NETGET_MATRIX_PYTHON").unwrap_or_default();
    assert!(
        !p.is_empty() && std::path::Path::new(&p).exists(),
        "NETGET_MATRIX_PYTHON must name a python with matrix-synapse and matrix-nio: \
         python3 -m venv v && v/bin/pip install matrix-synapse==1.162.0 matrix-nio==0.26.0"
    );
    p
}

async fn synapse(python: &str) -> RealServer {
    let conf = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/client/matrix/synapse_conf.py"
    );
    let server = RealServer::builder(python, HINT)
        .setup_command(python, HINT, [conf, "{dir}", "{port}"])
        .args([
            "-m",
            "synapse.app.homeserver",
            "-c",
            "{dir}/homeserver.yaml",
        ])
        .ready_when_log_matches("Synapse now listening on TCP port")
        .startup_timeout(Duration::from_secs(90))
        .start()
        .await
        .unwrap();
    let register = PathBuf::from(python)
        .parent()
        .unwrap()
        .join("register_new_matrix_user");
    for (user, password) in [("alice", "wonderland"), ("bob", "builder")] {
        let out = tokio::process::Command::new(&register)
            .args(["-u", user, "-p", password, "--no-admin", "-c"])
            .arg(server.dir().join("homeserver.yaml"))
            .arg(format!("http://{}", server.addr()))
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "register {user}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    server
}

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

#[tokio::test]
async fn netget_client_on_synapse_talks_to_a_nio_user() {
    let python = python();
    let server = synapse(&python).await;
    let url = format!("http://{}", server.addr());
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/client/matrix/nio_bob.py"
    );
    let mut bob = tokio::process::Command::new(&python)
        .args([script, &url, "builder"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(bob.stdout.take().unwrap()).lines();
    let mut seen: Vec<Value> = Vec::new();
    let mut next_step = async |name: &str, seen: &mut Vec<Value>| -> Value {
        tokio::time::timeout(Duration::from_secs(90), async {
            while let Ok(Some(l)) = lines.next_line().await {
                let v: Value = serde_json::from_str(&l).unwrap_or_else(|e| panic!("{e}: {l}"));
                seen.push(v.clone());
                if v["step"] == name {
                    return v;
                }
            }
            panic!("bob ended before {name}: {seen:?}");
        })
        .await
        .unwrap_or_else(|_| panic!("bob never reached {name}: {seen:?}\n{}", server.log()))
    };
    assert_eq!(
        next_step("ready", &mut seen).await["user_id"],
        "@bob:localhost"
    );

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "matrix".into(),
        remote_addr: Some(server.addr()),
        instruction: Some("Invite bob and talk to him".into()),
        startup_params: Some(json!({"user": "alice", "password": "wonderland"})),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");

    let invited = next_step("invited", &mut seen).await;
    assert_eq!(invited["inviter"], "@alice:localhost", "{invited}");
    let room = invited["room_id"].as_str().unwrap().to_string();
    assert_eq!(next_step("joined", &mut seen).await["type"], "JoinResponse");
    assert_eq!(
        next_step("welcomed", &mut seen).await["ok"],
        true,
        "{seen:?}"
    );
    assert_eq!(next_step("ponged", &mut seen).await["ok"], true, "{seen:?}");

    // The operator speaks through the running client.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"matrix_send","room_id":room,"body":"from the operator"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Executed { detail } if detail.contains("\"status\":200")),
        "{outcome:?}"
    );
    let done = next_step("operator", &mut seen).await;
    assert_eq!(done["ok"], true, "{done}");
    // What bob read from Synapse, in order, all from alice.
    let said: Vec<(String, String)> = serde_json::from_value(done["seen"].clone()).unwrap();
    let from_alice: Vec<&str> = said
        .iter()
        .filter(|(s, _)| s == "@alice:localhost")
        .map(|(_, b)| b.as_str())
        .collect();
    assert_eq!(
        from_alice,
        [
            "hello bob",
            "welcome, @bob:localhost",
            "pong",
            "from the operator"
        ],
        "{said:?}"
    );

    // What NetGet saw of bob: his join and his ping, from Synapse's /sync.
    let ping = wait_event(&state, id, |e| {
        e["event_type"] == "matrix_message" && e["request"]["content"]["body"] == "ping"
    })
    .await;
    assert_eq!(ping["sender"], "@bob:localhost");
    assert_eq!(ping["room_id"], room.as_str());
    let joined = wait_event(&state, id, |e| e["event_type"] == "matrix_member").await;
    assert_eq!(
        (joined["user_id"].as_str(), joined["membership"].as_str()),
        (Some("@bob:localhost"), Some("join"))
    );

    // Synapse's own history of the room, newest first.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"matrix_messages","room_id":room,"limit":20}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}")
    };
    let detail: Value = serde_json::from_str(&detail).unwrap();
    let bodies: Vec<&str> = detail["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert_eq!(
        &bodies[..5],
        [
            "from the operator",
            "pong",
            "ping",
            "welcome, @bob:localhost",
            "hello bob"
        ],
        "{detail}"
    );

    // A room NetGet is not in: Synapse refuses, and the refusal reaches the handler.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"matrix_send","room_id":"!nope:localhost","body":"x"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Executed { detail } if detail.contains("\"errcode\":\"M_")),
        "{outcome:?}"
    );
    let refusal = wait_event(&state, id, |e| {
        e["event_type"] == "matrix_response" && e["request"]["errcode"].is_string()
    })
    .await;
    assert_ne!(refusal["status"], 200, "{refusal}");

    // Invalid before anything is sent.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"matrix_send","room_id":"not-a-room","body":"x"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("room id")),
        "{outcome:?}"
    );
    drop(server);
}

#[tokio::test]
async fn a_wrong_password_fails_the_connect() {
    let python = python();
    let server = synapse(&python).await;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let err = ClientForm {
        protocol: "matrix".into(),
        remote_addr: Some(server.addr()),
        instruction: Some("x".into()),
        startup_params: Some(json!({"user": "alice", "password": "not-it"})),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await;
    let text = format!("{err:?}");
    assert!(
        text.contains("M_FORBIDDEN") && !text.contains("not-it"),
        "the refusal is named and the password is not: {text}"
    );
}
