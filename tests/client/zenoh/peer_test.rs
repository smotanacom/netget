//! NetGet's Zenoh client, as a peer, against zenoh-pico 1.10.1 peers (C, independent,
//! unchanged) listening on their own ports: a pico subscriber receives what NetGet publishes, a
//! pico queryable answers NetGet's get, and NetGet's subscriber receives a pico publisher's
//! values. Fails, never skips.
use crate::helpers::zenoh::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_against_zenoh_pico_peers() {
    let state = state();

    let sub = pico_peer("z_sub", &["-k", "demo/from-netget", "-n", "1"])
        .await
        .unwrap();
    let c1 = client_in(&state, sub.addr(), json!({"mode": "peer"}))
        .await
        .unwrap();
    let connected = wait_for(
        &state,
        AccessLogOwner::Client(c1.as_u32()),
        "zenoh_connected",
        |_| true,
    )
    .await;
    assert_eq!(
        connected["links"][0]["dst"],
        format!("tcp/{}", sub.addr()),
        "{connected}"
    );
    // Repeat until the pico subscriber has it (its declaration may arrive after the first put).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !sub.log().contains("demo/from-netget") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pico never received NetGet's put: {}",
            sub.log()
        );
        let r = state
            .send_to_client(
                c1,
                json!({"type": "zenoh_put", "key": "demo/from-netget", "payload": "hi pico"}),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{r:?}");
        let _ = sub
            .wait_for_log("demo/from-netget", Duration::from_millis(500))
            .await;
    }
    assert!(
        sub.log().contains("('demo/from-netget': 'hi pico')"),
        "{}",
        sub.log()
    );

    let q = pico_peer("z_queryable", &["-k", "demo/pq", "-v", "pico-answer"])
        .await
        .unwrap();
    let c2 = client_in(&state, q.addr(), json!({"mode": "peer"}))
        .await
        .unwrap();
    let o2 = AccessLogOwner::Client(c2.as_u32());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let result = loop {
        let r = state
            .send_to_client(
                c2,
                json!({"type": "zenoh_get", "selector": "demo/pq"}),
                Duration::from_secs(20),
            )
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{r:?}");
        let got = wait_for(&state, o2, "zenoh_get_result", |_| true).await;
        if got["replies"].as_array().is_some_and(|r| !r.is_empty())
            || tokio::time::Instant::now() > deadline
        {
            break got;
        }
    };
    assert_eq!(
        (
            result["replies"][0]["key"].as_str(),
            result["replies"][0]["payload"].as_str()
        ),
        (Some("demo/pq"), Some("pico-answer")),
        "{result}"
    );

    let publisher = pico_peer("z_pub", &["-k", "demo/ticks", "-v", "tick", "-n", "60"])
        .await
        .unwrap();
    let c3 = client_in(
        &state,
        publisher.addr(),
        json!({"mode": "peer", "subscribe": ["demo/**"]}),
    )
    .await
    .unwrap();
    let sample = wait_for(
        &state,
        AccessLogOwner::Client(c3.as_u32()),
        "zenoh_sample",
        |e| e["key"] == "demo/ticks",
    )
    .await;
    assert!(
        sample["payload"].as_str().unwrap().ends_with("] tick"),
        "{sample}"
    );

    for c in [c1, c2, c3] {
        state.remove_client(c).await;
    }
}
