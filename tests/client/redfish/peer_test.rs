//! NetGet's Redfish client against DMTF's Redfish-Mockup-Server 1.3.0 — an independent
//! service, unchanged, serving the public-rackmount1 mockup: session login (the mockup answers
//! 204 with X-Auth-Token and Location), the systems collection, a system, a PATCH, a reset
//! action whose target is read from the system's Actions, and a missing resource. Fails, never
//! skips.
use crate::helpers::redfish::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_logs_in_reads_patches_and_resets_against_dmtf_mockup() {
    let mockup = start_mockup().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        mockup.addr(),
        json!({"scheme": "http", "username": "root", "password": "calvin"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "redfish_connected", 1).await;
    assert_eq!(connected[0].request["authenticated"], "session");
    assert_eq!(
        connected[0].request["links"]["Chassis"],
        "/redfish/v1/Chassis"
    );
    for a in [
        json!({"type":"redfish_get","path":"/redfish/v1/Systems"}),
        json!({"type":"redfish_get","path":"/redfish/v1/Systems/437XR1138R2"}),
        json!({"type":"redfish_patch","path":"/redfish/v1/Systems/437XR1138R2","body":{"AssetTag":"netget-mock"}}),
        json!({"type":"redfish_action","resource_path":"/redfish/v1/Systems/437XR1138R2","action":"ComputerSystem.Reset","parameters":{"ResetType":"ForceRestart"}}),
        json!({"type":"redfish_get","path":"/redfish/v1/Systems/437XR1138R2"}),
        json!({"type":"redfish_get","path":"/redfish/v1/NoSuchThing"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(20))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "redfish_response", 6).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        r[0]["body"]["Members"][0]["@odata.id"],
        "/redfish/v1/Systems/437XR1138R2"
    );
    assert_eq!(
        (
            r[1]["status"].as_u64(),
            r[1]["body"]["@odata.type"]
                .as_str()
                .map(|t| t.starts_with("#ComputerSystem."))
        ),
        (Some(200), Some(true))
    );
    assert!(
        matches!(r[2]["status"].as_u64(), Some(200 | 204)),
        "{}",
        r[2]
    );
    assert_eq!(
        (r[3]["path"].as_str(), r[3]["status"].as_u64()),
        (
            Some("/redfish/v1/Systems/437XR1138R2/Actions/ComputerSystem.Reset"),
            Some(204)
        )
    );
    assert_eq!(
        r[4]["body"]["AssetTag"], "netget-mock",
        "the mockup applied the PATCH"
    );
    assert_eq!(r[5]["status"], 404);
    mockup
        .wait_for_log("ComputerSystem.Reset", Duration::from_secs(5))
        .await
        .unwrap();
    state.remove_client(cid).await;
}
