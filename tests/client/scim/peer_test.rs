//! NetGet's SCIM client against scim2-server 0.4.0 — an independent service, unchanged:
//! discovery, creates, a filtered sorted list, get, PATCH (read back with a GET), PUT, a uniqueness conflict and
//! delete. Fails, never skips.
use crate::helpers::scim::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_provisions_against_scim2_server() {
    let srv = start_scim2_server().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        srv.addr(),
        json!({"scheme": "http", "base_path": ""}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "scim_connected", 1).await;
    let names: Vec<&str> = connected[0].request["resource_types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert!(
        names.contains(&"User") && names.contains(&"Group"),
        "{names:?}"
    );
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for (u, g) in [("bjensen", "Barbara"), ("alice", "Alice")] {
        assert!(matches!(send(json!({"type":"scim_create","resource_type":"Users","resource":{"userName":u,"name":{"givenName":g},"emails":[{"value":format!("{u}@example.com"),"type":"work"}]}})).await.unwrap(), ClientSendOutcome::Sent { .. }));
    }
    let created = logs(&state, owner, "scim_response", 2).await;
    let id = created[0].request["resource"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for a in [
        json!({"type":"scim_list","resource_type":"Users","filter":"emails[type eq \"work\"]","sort_by":"userName"}),
        json!({"type":"scim_get","resource_type":"Users","id":id}),
        json!({"type":"scim_patch","resource_type":"Users","id":id,"operations":[{"op":"replace","path":"name.givenName","value":"Babs"},{"op":"add","path":"title","value":"Engineer"}]}),
        json!({"type":"scim_get","resource_type":"Users","id":id}),
        json!({"type":"scim_replace","resource_type":"Users","id":id,"resource":{"userName":"bjensen","active":false}}),
        json!({"type":"scim_create","resource_type":"Users","resource":{"userName":"alice"}}),
        json!({"type":"scim_delete","resource_type":"Users","id":id}),
        json!({"type":"scim_get","resource_type":"Users","id":id}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "scim_response", 10).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(r[0]["status"], 201);
    let listed: Vec<&str> = r[2]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["userName"].as_str())
        .collect();
    assert_eq!(
        (r[2]["total_results"].as_u64(), listed),
        (Some(2), vec!["alice", "bjensen"])
    );
    assert_eq!(r[3]["resource"]["userName"], "bjensen");
    assert!(
        matches!(r[4]["status"].as_u64(), Some(200 | 204)),
        "{}",
        r[4]
    );
    assert_eq!(
        (
            r[5]["resource"]["name"]["givenName"].as_str(),
            r[5]["resource"]["title"].as_str()
        ),
        (Some("Babs"), Some("Engineer"))
    );
    assert_eq!(r[6]["resource"]["active"], false);
    assert_eq!(
        (r[7]["status"].as_u64(), r[7]["error"]["scim_type"].as_str()),
        (Some(409), Some("uniqueness"))
    );
    assert_eq!(r[8]["status"], 204);
    assert_eq!(r[9]["status"], 404);
    state.remove_client(cid).await;
}
