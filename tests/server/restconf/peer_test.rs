//! FreeCONF's RESTCONF client (Go, independent, unchanged) against NetGet's server: it loads
//! the module list from ietf-yang-library, reads FreeCONF's car module, edits the speed (a PATCH)
//! and reads it back, and invokes car:addOil, getting the handler's output unwrapped from
//! car:output. Fails, never skips.
//!
//! Two FreeCONF behaviours are recorded rather than asserted as successes: its client expects a
//! list entry unwrapped (`{"pos":1,…}`), while RFC 8040 section 3.5.3 wraps it
//! (`{"car:tire":[{…}]}`, which NetGet's handler answers), so its view of tire=1 is empty; and
//! it resolves a list key lazily without a request.
use crate::helpers::restconf::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn freeconf_client_against_netget() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, policy(&dir.path().join("car.json"))).await;
    let (ok, r, text) = freeconf_client(&format!("http://{addr}/restconf")).await;
    assert!(ok, "{text}");
    assert_eq!(r["connect"]["modules"], 1, "{text}");
    let car = &r["read"]["data"];
    assert_eq!(
        (car["speed"].as_i64(), car["tire"].as_array().map(Vec::len)),
        (Some(1000), Some(4)),
        "{text}"
    );
    assert_eq!(r["edit"]["error"], json!(null), "{text}");
    assert_eq!(r["read_after"]["data"]["speed"], 25, "{text}");
    assert_eq!(r["rpc"]["data"], json!({"oilLevel": 2.5}), "{text}");

    let owner = AccessLogOwner::Server(sid.as_u32());
    let requests: Vec<_> = state
        .list_access_logs_for(Some(owner), None)
        .await
        .into_iter()
        .map(|e| e.request)
        .collect();
    assert!(
        requests.iter().any(|q| q["method"] == "PATCH"
            && q["path"] == "car:"
            && q["body"] == json!({"speed": 25})),
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|q| q["method"] == "GET"
            && q["target"] == json!([{"name": "tire", "module": "car", "keys": ["1"]}])),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|q| q["query"]["depth"] == "1" && q["query"]["content"] == "config"),
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|q| q["operation"] == "car:addOil"
            && q["input"] == json!({"drainFirst": true, "amount": 2.5})),
        "{requests:?}"
    );
    state.remove_server(sid).await;
}
