//! NetGet's BMP exporter against gobmp 1.1.0 (Go, independent, unchanged), which parses and
//! prints what it collects: a peer up with both OPENs, announcements with their attributes, a
//! withdrawal, statistics, a peer down with its NOTIFICATION, and termination. Fails, never
//! skips.
use crate::helpers::bmp::*;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn exporter_reports_to_gobmp() {
    let gobmp = start_gobmp().await.unwrap();
    let state = state();
    let cid = client_in(&state, gobmp.addr(), json!({"sys_name": "edge-7"}))
        .await
        .unwrap();
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for a in [
        json!({"type": "bmp_peer_up", "peer": {"address": "192.0.2.2", "asn": 65002, "bgp_id": "192.0.2.2"}, "local_address": "192.0.2.1", "local_asn": 65001, "local_bgp_id": "192.0.2.1", "remote_port": 40179}),
        json!({"type": "bmp_route_monitoring", "peer_address": "192.0.2.2", "announce": ["203.0.113.0/24", "198.51.100.0/25"], "next_hop": "192.0.2.2", "as_path": [65002, 65010], "origin": "EGP", "med": 20, "local_pref": 150, "communities": ["65002:100"]}),
        json!({"type": "bmp_route_monitoring", "peer_address": "192.0.2.2", "withdraw": ["198.51.100.0/25"]}),
        json!({"type": "bmp_statistics", "peer_address": "192.0.2.2", "counters": [{"type": "rejected_prefixes", "value": 3}, {"type": "adj_rib_in_routes", "value": 1}]}),
        json!({"type": "bmp_peer_down", "peer_address": "192.0.2.2", "reason": "remote_notification", "notification": {"code": 6, "subcode": 2}}),
    ] {
        let r = send(a.clone()).await.unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    // A route for a peer that is not up is refused before the wire.
    let r = send(json!({"type": "bmp_route_monitoring", "peer_address": "192.0.2.99", "withdraw": ["10.0.0.0/8"]})).await.unwrap();
    assert!(matches!(r, ClientSendOutcome::Rejected { .. }), "{r:?}");
    assert!(matches!(
        send(json!({"type": "bmp_termination", "message": "maintenance"}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));

    gobmp
        .wait_for_log("Termination message", Duration::from_secs(20))
        .await
        .unwrap();
    let log = gobmp.log();
    let messages = gobmp_messages(&log);
    let peer: Vec<&Value> = messages
        .iter()
        .filter(|(k, _)| *k == 10)
        .map(|(_, m)| m)
        .collect();
    let up = peer
        .iter()
        .find(|m| m["action"] == "add")
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(
        (
            up["remote_ip"].as_str(),
            up["remote_asn"].as_u64(),
            up["remote_bgp_id"].as_str(),
            up["local_asn"].as_u64(),
            up["local_bgp_id"].as_str(),
            up["remote_port"].as_u64()
        ),
        (
            Some("192.0.2.2"),
            Some(65002),
            Some("192.0.2.2"),
            Some(65001),
            Some("192.0.2.1"),
            Some(40179)
        ),
        "{up}"
    );
    let down = peer
        .iter()
        .find(|m| m["action"] == "down")
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(down["bmp_reason"], 3, "{down}");
    let prefixes: Vec<&Value> = messages
        .iter()
        .filter(|(k, _)| *k == 74)
        .map(|(_, m)| m)
        .collect();
    let announced = prefixes
        .iter()
        .find(|m| m["prefix"] == "203.0.113.0" && m["action"] == "add")
        .unwrap_or_else(|| panic!("{log}"));
    let attrs = &announced["base_attrs"];
    assert_eq!(
        (
            attrs["as_path"].clone(),
            attrs["origin"].as_str(),
            attrs["med"].as_u64(),
            attrs["local_pref"].as_u64(),
            attrs["community_list"].clone(),
            announced["nexthop"].as_str(),
            announced["prefix_len"].as_u64()
        ),
        (
            json!([65002, 65010]),
            Some("egp"),
            Some(20),
            Some(150),
            json!(["65002:100"]),
            Some("192.0.2.2"),
            Some(24)
        ),
        "{announced}"
    );
    assert!(
        prefixes.iter().any(|m| m["prefix"] == "198.51.100.0"
            && m["prefix_len"] == 25
            && m["action"] == "add"),
        "{log}"
    );
    assert!(
        prefixes
            .iter()
            .any(|m| m["prefix"] == "198.51.100.0" && m["action"] == "del"),
        "{log}"
    );
    let stats = messages
        .iter()
        .find(|(k, _)| *k == 1)
        .map(|(_, m)| m)
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(stats["remote_ip"], "192.0.2.2", "{stats}");
    assert_eq!(
        (
            stats["prefixes_rejected_inbound"].as_u64(),
            stats["ads_rib_in"].as_u64()
        ),
        (Some(3), Some(1)),
        "{stats}"
    );
    state.remove_client(cid).await;
}
