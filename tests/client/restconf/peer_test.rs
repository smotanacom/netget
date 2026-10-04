//! NetGet's RESTCONF client against FreeCONF's RESTCONF server (Go, independent, unchanged)
//! serving its car example: discovery through host-meta, the module list, reads of the module,
//! a list entry and a leaf, a PATCH read back, a DELETE of a list entry, a missing entry, and
//! car:addOil both accepted and refused with an RFC 8040 error. Fails, never skips.
use crate::helpers::restconf::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_against_freeconf() {
    let server = start_freeconf_server().await.unwrap();
    let state = state();
    let cid = client_in(&state, server.addr()).await.unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = &logs(&state, owner, "restconf_connected", 1).await[0];
    assert_eq!(connected["root"], "/restconf", "{connected}");
    assert!(
        connected["modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "car" && m["namespace"] == "freeconf.org/car"),
        "{connected}"
    );
    for a in [
        json!({"type": "restconf_get", "path": "car:"}),
        json!({"type": "restconf_get", "path": "car:tire=1"}),
        json!({"type": "restconf_patch", "path": "car:", "data": {"car:speed": 40}}),
        json!({"type": "restconf_get", "path": "car:speed"}),
        json!({"type": "restconf_delete", "path": "car:tire=3"}),
        json!({"type": "restconf_get", "path": "car:tire=3"}),
        json!({"type": "restconf_invoke", "operation": "car:addOil", "input": {"drainFirst": true, "amount": 10}}),
        json!({"type": "restconf_invoke", "operation": "car:addOil", "input": {"drainFirst": true, "amount": 2.5}}),
    ] {
        let r = state
            .send_to_client(cid, a.clone(), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    let r: Vec<Value> = logs(&state, owner, "restconf_response", 8).await;
    assert_eq!(
        (r[0]["status"].as_u64(), r[0]["data"]["car:speed"].as_i64()),
        (Some(200), Some(1000)),
        "{}",
        r[0]
    );
    assert_eq!(r[1]["data"]["pos"], 1, "{}", r[1]);
    assert!(
        r[2]["status"]
            .as_u64()
            .is_some_and(|s| (200..300).contains(&s)),
        "{}",
        r[2]
    );
    assert_eq!(r[3]["data"]["speed"], 40, "{}", r[3]);
    assert!(
        r[4]["status"]
            .as_u64()
            .is_some_and(|s| (200..300).contains(&s)),
        "{}",
        r[4]
    );
    assert_eq!(r[5]["status"], 404, "{}", r[5]);
    assert_eq!(
        (
            r[6]["status"].as_u64(),
            r[6]["data"]["car:output"]["oilLevel"].as_f64()
        ),
        (Some(200), Some(10.0)),
        "{}",
        r[6]
    );
    assert_eq!(r[7]["status"], 500, "{}", r[7]);
    assert_eq!(
        r[7]["errors"][0]["error-tag"], "operation-failed",
        "{}",
        r[7]
    );
    assert!(
        r[7]["errors"][0]["error-message"]
            .as_str()
            .unwrap()
            .contains("invalid oil change level"),
        "{}",
        r[7]
    );
    state.remove_client(cid).await;
}
