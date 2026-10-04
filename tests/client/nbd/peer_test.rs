//! NetGet's NBD client against nbdkit 1.48.1 (C, independent, unchanged) serving its data
//! plugin behind its error filter: the export list, size and block sizes, reads of the plugin's
//! bytes, allocation from block status, EIO once the filter is armed, and flush. Fails, never
//! skips.
use crate::helpers::nbd::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_against_nbdkit() {
    let nbdkit = start_nbdkit().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        nbdkit.addr(),
        json!({"export": "disk", "list_exports": true}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let c = &logs(&state, owner, "nbd_connected", 1).await[0];
    assert_eq!(
        (
            c["size"].as_u64(),
            c["structured_replies"].as_bool(),
            c["base_allocation"].as_bool()
        ),
        (Some(1 << 20), Some(true), Some(true)),
        "{c}"
    );
    // The data plugin is name-agnostic: its list is the single default export.
    assert_eq!(
        c["exports"],
        json!([{"name": "", "description": ""}]),
        "{c}"
    );
    assert_eq!(c["block_size"]["preferred"], 32768, "{c}");
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for a in [
        json!({"type": "nbd_read", "offset": 0, "length": 12}),
        json!({"type": "nbd_read", "offset": 65536, "length": 4}),
        json!({"type": "nbd_read", "offset": 524288, "length": 65536}),
        json!({"type": "nbd_block_status", "offset": 0, "length": 1048576}),
        json!({"type": "nbd_flush"}),
    ] {
        assert!(
            matches!(
                send(a.clone()).await.unwrap(),
                ClientSendOutcome::Sent { .. }
            ),
            "{a}"
        );
    }
    let reads = logs(&state, owner, "nbd_read_result", 3).await;
    assert_eq!(reads[0]["data"], "hello nbdkit");
    assert_eq!(reads[1]["data"], "tail");
    assert_eq!(reads[2]["all_zero"], true, "{}", reads[2]);
    let status = &logs(&state, owner, "nbd_block_status_result", 1).await[0];
    let extents = status["extents"].as_array().unwrap();
    assert_eq!(
        (extents[0]["offset"].as_u64(), extents[0]["hole"].as_bool()),
        (Some(0), Some(false)),
        "{status}"
    );
    assert!(
        extents
            .iter()
            .any(|e| e["hole"] == true && e["zero"] == true),
        "{status}"
    );
    assert_eq!(
        logs(&state, owner, "nbd_flush_result", 1).await[0]["error"],
        "OK"
    );

    std::fs::write(nbdkit.dir().join("inject"), b"").unwrap();
    assert!(matches!(
        send(json!({"type": "nbd_read", "offset": 0, "length": 512}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let failed = &logs(&state, owner, "nbd_read_result", 4).await[3];
    assert_eq!(failed["error"], "EIO", "{failed}");
    assert!(matches!(
        send(json!({"type": "disconnect"})).await.unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(cid).await;
}
