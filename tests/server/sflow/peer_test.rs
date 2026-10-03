use super::e2e_test::{logs, start};
use serde_json::json;
use std::time::Duration;

use crate::helpers::sflow::{peer, peer_golden, Collector};
#[tokio::test]
async fn unmodified_public_encoder_actual_wire_matches_literal_and_typed_collector() {
    let (state, id, addr, _) = start(None, None).await;
    let root = tempfile::Builder::new()
        .prefix("netget-sflow-writer-")
        .tempdir()
        .unwrap();
    let prefix = root.path().join("wire");
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(peer())
            .args(["emit", &addr.to_string(), prefix.to_str().unwrap()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut golden = peer_golden();
    assert_eq!(std::fs::read(prefix.with_extension("0")).unwrap(), golden);
    golden[16..20].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(std::fs::read(prefix.with_extension("1")).unwrap(), golden);
    let e = logs(&state, id, "sflow_message", 2).await;
    for (index, sequence) in [u32::MAX, 0].into_iter().enumerate() {
        let m = &e[index].request["message"];
        assert_eq!(m["sequence_number"], sequence);
        assert_eq!(m["agent_address"], "192.0.2.1");
        assert_eq!(m["sub_agent_id"], 77);
        assert_eq!(m["record_count"], 4);
        assert_eq!(m["samples"][0]["source"], json!({"class":2,"index":3}));
        assert_eq!(
            m["samples"][0]["records"][0]["packet"],
            json!({"packet_length":64,"protocol":17,"source_ip":"192.0.2.2","destination_ip":"198.51.100.2","source_port":53,"destination_port":123,"tcp_flags":0,"traffic_class":16})
        );
        assert_eq!(m["samples"][0]["records"][0]["header_length"], 28);
        assert_eq!(m["samples"][0]["records"][0]["decode_status"], "decoded");
        let counters = &m["samples"][1]["records"];
        assert_eq!(counters[0]["in_octets"], 9007199254740999u64);
        assert_eq!(counters[0]["out_octets"], u64::MAX);
        assert_eq!(counters[0]["promiscuous_mode"], 2);
        assert_eq!(counters[1]["alignment_errors"], 1);
        assert_eq!(counters[1]["symbol_errors"], 13);
        assert!(!serde_json::to_string(m).unwrap().contains("header_data"));
    }
    assert_eq!(
        e[1].request["message"]["sequence_tracking"]["status"],
        "in_order"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn unmodified_exporter_compensation_is_verified_by_literal_wire_and_goflow2_service() {
    let mut service = Collector::start().await;
    let root = tempfile::Builder::new()
        .prefix("netget-sflow-peer-cross-")
        .tempdir()
        .unwrap();
    let prefix = root.path().join("wire");
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(peer())
            .args([
                "emit",
                &format!("127.0.0.1:{}", service.port),
                prefix.to_str().unwrap(),
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(prefix.with_extension("0")).unwrap(),
        peer_golden()
    );
    let messages = service.messages(2).await;
    for (seq, row) in [u32::MAX, 0].into_iter().zip(messages.iter()) {
        let m = &row["message"];
        assert_eq!(row["type"], "sflow");
        assert_eq!(m["sequence-number"], seq);
        assert_eq!(m["agent-ip"], "192.0.2.1");
        assert_eq!(m["samples-count"], 2);
        for s in m["samples"].as_array().unwrap() {
            assert_eq!(s["header"]["source-id-type"], 2);
            assert_eq!(s["header"]["source-id-value"], 3);
        }
        assert_eq!(m["samples"][0]["records"][0]["header"]["length"], 44);
        assert_eq!(m["samples"][0]["records"][0]["data"]["original-length"], 28);
        assert_eq!(
            m["samples"][0]["records"][1]["data"],
            json!({"src-vlan":42,"src-priority":3,"dst-vlan":43,"dst-priority":4})
        );
        let c = &m["samples"][1]["records"][0]["data"];
        assert_eq!(c["if-in-octets"], 9007199254740999u64);
        assert_eq!(c["if-out-octets"], u64::MAX);
        assert_eq!(c["if-promiscuous-mode"], 2);
        assert_eq!(
            m["samples"][1]["records"][1]["data"]["dot3-stats-symbol-errors"],
            13
        );
    }
    service.stop().await;
}
