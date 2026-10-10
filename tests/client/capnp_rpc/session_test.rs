//! The Cap'n Proto RPC client against NetGet's own server (its handler script copied from
//! `tests/server/capnp_rpc/wire_test.rs`): a handler-driven call on connect, injected calls
//! with results and exceptions, local refusals, and the follow-up bound.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

pub const SCHEMA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/server/capnp_rpc/directory.capnp"
);

const DIRECTORY_SCRIPT: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; m=e['method']; p=e['params']
if m=='ping': a={'type':'capnp_return','results':{'pong':'pong from netget'}}
elif m=='add': a={'type':'capnp_return','results':{'sum':p['a']+p['b']}}
elif m=='lookup' and p['name']=='missing': a={'type':'capnp_exception','reason':'no entry named missing'}
elif m=='lookup': a={'type':'capnp_return','results':{'entry':{'name':p['name'],'size':1234,'kind':'file','target':'/docs/'+p['name']}}}
elif m=='store': a={'type':'capnp_return','results':{'stored':p['entry'],'count':len(p['entry']['children'] or [])+1}}
else: a={'type':'capnp_exception','reason':'refused: '+p['why'],'kind':'failed'}
print(json.dumps({'actions':[a]}))"#;

/// Fail, naming the package, when the Cap'n Proto compiler is absent.
pub fn require_capnp() {
    let ok = std::process::Command::new("capnp")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok,
        "the capnp compiler is required: apt-get install capnproto (or brew install capnp)"
    );
}

/// The entry the connect handler stores.
pub fn connect_entry() -> Value {
    json!({"name":"from-netget","size":7,"kind":"link","tags":["a","b"],"owner":{"uid":42,"gid":7},
           "blob":{"$hex":"cafe"},"children":[{"name":"kid","priority":9}],"scores":[3.25],"hidden":false})
}

async fn netget_server() -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "capnp-rpc".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve".into()),
        startup_params: Some(json!({"schema": SCHEMA, "interface": "Directory"})),
        event_handlers: Some(vec![json!({"event_pattern":"capnp_call","handler":{"type":"script","language":"python","code":DIRECTORY_SCRIPT}})]),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, format!("127.0.0.1:{}", addr.port()))
}

/// A client whose connect handler stores `connect_entry()` and answers every result with
/// `then`.
pub async fn client(remote: String, then: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "capnp-rpc".into(),
        remote_addr: Some(remote),
        instruction: Some("Use the directory".into()),
        startup_params: Some(json!({"schema": SCHEMA, "interface": "Directory"})),
        event_handlers: Some(vec![
            json!({"event_pattern":"capnp_connected","handler":{"type":"static","actions":[{"type":"capnp_call","method":"store","params":{"entry":connect_entry()}}]}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":then}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}

/// Call a method through the client and return the capnp_result it produced.
pub async fn call(state: &AppState, id: ClientId, method: &str, params: Value) -> Value {
    match state
        .send_to_client(
            id,
            json!({"type":"capnp_call","method":method,"params":params}),
            Duration::from_secs(30),
        )
        .await
        .unwrap()
    {
        ClientSendOutcome::Executed { detail } => serde_json::from_str(&detail).unwrap(),
        other => panic!("{method}: {other:?}"),
    }
}

/// The connect handler's store came back from the server as the server decoded it.
pub async fn check_connect_store(state: &AppState, id: ClientId) {
    let data = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let found = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .find(|e| e["event_type"] == "capnp_result" && e["request"]["method"] == "store");
            if let Some(e) = found {
                break e["request"].clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .ok();
    let Some(data) = data else {
        let status = state
            .get_client(id)
            .await
            .map(|c| format!("{:?}", c.status));
        let logs = state
            .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
            .await;
        panic!(
            "the connect handler's store was not answered; client {status:?}; logs {}",
            serde_json::to_string(&logs).unwrap()
        );
    };
    let stored = &data["results"]["stored"];
    assert_eq!(stored["name"], "from-netget", "{data}");
    assert_eq!(stored["kind"], "link");
    assert_eq!(stored["blob"], json!({"$hex": "cafe"}));
    assert_eq!(stored["owner"], json!({"uid": 42, "gid": 7}));
    assert_eq!(stored["children"][0]["priority"], 9);
    assert_eq!(stored["children"][0]["hidden"], true);
    assert_eq!(stored["scores"], json!([3.25]));
    assert_eq!(data["results"]["count"], 2);
}

#[tokio::test]
async fn calls_against_netget() {
    require_capnp();
    let (server_state, server_id, addr) = netget_server().await;
    let (state, id) = client(addr, json!([])).await;
    check_connect_store(&state, id).await;
    let r = call(&state, id, "add", json!({"a": 2, "b": 3})).await;
    assert_eq!(r["results"], json!({"sum": 5}), "{r}");
    let r = call(&state, id, "ping", json!({})).await;
    assert_eq!(r["results"]["pong"], "pong from netget");
    let r = call(&state, id, "lookup", json!({"name": "missing"})).await;
    assert_eq!(
        (r["results"].clone(), r["exception"]["reason"].clone()),
        (Value::Null, json!("no entry named missing"))
    );
    for bad in [
        json!({"type":"capnp_call","method":"nope","params":{}}),
        json!({"type":"capnp_call","method":"add","params":{"a":"two"}}),
        json!({"type":"capnp_call","method":"add","params":{"c":1}}),
        json!({"type":"capnp_call","method":"add","params":{"a":4294967296u64}}),
    ] {
        let out = state
            .send_to_client(id, bad.clone(), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            matches!(out, ClientSendOutcome::Rejected { .. }),
            "{bad}: {out:?}"
        );
    }
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}

#[tokio::test]
async fn handler_chain_is_bounded() {
    require_capnp();
    let (server_state, server_id, addr) = netget_server().await;
    let (state, id) = client(
        addr,
        json!([{"type":"capnp_call","method":"add","params":{"a":1,"b":1}}]),
    )
    .await;
    let count = || async {
        server_state
            .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
            .await
            .iter()
            .filter(|e| {
                serde_json::to_string(e)
                    .unwrap()
                    .contains("\"method\":\"add\"")
            })
            .count()
    };
    // The store is depth 1, so adds run at depths 2 through MAX_FOLLOWUP_DEPTH.
    let expected = netget::client::capnp_rpc::MAX_FOLLOWUP_DEPTH - 1;
    tokio::time::timeout(Duration::from_secs(60), async {
        while count().await < expected {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the chain ran");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(count().await, expected);
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
