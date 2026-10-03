use super::e2e_test::{send, start};
use crate::helpers::netflow_v9::{decoded_values, Collector};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
#[tokio::test]
async fn required_goflow2_service_decodes_native_ipv4_ipv6_large_counters_uptime_and_options() {
    let mut service = Collector::start().await;
    let (state, id) = start(format!("127.0.0.1:{}", service.port), None, None).await;
    let mut b = json!({"source_id":42,"export_time":1700000000,"sys_uptime_ms":123456,
 "templates":[{"id":256,"fields":[{"element":"source_ipv4"},{"element":"destination_ipv4"},{"element":"in_bytes","length":8},{"element":"tcp_flags"},{"element":"first_switched"},{"element":"last_switched"}]},
 {"id":257,"fields":[{"element":"source_ipv6"},{"element":"destination_ipv6"},{"element":"in_packets"}]},
 {"id":258,"scope_count":1,"fields":[{"scope":"interface"},{"element":"sampling_interval"},{"element":"sampling_algorithm"}]}],
 "data_sets":[{"template_id":256,"records":[[{"kind":"ipv4","value":"192.0.2.1"},{"kind":"ipv4","value":"198.51.100.2"},{"kind":"unsigned","value":9007199254740999u64},{"kind":"unsigned","value":255},{"kind":"uptime_milliseconds","value":0},{"kind":"uptime_milliseconds","value":u32::MAX}]]},
 {"template_id":257,"records":[[{"kind":"ipv6","value":"2001:db8::1"},{"kind":"ipv6","value":"2001:db8::2"},{"kind":"unsigned","value":7}]]},
 {"template_id":258,"records":[[{"kind":"unsigned","value":9},{"kind":"unsigned","value":100},{"kind":"unsigned","value":1}]]}]});
    let records = b["data_sets"][0]["records"].as_array_mut().unwrap();
    records.push(records[0].clone());
    for _ in 0..2 {
        assert!(matches!(
            send(&state, id, b.clone()).await,
            ClientSendOutcome::Executed { .. }
        ));
    }
    let rows = service.messages(42, 2).await;
    for sequence in [0, 1] {
        let row = rows
            .iter()
            .find(|r| r["message"]["sequence-number"] == sequence)
            .unwrap();
        assert_eq!(row["type"], "netflowv9");
        let m = &row["message"];
        assert_eq!(m["count"], 7);
        assert_eq!(m["system-uptime"], 123456);
        assert_eq!(m["unix-seconds"], 1700000000);
        let sets = m["flow-sets"].as_array().unwrap();
        assert_eq!(sets.len(), 6);
        assert_eq!(
            decoded_values(&sets[3]["records"][0]),
            vec![
                (8, vec![192, 0, 2, 1]),
                (12, vec![198, 51, 100, 2]),
                (1, 9007199254740999u64.to_be_bytes().to_vec()),
                (6, vec![255]),
                (22, 0u32.to_be_bytes().to_vec()),
                (21, u32::MAX.to_be_bytes().to_vec())
            ]
        );
        assert_eq!(
            decoded_values(&sets[4]["records"][0]),
            vec![
                (
                    27,
                    "2001:db8::1"
                        .parse::<std::net::Ipv6Addr>()
                        .unwrap()
                        .octets()
                        .to_vec()
                ),
                (
                    28,
                    "2001:db8::2"
                        .parse::<std::net::Ipv6Addr>()
                        .unwrap()
                        .octets()
                        .to_vec()
                ),
                (2, 7u32.to_be_bytes().to_vec())
            ]
        );
        assert_eq!(sets[3]["records"].as_array().unwrap().len(), 2);
        assert_eq!(
            decoded_values(&sets[3]["records"][0]),
            decoded_values(&sets[3]["records"][1])
        );
        let options = &sets[2]["records"][0];
        assert_eq!(options["scope-length"], 4);
        assert_eq!(options["option-length"], 8);
        let r = &sets[5]["records"][0];
        assert_eq!(r["scope-values"][0]["type"], 2);
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(r["scope-values"][0]["value"].as_str().unwrap())
            .unwrap();
        assert_eq!(raw, 9u32.to_be_bytes());
        let opts = &r["option-values"];
        assert_eq!(opts[0]["type"], 34);
        assert_eq!(opts[0]["value"], "AAAAZA==");
        assert_eq!(opts[1]["type"], 35);
        assert_eq!(opts[1]["value"], "AQ==");
    }
    state.remove_client(id).await;
    service.stop().await;
}
