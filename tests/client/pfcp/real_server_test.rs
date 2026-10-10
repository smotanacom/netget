//! NetGet's PFCP client (an SMF) against **wmnsk/go-pfcp** playing the UPF (`pfcp-peer upf`,
//! built by `tests/server/pfcp/install_peers.py`; `NETGET_PFCP_GO_PEER` names it). The peer
//! parses every request with go-pfcp and prints what it decoded, so the assertions are on
//! what an independent implementation read off NetGet's bytes. Fails rather than skips
//! without it. No LLM calls.
//!
//! The chain: on `pfcp_ready` the model associates. On the accepted association it
//! establishes a session; on the accepted establishment it modifies that session using the
//! cp_seid it was given; on the accepted modification it deletes the session. The UPF
//! heartbeats NetGet in between, and NetGet answers in Rust.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='pfcp_ready':
  a=[{'type':'pfcp_associate'}]
elif t=='pfcp_response' and e.get('cause')=='request_accepted':
  r=e['request']
  if r=='association_setup_request':
    a=[{'type':'pfcp_establish_session','ies':{
      'create_pdr':[{'pdr_id':1,'precedence':255,'pdi':{'source_interface':'access','f_teid':{'choose':True},'ue_ip_address':{'ipv4':'10.60.0.1'}},'outer_header_removal':0,'far_id':1},
                    {'pdr_id':2,'precedence':255,'pdi':{'source_interface':'core','ue_ip_address':{'ipv4':'10.60.0.1','direction':'destination'}},'far_id':2}],
      'create_far':[{'far_id':1,'apply_action':['FORW'],'forwarding_parameters':{'destination_interface':'core','network_instance':'internet'}},
                    {'far_id':2,'apply_action':['FORW'],'forwarding_parameters':{'destination_interface':'access','outer_header_creation':{'description':['gtpu_udp_ipv4'],'teid':43981,'ipv4':'10.0.0.9'}}}]}}]
  elif r=='session_establishment_request':
    a=[{'type':'pfcp_modify_session','cp_seid':e['cp_seid'],'ies':{'update_far':{'far_id':2,'apply_action':['BUFF','NOCP']}}}]
  elif r=='session_modification_request':
    a=[{'type':'pfcp_delete_session','cp_seid':e['cp_seid']}]
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

#[tokio::test]
async fn netget_smf_against_go_pfcp_upf() {
    let peer = std::env::var("NETGET_PFCP_GO_PEER").unwrap_or_default();
    assert!(
        !peer.is_empty() && std::path::Path::new(&peer).exists(),
        "NETGET_PFCP_GO_PEER must name the go-pfcp peer: python3 tests/server/pfcp/install_peers.py <dir>"
    );
    let mut upf = tokio::process::Command::new(peer)
        .args(["upf", "127.0.0.1:0"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(upf.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let addr = first["listening"].as_str().unwrap().to_string();

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "pfcp".into(),
        remote_addr: Some(addr.clone()),
        instruction: Some("Run one session through its life".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}})]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");

    // What go-pfcp decoded, in order.
    let mut got = Vec::new();
    let read = async {
        while let Ok(Some(l)) = lines.next_line().await {
            let v: Value = serde_json::from_str(&l).unwrap_or_else(|e| panic!("{e}: {l}"));
            let done = v["got"] == "session_deletion";
            got.push(v);
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .unwrap_or_else(|_| panic!("the chain stalled: {got:?}"));
    let find = |name: &str| {
        got.iter()
            .find(|g| g["got"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name}: {got:?}"))
    };
    assert_eq!(
        find("association_setup"),
        json!({"got": "association_setup", "node_id": "127.0.0.1", "recovery_time_stamp": true})
    );
    let est = find("session_establishment");
    assert_eq!(est["cp_seid"], 1, "{est}");
    assert_eq!(est["header_seid"], 0, "{est}");
    assert_eq!(est["node_id"], "127.0.0.1", "{est}");
    assert_eq!(
        est["pdrs"],
        json!([{"pdr_id": 1, "far_id": 1, "source_interface": 0}, {"pdr_id": 2, "far_id": 2, "source_interface": 1}]),
        "{est}"
    );
    assert_eq!(
        est["fars"][1],
        json!({"far_id": 2, "apply_action": [2], "outer_teid": 43981, "outer_ipv4": "10.0.0.9"}),
        "{est}"
    );
    // The modification and deletion went to the UPF's SEID (0x9999), learned from its answer.
    let m = find("session_modification");
    assert_eq!(
        m,
        json!({"got": "session_modification", "header_seid": 0x9999, "update_far": [{"far_id": 2, "apply_action": [12]}]})
    );
    assert_eq!(find("session_deletion")["header_seid"], 0x9999);
    // NetGet saw the UPF's answer to the establishment, F-TEID included.
    let resp = wait_event(&state, id, |e| {
        e["event_type"] == "pfcp_response"
            && e["request"]["request"] == "session_establishment_request"
    })
    .await;
    assert_eq!(resp["up_seid"], 0x9999, "{resp}");
    assert_eq!(
        resp["ies"]["created_pdr"]["f_teid"],
        json!({"teid": 0x101, "ipv4": "127.0.0.1"}),
        "{resp}"
    );

    // The UPF heartbeated NetGet after the establishment and got an answer.
    let hb = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match lines.next_line().await {
                Ok(Some(l)) if l.contains("heartbeat_response") => return l,
                Ok(Some(_)) => continue,
                _ => return String::new(),
            }
        }
    })
    .await
    .unwrap_or_default();
    // (it can arrive before the deletion line; check what was already read too)
    assert!(
        hb.contains("\"seq_ok\":true")
            || got
                .iter()
                .any(|g| g["got"] == "heartbeat_response" && g["seq_ok"] == true),
        "the UPF's heartbeat was not answered: {hb} {got:?}"
    );

    // Injected: a session that does not exist is refused before anything is sent.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"pfcp_delete_session","cp_seid":77}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("cp_seid 77")),
        "{outcome:?}"
    );
}
