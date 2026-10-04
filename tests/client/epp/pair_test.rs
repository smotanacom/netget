//! NetGet's EPP client against NetGet's EPP server over TLS, trusting the certificate the
//! server publishes: a handler-driven check, an injected info, a rejected injection and an
//! injected logout ending the session. Fails, never skips.
use crate::helpers::epp::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

const DRIVER: &str = r#"import json,sys
i=json.load(sys.stdin)
if i['event_type_id']=='epp_connected': print(json.dumps({'actions':[{'type':'epp_check','object':'domain','names':['taken.example','new.example']}]}))
else: print(json.dumps({'actions':[]}))
"#;

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, registrar()).await;
    let ca = certificate(&state, sid, dir.path()).await;
    let remote = format!("localhost:{}", addr.port());
    let id = client_in(
        &state,
        remote,
        script(DRIVER),
        json!({"client_id": "registrar1", "password": "secret-pw-1", "ca_cert_path": ca}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(id.as_u32());
    let check = wait_for(&state, owner, "epp_response", |e| e["command"] == "check").await;
    assert_eq!(
        check["data"]["results"][1],
        json!({"name": "new.example", "available": true, "reason": null}),
        "{check}"
    );

    let sent = state
        .send_to_client(
            id,
            json!({"type": "epp_info", "object": "domain", "name": "taken.example"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let info = wait_for(&state, owner, "epp_response", |e| e["command"] == "info").await;
    assert_eq!(
        (info["data"]["clID"].clone(), info["data"]["roid"].clone()),
        (json!("OtherReg"), json!("TAKEN1-REP")),
        "{info}"
    );
    assert_eq!(
        info["data"]["status"],
        json!(["clientTransferProhibited"]),
        "{info}"
    );
    let refused = state
        .send_to_client(
            id,
            json!({"type": "epp_check", "object": "widget", "names": ["x"]}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(refused, ClientSendOutcome::Rejected { .. }),
        "{refused:?}"
    );
    state
        .send_to_client(id, json!({"type": "epp_logout"}), Duration::from_secs(10))
        .await
        .unwrap();
    let logout = wait_for(&state, owner, "epp_response", |e| e["command"] == "logout").await;
    assert_eq!(logout["code"], 1500);
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the client kept its session after logout");
    state.remove_server(sid).await;
}
