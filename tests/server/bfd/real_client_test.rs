//! NetGet's BFD speaker against **BIRD 2** (`apt-get install bird2`), run unprivileged from a
//! config in a temp dir with a static multihop session to NetGet. Fails rather than skips
//! without it. What the session is doing is read back from BIRD's side with `birdc`.
//!
//! NetGet listens on 127.0.0.2:4784, beside BIRD's wildcard 4784 (both SO_REUSEADDR), so
//! BIRD's packets to 127.0.0.2 reach NetGet and NetGet's to 127.0.0.1 reach BIRD. A python
//! policy accepts 127.0.0.1 with 100 ms timers and, the first time the session is Up, asks for
//! a 400 ms receive interval: BIRD's transmit interval becoming 0.400 is the Poll Sequence
//! completing, and the model's answer read off the peer. No LLM calls.
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

pub const BIRD: InstallHint = InstallHint {
    brew: "bird",
    apt: "bird2",
};

/// Every test here holds UDP 4784 on loopback for its duration.
pub static PORT_4784: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub const KEY: &str = "netget-bfd-key";

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='bfd_session_request' and e['peer']=='127.0.0.1':
  a=[{'type':'bfd_accept_session','desired_min_tx_ms':100,'required_min_rx_ms':100,'detect_mult':3}]
elif t=='bfd_session_state' and e['state']=='Up' and e['required_min_rx_ms']==100:
  a=[{'type':'bfd_set_timers','desired_min_tx_ms':100,'required_min_rx_ms':400,'detect_mult':3}]
print(json.dumps({'actions':a}))"#;

/// BIRD with one multihop session to `neighbor`, from 127.0.0.1; `auth` is BIRD's
/// authentication lines for the session.
pub async fn bird(neighbor: &str, auth: &str) -> RealServer {
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
  neighbor {neighbor} local 127.0.0.1 multihop on;
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

/// BIRD's own line for its session with `neighbor`: [ip, interface, state, since, interval,
/// timeout].
pub async fn bird_session(bird: &RealServer, neighbor: &str) -> Vec<String> {
    let birdc = find_binary("birdc").expect("birdc is required: apt-get install bird2");
    let out = tokio::process::Command::new(birdc)
        .args(["-s", &bird.dir().join("ctl").display().to_string()])
        .args(["show", "bfd", "sessions"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with(neighbor))
        .map(|l| l.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Wait until BIRD's session with `neighbor` satisfies `pred`.
pub async fn wait_bird(
    bird: &RealServer,
    neighbor: &str,
    what: &str,
    pred: impl Fn(&[String]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let line = bird_session(bird, neighbor).await;
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

pub async fn start(params: Value, policy: &str) -> (AppState, ServerId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "bfd".into(),
        port: Some(4784),
        host: Some("127.0.0.2".into()),
        instruction: Some("Accept 127.0.0.1".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":policy}}),
        ]),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    (state, id)
}

pub async fn events(state: &AppState, id: ServerId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

async fn the_session(state: &AppState, id: ServerId) -> u32 {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            for conn in s.connections.values() {
                if state.has_peer_handle(id, conn.id.as_u32()).await {
                    return conn.id.as_u32();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("no BFD session registered a peer handle");
}

#[tokio::test]
async fn bird_session_up_renegotiated_and_taken_down() {
    let _port = PORT_4784.lock().await;
    let bird = bird("127.0.0.2", "").await;
    let (state, id) = start(json!({}), POLICY).await;

    // Up, and then BIRD sending at the 400 ms NetGet asked for once Up.
    wait_bird(&bird, "127.0.0.2", "Up at 0.400", |l| {
        l.get(2).map(String::as_str) == Some("Up") && l.get(4).map(String::as_str) == Some("0.400")
    })
    .await;
    let request = &events(&state, id, "bfd_session_request").await[0];
    assert_eq!(request["peer"], "127.0.0.1", "{request}");
    assert_eq!(request["multihop"], true, "{request}");
    assert_eq!(
        request["desired_min_tx_ms"], 1000,
        "BIRD sends slowly until Up: {request}"
    );
    let ups = events(&state, id, "bfd_session_state").await;
    assert!(ups.iter().any(|e| e["state"] == "Up"), "{ups:?}");

    // The dashboard's action on this peer: AdminDown, which BIRD sees as its neighbour going
    // down, and back.
    let conn = the_session(&state, id).await;
    let outcome = state
        .send_to_peer(
            id,
            conn,
            json!({"type":"bfd_admin_down"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ClientSendOutcome::Executed { .. }),
        "{outcome:?}"
    );
    wait_bird(&bird, "127.0.0.2", "Down", |l| {
        l.get(2).map(String::as_str) == Some("Down")
    })
    .await;
    state
        .send_to_peer(
            id,
            conn,
            json!({"type":"bfd_admin_up"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    wait_bird(&bird, "127.0.0.2", "Up again", |l| {
        l.get(2).map(String::as_str) == Some("Up")
    })
    .await;
    // An action the session refuses is refused, not applied.
    let refused = state
        .send_to_peer(
            id,
            conn,
            json!({"type":"bfd_set_timers","desired_min_tx_ms":1}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(refused, ClientSendOutcome::Rejected { .. }),
        "{refused:?}"
    );

    // Stopping the server: BIRD detects the silence and goes Down.
    state.remove_server(id).await;
    wait_bird(&bird, "127.0.0.2", "Down after stop", |l| {
        l.get(2).map(String::as_str) == Some("Down")
    })
    .await;
}

const ACCEPT_ALL: &str = r#"import json,sys
i=json.load(sys.stdin)
print(json.dumps({'actions':[{'type':'bfd_accept_session','desired_min_tx_ms':100,'required_min_rx_ms':100}] if i['event_type_id']=='bfd_session_request' else []}))"#;

#[tokio::test]
async fn bird_keyed_sha1_right_and_wrong_key() {
    let _port = PORT_4784.lock().await;
    let auth = format!("authentication keyed sha1;\n    password \"{KEY}\" {{ id 1; }};");
    let bird = bird("127.0.0.2", &auth).await;
    let (state, id) = start(
        json!({"auth_type":"keyed_sha1","auth_key_id":1,"auth_password":KEY}),
        ACCEPT_ALL,
    )
    .await;
    wait_bird(&bird, "127.0.0.2", "Up", |l| {
        l.get(2).map(String::as_str) == Some("Up")
    })
    .await;
    let request = &events(&state, id, "bfd_session_request").await[0];
    assert_eq!(request["authentication"], "keyed_sha1", "{request}");
    state.remove_server(id).await;
    wait_bird(&bird, "127.0.0.2", "Down", |l| {
        l.get(2).map(String::as_str) == Some("Down")
    })
    .await;

    // The wrong key: NetGet drops every packet BIRD sends, so BIRD never leaves Down.
    let (state, id) = start(
        json!({"auth_type":"keyed_sha1","auth_key_id":1,"auth_password":"not-the-key"}),
        ACCEPT_ALL,
    )
    .await;
    // BIRD sends once a second while Down; four seconds is four rejected packets.
    let until = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < until {
        let line = bird_session(&bird, "127.0.0.2").await;
        assert_eq!(line.get(2).map(String::as_str), Some("Down"), "{line:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The key is checked before the model is asked: a peer that cannot authenticate is not
    // a session request at all.
    assert!(events(&state, id, "bfd_session_request").await.is_empty());
    state.remove_server(id).await;
}
