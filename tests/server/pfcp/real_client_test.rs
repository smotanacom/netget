//! NetGet's PFCP UPF against **wmnsk/go-pfcp** playing the SMF (`pfcp-peer smf`, built by
//! `install_peers.py`; `NETGET_PFCP_GO_PEER` names it). Every request is go-pfcp's encoding
//! and every response is parsed by go-pfcp. Fails rather than skips without it. Then the
//! pcap oracle (tshark's pfcp dissector) reads NetGet's responses, and the codec's bounds are
//! checked on their own. No LLM calls: a python policy is the model.
use crate::helpers::pcap_oracle::PcapOracle;
use netget::cli::management::ServerForm;
use netget::server::pfcp::wire;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
ies={}
if t=='pfcp_session_establishment':
  pdrs=e['ies'].get('create_pdr',[])
  pdrs=pdrs if isinstance(pdrs,list) else [pdrs]
  ies={'created_pdr':[{'pdr_id':p['pdr_id'],'f_teid':{'teid':4096+p['pdr_id'],'ipv4':'127.0.0.1'}} for p in pdrs if p.get('pdi',{}).get('f_teid',{}).get('choose')]}
print(json.dumps({'actions':[{'type':'pfcp_respond','cause':'request_accepted','ies':ies}]}))"#;

pub fn peer() -> String {
    let p = std::env::var("NETGET_PFCP_GO_PEER").unwrap_or_default();
    assert!(
        !p.is_empty() && std::path::Path::new(&p).exists(),
        "NETGET_PFCP_GO_PEER must name the go-pfcp peer: python3 tests/server/pfcp/install_peers.py <dir>"
    );
    p
}

async fn start() -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "pfcp".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a UPF".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
        ]),
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

#[tokio::test]
async fn go_pfcp_smf_drives_netget_upf() {
    let (state, id, port) = start().await;
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(peer())
            .args(["smf", &format!("127.0.0.1:{port}")])
            .output(),
    )
    .await
    .expect("the go-pfcp SMF did not finish")
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

    // Rust's own refusal, before any association, asked of no model.
    assert_eq!(step("establish_unassociated")["cause"], 72, "{text}");
    let assoc = step("associate");
    assert_eq!(
        assoc,
        json!({"step": "associate", "cause": 1, "node_id": "127.0.0.1", "recovery_time_stamp": true, "seq_ok": true})
    );
    assert_eq!(
        step("heartbeat"),
        json!({"step": "heartbeat", "ok": true, "seq_ok": true})
    );
    let est = step("establish");
    assert_eq!(est["cause"], 1, "{est}");
    assert_eq!(
        est["header_seid"], 0x1111,
        "the response is addressed to the SMF's own SEID: {est}"
    );
    assert!(est["up_seid"].as_u64().is_some_and(|s| s != 0), "{est}");
    // The model gave PDR 1 (which asked for one with CHOOSE) its TEID; PDR 2 asked for none.
    assert_eq!(
        est["created_pdr"],
        json!([{"pdr_id": 1, "teid": 4097, "ipv4": "127.0.0.1"}]),
        "{est}"
    );
    let up_seid = est["up_seid"].clone();
    assert_eq!(
        step("modify"),
        json!({"step": "modify", "cause": 1, "header_seid": 0x1111})
    );
    assert_eq!(
        step("retransmit")["identical"],
        true,
        "a retransmitted request is answered from the cache"
    );
    assert_eq!(step("delete")["cause"], 1);
    assert_eq!(
        step("delete_again")["cause"],
        65,
        "the deleted session no longer exists"
    );

    // What the model was shown: go-pfcp's IEs, decoded into readable values.
    let e = &events(&state, id, "pfcp_session_establishment").await[0];
    assert_eq!(e["cp_seid"], 0x1111, "{e}");
    assert_eq!(e["up_seid"], up_seid, "{e}");
    let pdr1 = &e["ies"]["create_pdr"][0];
    assert_eq!(pdr1["pdi"]["f_teid"]["choose"], true, "{e}");
    assert_eq!(pdr1["pdi"]["source_interface"], "access", "{e}");
    assert_eq!(pdr1["pdi"]["ue_ip_address"]["ipv4"], "10.60.0.1", "{e}");
    assert_eq!(pdr1["pdi"]["network_instance"], "internet", "{e}");
    let far2 = &e["ies"]["create_far"][1];
    assert_eq!(far2["apply_action"], json!(["FORW"]), "{e}");
    assert_eq!(
        far2["forwarding_parameters"]["outer_header_creation"],
        json!({"description": ["gtpu_udp_ipv4"], "teid": 0xabcd, "ipv4": "10.0.0.9"}),
        "{e}"
    );
    let all = events(&state, id, "pfcp_session_request").await;
    let m = all
        .iter()
        .find(|e| e["message"] == "session_modification_request")
        .unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!(
        m["ies"]["update_far"]["apply_action"],
        json!(["BUFF", "NOCP"]),
        "{m}"
    );
    // The retransmission did not reach the model a second time.
    assert_eq!(
        events(&state, id, "pfcp_session_request").await.len(),
        2,
        "modify and delete only"
    );
}

#[tokio::test]
async fn responses_through_the_pcap_oracle_and_bounds() {
    let (_state, _id, port) = start().await;
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    s.connect(("127.0.0.1", port)).await.unwrap();
    let mut sent = Vec::new();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 65_536];
    let assoc = wire::encode(
        5,
        None,
        1,
        &json!({"node_id": {"ipv4": "127.0.0.1"}, "recovery_time_stamp": 1_700_000_000}),
    )
    .unwrap();
    let est = wire::encode(
        50,
        Some(0),
        2,
        &json!({"node_id": {"ipv4": "127.0.0.1"}, "f_seid": {"seid": 7, "ipv4": "127.0.0.1"},
            "create_pdr": {"pdr_id": 1, "precedence": 1, "pdi": {"source_interface": "access", "f_teid": {"choose": true}}, "far_id": 1},
            "create_far": {"far_id": 1, "apply_action": ["FORW"]}}),
    )
    .unwrap();
    for req in [assoc, est] {
        s.send(&req).await.unwrap();
        sent.push(req);
        let n = tokio::time::timeout(Duration::from_secs(20), s.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        got.push(buf[..n].to_vec());
    }
    let reply = wire::parse(&got[1]).unwrap();
    assert_eq!(reply.header.seid, Some(7));
    assert_eq!(
        reply.ies["created_pdr"]["f_teid"],
        json!({"teid": 4097, "ipv4": "127.0.0.1"})
    );
    let mut oracle = PcapOracle::udp("pfcp").port(wire::PORT);
    for b in &sent {
        oracle = oracle.to_server(b);
    }
    for b in &got {
        oracle = oracle.from_server(b);
    }
    oracle.assert_clean();

    // Grouped IEs nested past the bound: dropped, no response, and the server goes on.
    // Hand-built: the encoder refuses this depth itself.
    let mut ie = 56u16.to_be_bytes().to_vec();
    ie.extend(2u16.to_be_bytes());
    ie.extend(1u16.to_be_bytes());
    for _ in 0..wire::MAX_DEPTH + 1 {
        let mut outer = 2u16.to_be_bytes().to_vec(); // PDI, a grouped IE
        outer.extend((ie.len() as u16).to_be_bytes());
        outer.extend(&ie);
        ie = outer;
    }
    let mut msg = vec![0x20, 1];
    msg.extend(((4 + ie.len()) as u16).to_be_bytes());
    msg.extend([0, 0, 9, 0]);
    msg.extend(&ie);
    assert!(wire::parse(&msg).is_err());
    s.send(&msg).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), s.recv(&mut buf))
            .await
            .is_err(),
        "answered a depth bomb"
    );
    let hb = wire::encode(1, None, 10, &json!({"recovery_time_stamp": 1_700_000_000})).unwrap();
    s.send(&hb).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), s.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wire::parse(&buf[..n]).unwrap().header.message_type, 2);
    // A version the UPF does not speak gets Version Not Supported.
    let mut v2 = hb.clone();
    v2[0] = 0x40;
    s.send(&v2).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), s.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((n, buf[1]), (8, 11), "a header-only Version Not Supported Response");
}

#[test]
fn codec_round_trip() {
    let ies = json!({
        "node_id": {"fqdn": "upf.example.net"},
        "cause": "request_accepted",
        "f_seid": {"seid": 99, "ipv4": "192.0.2.1"},
        "created_pdr": [{"pdr_id": 1, "f_teid": {"teid": 1, "ipv4": "192.0.2.2"}}, {"pdr_id": 2, "f_teid": {"teid": 2, "ipv4": "192.0.2.2"}}],
        "update_far": {"far_id": 3, "apply_action": ["DROP"]},
        "ie_200": {"hex": "0102"}
    });
    let b = wire::encode(51, Some(5), 77, &ies).unwrap();
    let m = wire::parse(&b).unwrap();
    assert_eq!(
        (m.header.message_type, m.header.seid, m.header.sequence),
        (51, Some(5), 77)
    );
    assert_eq!(m.ies, ies);
    assert!(wire::encode(51, None, 1, &json!({"no_such_ie": 1})).is_err());
    assert!(wire::encode(51, None, 1, &json!({"cause": "not_a_cause"})).is_err());
}
