//! NetGet's BFD client against **BIRD 2** (`apt-get install bird2`), run unprivileged from a
//! config in a temp dir with a static multihop session to 127.0.0.4, where NetGet's client
//! listens. Fails rather than skips without it. The session is read back from BIRD's side
//! with `birdc`.
//!
//! The model's part: once the session is Up, a python handler asks for a 400 ms receive
//! interval. BIRD's transmit interval becoming 0.400 is that answer, negotiated with a Poll
//! Sequence, as BIRD reports it. No LLM calls.
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const BIRD: InstallHint = InstallHint {
    brew: "bird",
    apt: "bird2",
};
/// Every test here holds UDP 4784 on loopback.
static PORT_4784: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const ME: &str = "127.0.0.4";

async fn bird(auth: &str) -> RealServer {
    let conf = format!(
        r#"log stderr all;
router id 127.0.0.1;
protocol bfd {{
  multihop {{
    min rx interval 100 ms;
    min tx interval 100 ms;
    idle tx interval 1000 ms;
    multiplier 3;
    {auth}
  }};
  neighbor {ME} local 127.0.0.1 multihop on;
}}
"#
    );
    RealServer::builder("bird", BIRD)
        .config_file("bird.conf", &conf)
        .args([
            "-f",
            "-c",
            "{dir}/bird.conf",
            "-s",
            "{dir}/ctl",
            "-P",
            "{dir}/pid",
        ])
        .without_tcp_readiness()
        .ready_when_log_matches("Started")
        .startup_timeout(Duration::from_secs(20))
        .start()
        .await
        .expect("start bird")
}

async fn bird_session(bird: &RealServer) -> Vec<String> {
    let birdc = find_binary("birdc").expect("birdc is required: apt-get install bird2");
    let out = tokio::process::Command::new(birdc)
        .args(["-s", &bird.dir().join("ctl").display().to_string()])
        .args(["show", "bfd", "sessions"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with(ME))
        .map(|l| l.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

async fn wait_bird(bird: &RealServer, what: &str, pred: impl Fn(&[String]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let line = bird_session(bird).await;
        if pred(&line) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "BIRD's session never became {what}: {line:?}\n{}",
            bird.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn state_is(l: &[String], s: &str) -> bool {
    l.get(2).map(String::as_str) == Some(s)
}

async fn client(params: Value, handlers: Vec<Value>) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "bfd".into(),
        remote_addr: Some("127.0.0.1:4784".into()),
        instruction: Some("Keep the session up".into()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect("connect");
    (state, id)
}

async fn events(state: &AppState, id: ClientId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

const ON_UP: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
print(json.dumps({'actions':[{'type':'bfd_set_timers','desired_min_tx_ms':100,'required_min_rx_ms':400,'detect_mult':3}] if e['state']=='Up' and e['required_min_rx_ms']==100 else []}))"#;

#[tokio::test]
async fn netget_client_session_with_bird() {
    let _port = PORT_4784.lock().await;
    let bird = bird("").await;
    let (state, id) = client(
        json!({"local_address": ME, "desired_min_tx_ms": 100, "required_min_rx_ms": 100, "detect_mult": 3}),
        vec![
            json!({"event_pattern":"bfd_session_state","handler":{"type":"script","language":"python","code":ON_UP}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ],
    )
    .await;

    wait_bird(&bird, "Up at 0.400", |l| {
        state_is(l, "Up") && l.get(4).map(String::as_str) == Some("0.400")
    })
    .await;
    let started = &events(&state, id, "bfd_session_started").await[0];
    assert_eq!(started["peer"], "127.0.0.1:4784", "{started}");
    assert_eq!(started["multihop"], true, "{started}");
    let states = events(&state, id, "bfd_session_state").await;
    assert!(states.iter().any(|e| e["state"] == "Up"), "{states:?}");

    // Injected: AdminDown (BIRD sees its neighbour go down), back up, and a refused action.
    let down = state
        .send_to_client(
            id,
            json!({"type":"bfd_admin_down","diag":"path_down"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = down else {
        panic!("{down:?}")
    };
    assert!(detail.contains("\"state\":\"AdminDown\""), "{detail}");
    wait_bird(&bird, "Down", |l| state_is(l, "Down")).await;
    state
        .send_to_client(id, json!({"type":"bfd_admin_up"}), Duration::from_secs(5))
        .await
        .unwrap();
    wait_bird(&bird, "Up again", |l| state_is(l, "Up")).await;
    let refused = state
        .send_to_client(
            id,
            json!({"type":"bfd_admin_down","diag":"nonsense"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(refused, ClientSendOutcome::Rejected { .. }),
        "{refused:?}"
    );

    // Disconnect: the client stops sending and BIRD's detection timer brings the session down.
    let gone = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(gone, ClientSendOutcome::Disconnected), "{gone:?}");
    wait_bird(&bird, "Down after disconnect", |l| state_is(l, "Down")).await;
}

#[tokio::test]
async fn keyed_md5_with_bird() {
    let _port = PORT_4784.lock().await;
    let bird =
        bird("authentication meticulous keyed md5;\n    password \"md5-key\" { id 7; };").await;
    let (_state, _id) = client(
        json!({"local_address": ME, "auth_type": "meticulous_keyed_md5", "auth_key_id": 7, "auth_password": "md5-key"}),
        vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})],
    )
    .await;
    wait_bird(&bird, "Up", |l| state_is(l, "Up")).await;
}
