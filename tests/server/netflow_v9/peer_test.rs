use super::e2e_test::{logs, start};
use crate::helpers::netflow_v9::{decoded_values, export, peer_golden, Collector};
use serde_json::json;
#[tokio::test]
async fn required_unmodified_exporter_actual_wire_matches_literal_and_native_typed_values() {
    let (state, id, addr) = start(None, None).await;
    let mut wire = export(addr, false).await;
    let epoch = u32::from_be_bytes(wire[8..12].try_into().unwrap());
    assert!(epoch > 1700000000);
    wire[8..12].fill(0);
    assert_eq!(wire, peer_golden());
    let e = logs(&state, id, "netflow_v9_message", 1).await;
    let m = &e[0].request["message"];
    assert_eq!(m["header_count"], 3);
    assert_eq!(m["known_total_record_count"], 3);
    assert_eq!(m["source_id"], 0);
    assert_eq!(m["sequence_number"], 1);
    assert_eq!(m["sys_uptime_ms"], 200);
    assert_eq!(m["export_time"], epoch);
    assert_eq!(m["count_status"], "validated");
    assert_eq!(m["template_changes"].as_array().unwrap().len(), 2);
    assert_eq!(m["record_count"], 1);
    assert_eq!(
        m["data_sets"][0]["records"][0],
        json!([
 {"kind":"ipv4","value":"192.0.2.1"},{"kind":"ipv4","value":"198.51.100.2"},
 {"kind":"uptime_milliseconds","value":200},{"kind":"uptime_milliseconds","value":0},
 {"kind":"unsigned","value":84},{"kind":"unsigned","value":3},
 {"kind":"unsigned","value":0},{"kind":"unsigned","value":0},
 {"kind":"unsigned","value":12345},{"kind":"unsigned","value":2055},
 {"kind":"unsigned","value":17},{"kind":"unsigned","value":0},
 {"kind":"unsigned","value":4},{"kind":"unsigned","value":0},
 {"kind":"unsigned","value":0},{"kind":"unsigned","value":0}])
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn required_unmodified_exporter_options_use_v9_scope_byte_lengths_and_total_count() {
    let (state, id, addr) = start(None, None).await;
    let w = export(addr, true).await;
    assert_eq!(u16::from_be_bytes(w[2..4].try_into().unwrap()), 5);
    let e = logs(&state, id, "netflow_v9_message", 1).await;
    let m = &e[0].request["message"];
    assert_eq!(m["header_count"], 5);
    assert_eq!(m["record_count"], 2);
    assert_eq!(m["template_changes"].as_array().unwrap().len(), 3);
    let t = &m["data_sets"][0]["template"];
    assert_eq!(t["scope_count"], 1);
    assert_eq!(t["fields"][0]["scope"], "interface");
    assert!(t["fields"][0]["element"].is_null());
    assert_eq!(
        m["data_sets"][0]["records"][0],
        json!([{"kind":"unsigned","value":0},{"kind":"unsigned","value":2},{"kind":"unsigned","value":1}])
    );
    assert_eq!(
        m["data_sets"][1]["records"][0][4],
        json!({"kind":"unsigned","value":56})
    );
    assert_eq!(
        m["data_sets"][1]["records"][0][5],
        json!({"kind":"unsigned","value":2})
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn actual_independent_exporter_wire_is_cross_decoded_by_goflow2_service() {
    let mut service = Collector::start().await;
    let mut w = export(
        format!("127.0.0.1:{}", service.port).parse().unwrap(),
        false,
    )
    .await;
    w[8..12].fill(0);
    assert_eq!(w, peer_golden());
    let rows = service.messages(0, 1).await;
    let row = &rows[0];
    assert_eq!(row["type"], "netflowv9");
    let m = &row["message"];
    assert_eq!(m["count"], 3);
    assert_eq!(m["sequence-number"], 1);
    assert_eq!(m["system-uptime"], 200);
    let sets = m["flow-sets"].as_array().unwrap();
    assert_eq!(sets[0]["records"][0]["field-count"], 16);
    assert_eq!(sets[1]["records"][0]["template-id"], 2048);
    assert_eq!(
        decoded_values(&sets[2]["records"][0]),
        vec![
            (8, vec![192, 0, 2, 1]),
            (12, vec![198, 51, 100, 2]),
            (21, 200u32.to_be_bytes().to_vec()),
            (22, 0u32.to_be_bytes().to_vec()),
            (1, 84u32.to_be_bytes().to_vec()),
            (2, 3u32.to_be_bytes().to_vec()),
            (10, 0u32.to_be_bytes().to_vec()),
            (14, 0u32.to_be_bytes().to_vec()),
            (7, 12345u16.to_be_bytes().to_vec()),
            (11, 2055u16.to_be_bytes().to_vec()),
            (4, vec![17]),
            (6, vec![0]),
            (60, vec![4]),
            (5, vec![0]),
            (32, vec![0, 0]),
            (58, vec![0, 0])
        ]
    );
    service.stop().await;
}
