//! NetGet's libp2p host against **go-libp2p** (TCP, Noise, yamux only; `libp2p-peer`, built
//! by `install_peers.py`, named by `NETGET_LIBP2P_GO_PEER`). go-libp2p negotiates, encrypts
//! and multiplexes every byte, verifies NetGet's peer id against its Noise static key, and
//! parses NetGet's identify. Fails rather than skips without the peer. No LLM calls: a python
//! policy is the model. Then multistream refusals over raw TCP, and a failed handler.
use netget::cli::management::ServerForm;
use netget::server::libp2p::noise::Identity;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const SEED: &str = "netget-libp2p-test";

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='libp2p_peer_connected':
  a=[{'type':'libp2p_open_stream','protocol':'/netget/chat/1.0.0','data':'welcome'}]
elif t=='libp2p_message' and not e['data'].startswith('echo: '):
  a=[{'type':'libp2p_send','data':'echo: '+e['data']}]
print(json.dumps({'actions':a}))"#;

pub fn peer() -> String {
    let p = std::env::var("NETGET_LIBP2P_GO_PEER").unwrap_or_default();
    assert!(
        !p.is_empty() && std::path::Path::new(&p).exists(),
        "NETGET_LIBP2P_GO_PEER must name the go-libp2p peer: python3 tests/server/libp2p/install_peers.py <dir>"
    );
    p
}

fn peer_id() -> String {
    netget::server::libp2p::identity_from_params(Some(SEED.into()))
        .unwrap()
        .peer_id_string()
}

async fn start(handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "libp2p".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a libp2p peer".into()),
        startup_params: Some(json!({"private_key_seed": SEED})),
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

async fn go(args: &[&str]) -> (bool, Vec<Value>, String) {
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(peer()).args(args).output(),
    )
    .await
    .expect("the go-libp2p peer did not finish")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let steps = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l:?}\n{text}")))
        .collect();
    (
        out.status.success(),
        steps,
        format!("{text}{}", String::from_utf8_lossy(&out.stderr)),
    )
}

fn step<'a>(steps: &'a [Value], name: &str, text: &str) -> &'a Value {
    steps
        .iter()
        .find(|s| s["step"] == name)
        .unwrap_or_else(|| panic!("no {name}: {text}"))
}

#[tokio::test]
async fn go_libp2p_dials_netget() {
    let policy = vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ];
    let (state, id, port) = start(Some(policy)).await;
    let addr = format!("/ip4/127.0.0.1/tcp/{port}/p2p/{}", peer_id());
    let (ok, steps, text) = go(&["dial", &addr]).await;
    assert!(ok, "{text}");

    // go-libp2p's own reading of NetGet's identify.
    let identify = step(&steps, "identify", &text);
    assert_eq!(identify["peer_id"], peer_id().as_str());
    assert!(
        identify["agent"].as_str().unwrap().starts_with("netget/"),
        "{identify}"
    );
    assert_eq!(identify["protocol_version"], "ipfs/0.1.0");
    assert_eq!(
        identify["protocols"],
        json!([
            "/ipfs/id/1.0.0",
            "/ipfs/id/push/1.0.0",
            "/ipfs/ping/1.0.0",
            "/netget/chat/1.0.0"
        ])
    );
    assert_eq!(
        identify["addrs"],
        json!([format!("/ip4/127.0.0.1/tcp/{port}")])
    );
    assert_eq!(step(&steps, "ping", &text)["ok"], true);
    assert_eq!(
        step(&steps, "talk", &text)["replies"],
        json!(["echo: hello", "echo: how are you?"])
    );
    assert_eq!(step(&steps, "unsupported", &text)["refused"], true);
    let oversize = step(&steps, "oversize", &text);
    assert!(
        oversize["error"].as_str().unwrap().contains("reset"),
        "an oversized message resets its stream: {oversize}"
    );
    // The stream the model opened when the peer connected; go's echo answered it.
    let inbound = step(&steps, "inbound", &text);
    assert_eq!(
        (inbound["protocol"].as_str(), inbound["body"].as_str()),
        (Some("/netget/chat/1.0.0"), Some("welcome"))
    );

    // What the model was shown.
    let connected = &events(&state, id, "libp2p_peer_connected").await[0];
    assert_eq!(
        connected["agent_version"], "netget-test-go-peer",
        "{connected}"
    );
    assert!(connected["peer_id"]
        .as_str()
        .unwrap()
        .starts_with("12D3KooW"));
    assert!(
        connected["protocols"]
            .as_array()
            .unwrap()
            .contains(&json!("/netget/chat/1.0.0")),
        "{connected}"
    );
    let messages = events(&state, id, "libp2p_message").await;
    let mut bodies: Vec<&str> = messages
        .iter()
        .map(|m| m["data"].as_str().unwrap())
        .collect();
    bodies.sort();
    assert_eq!(
        bodies,
        ["echo: welcome", "hello", "how are you?"],
        "{messages:?}"
    );
    let hello = messages.iter().find(|m| m["data"] == "hello").unwrap();
    assert_eq!(hello["protocol"], "/netget/chat/1.0.0");
    assert_eq!(hello["encoding"], "utf8");
    assert_eq!(
        hello["stream_id"].as_u64().unwrap() % 2,
        1,
        "dialler streams are odd"
    );
}

#[tokio::test]
async fn a_wrong_peer_id_is_refused_by_the_dialler() {
    let (_state, _id, port) = start(Some(vec![
        json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
    ]))
    .await;
    // Another valid peer id: go-libp2p must refuse NetGet's proof of identity.
    let other = Identity::from_seed([7; 32]).peer_id_string();
    let addr = format!("/ip4/127.0.0.1/tcp/{port}/p2p/{other}");
    let (_, steps, text) = go(&["probe", &addr]).await;
    let probe = step(&steps, "probe", &text);
    assert!(
        probe["connect_error"]
            .as_str()
            .unwrap_or_default()
            .contains("peer id mismatch"),
        "{probe}"
    );
}

#[tokio::test]
async fn a_failed_handler_resets_the_stream() {
    let (_state, _id, port) = start(None).await;
    let addr = format!("/ip4/127.0.0.1/tcp/{port}/p2p/{}", peer_id());
    let (_, steps, text) = go(&["probe", &addr]).await;
    let probe = step(&steps, "probe", &text);
    assert_eq!(probe["reply"], "", "{probe}");
    assert!(
        probe["error"].as_str().unwrap().contains("reset"),
        "the peer learns at once rather than waiting: {probe}"
    );
}

async fn ms_read(s: &mut tokio::net::TcpStream) -> String {
    let n = s.read_u8().await.unwrap() as usize; // every message here is under 128 bytes
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).await.unwrap();
    String::from_utf8(b).unwrap()
}

fn ms(s: &str) -> Vec<u8> {
    let mut v = vec![(s.len() + 1) as u8];
    v.extend(s.as_bytes());
    v.push(b'\n');
    v
}

#[tokio::test]
async fn multistream_refusals_over_raw_tcp() {
    let (_state, _id, port) = start(None).await;
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    s.write_all(&[ms("/multistream/1.0.0"), ms("/tls/1.0.0")].concat())
        .await
        .unwrap();
    assert_eq!(ms_read(&mut s).await, "/multistream/1.0.0\n");
    assert_eq!(ms_read(&mut s).await, "na\n", "TLS security is not offered");
    // Past the proposal bound the connection is dropped.
    for _ in 1..netget::server::libp2p::wire::MAX_PROPOSALS {
        s.write_all(&ms("/plaintext/2.0.0")).await.unwrap();
        assert_eq!(ms_read(&mut s).await, "na\n");
    }
    s.write_all(&ms("/plaintext/2.0.0")).await.unwrap();
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "closed without another answer: {rest:?}");

    // A multistream message announcing more than the bound is refused before it is read.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut big = Vec::new();
    netget::server::libp2p::wire::put_uvarint(
        &mut big,
        netget::server::libp2p::wire::MAX_MULTISTREAM_MESSAGE as u64 + 1,
    );
    s.write_all(&big).await.unwrap();
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "{rest:?}");
}

#[test]
fn base58_and_varint_round_trip() {
    use netget::server::libp2p::wire;
    let id = peer_id();
    assert!(id.starts_with("12D3KooW"), "{id}");
    assert_eq!(wire::base58(&wire::unbase58(&id).unwrap()), id);
    assert_eq!(wire::base58(&[0, 0, 1]), "112");
    for v in [0u64, 1, 127, 128, 300, 1 << 40] {
        let mut b = Vec::new();
        wire::put_uvarint(&mut b, v);
        assert_eq!(wire::get_uvarint(&b).unwrap(), (v, b.len()));
    }
    assert!(wire::get_uvarint(&[0xff; 11]).is_err());
}
