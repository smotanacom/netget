use crate::helpers::diameter::{peer_client, policy, server, PeerKind};
use serde_json::json;
#[tokio::test]
async fn both_unchanged_independent_clients_accept_reject_all_three_auth_types() {
    let script = json!({"event_pattern":"diameter_aa_request","handler":{"type":"script","language":"python","code":"import json,sys\nr=json.load(sys.stdin)['event']['request']\nv='accept' if r['auth_request_type']==2 or r.get('password')=='Correct' else 'reject'\nprint(json.dumps({'actions':[{'type':'respond_diameter_aa','reply':{'verdict':v,'service_type':1,'filter_ids':['test-policy'],'session_timeout':60}}]}))"}});
    let (state, id, addr, _) = server(Some(vec![script]), None).await;
    for kind in [PeerKind::Python, PeerKind::Go] {
        for typ in 1..=3 {
            for (password, expected) in [
                ("Correct", 2001),
                ("wrong", if typ == 2 { 2001 } else { 4001 }),
            ] {
                let response = peer_client(kind, addr, password, typ).await;
                assert_eq!(response["source_modified"], false);
                assert_eq!(response["result_code"], expected);
            }
        }
    }
    let rows = state.list_access_logs(None).await;
    let rows: Vec<_> = rows
        .iter()
        .filter(|e| e.event_type == "diameter_aa_request")
        .collect();
    assert_eq!(rows.len(), 12);
    assert!(rows
        .iter()
        .any(|e| e.request["request"]["password"] == "Correct"));
    assert!(rows.iter().all(|e| e.response[0]["type"]
        == if e.request["request"].get("password").is_some() {
            "private_handler_result"
        } else {
            "respond_diameter_aa"
        }));
    state.remove_server(id).await;
}
#[tokio::test]
async fn unmatched_requests_deny_in_both_independent_peers_without_model() {
    let (state, id, addr, _) = server(None, None).await;
    for kind in [PeerKind::Python, PeerKind::Go] {
        assert_eq!(
            peer_client(kind, addr, "Correct", 3).await["result_code"],
            4001
        );
    }
    assert_eq!(
        state
            .list_access_logs(None)
            .await
            .iter()
            .filter(|e| e.event_type == "diameter_aa_request")
            .count(),
        2
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn independently_encoded_request_gets_safe_backend_error_and_error_flag() {
    let (state,id,addr,_)=server(Some(vec![json!({"event_pattern":"diameter_aa_request","handler":{"type":"static","actions":[{"type":"respond_diameter_aa","reply":{"verdict":"error"}}]}})]),None).await;
    for kind in [PeerKind::Python, PeerKind::Go] {
        assert_eq!(
            peer_client(kind, addr, "Correct", 3).await["result_code"],
            5012
        );
    }
    state.remove_server(id).await;
}
#[tokio::test]
async fn independent_peers_receive_selected_accept_verdict() {
    let (state, id, addr, _) = server(Some(policy("accept")), None).await;
    for kind in [PeerKind::Python, PeerKind::Go] {
        assert_eq!(
            peer_client(kind, addr, "Correct", 3).await["result_code"],
            2001
        );
    }
    state.remove_server(id).await;
}
