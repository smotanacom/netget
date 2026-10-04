//! NetGet's JMAP client against NetGet's JMAP server over HTTPS, trusting the certificate the
//! server publishes, with a Bearer token: a request with a result reference, an injected
//! request, and a session whose apiUrl points elsewhere refused. Fails, never skips.
use crate::helpers::jmap::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

const DRIVER: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='jmap_connected': out({'type':'jmap_request','calls':[['Mailbox/query',{'filter':{'role':'inbox'}},'0'],['Mailbox/get',{'#ids':{'resultOf':'0','name':'Mailbox/query','path':'/ids'}},'1']]})
out()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let mut params = alice();
    params["api_tokens"] = json!({"tok-1": "alice@example.com"});
    let (sid, addr) = server_in(&state, params).await;
    let ca = certificate(&state, sid, dir.path()).await;
    let remote = format!("localhost:{}", addr.port());
    let id = client_in(
        &state,
        remote.clone(),
        script(DRIVER),
        json!({"api_token": "tok-1", "ca_cert_path": ca}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(id.as_u32());
    let r = wait_for(&state, owner, "jmap_response", |e| {
        e["method_responses"]
            .as_array()
            .is_some_and(|m| m.len() == 2)
    })
    .await;
    assert_eq!(r["method_responses"][1][0], "Mailbox/get", "{r}");
    assert_eq!(
        r["method_responses"][1][1]["list"][0]["name"], "Inbox",
        "{r}"
    );
    assert_eq!(r["method_responses"][1][1]["accountId"], "a1", "{r}");
    assert_eq!(r["session_changed"], false);

    let sent = state
        .send_to_client(id, json!({"type": "jmap_request", "calls": [["Email/changes", {"sinceState": "ancient"}, "x"]]}), Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let e = wait_for(&state, owner, "jmap_response", |e| {
        e["method_responses"][0][2] == "x"
    })
    .await;
    assert_eq!(
        e["method_responses"][0][1]["type"], "cannotCalculateChanges",
        "{e}"
    );
    let refused = state
        .send_to_client(
            id,
            json!({"type": "jmap_request", "calls": []}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(refused, ClientSendOutcome::Rejected { .. }),
        "{refused:?}"
    );

    // The session names its apiUrl by the Host it was asked with; a client that reached it as
    // 127.0.0.1 while trusting only "localhost" is refused by TLS, and plain HTTP to an HTTPS
    // server never gets a session.
    assert!(client_in(
        &state,
        remote.clone(),
        script(DRIVER),
        json!({"api_token": "tok-1", "tls": false})
    )
    .await
    .is_err());
    assert!(client_in(
        &state,
        remote,
        script(DRIVER),
        json!({"api_token": "tok-1"})
    )
    .await
    .is_err());
    state.remove_server(sid).await;
}
