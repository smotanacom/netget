use crate::helpers::gnmi_peer as peer;
use serde_json::json;
#[tokio::test]
async fn independent_generated_sdk_capabilities_get_set_and_all_subscription_modes() {
    let state = peer::state().await;
    let (id, port) = peer::server(&state, peer::handlers(), json!({}))
        .await
        .unwrap();
    assert_eq!(
        peer::client(port, "normal", None).await.unwrap()["operations"],
        json!([1, 2, 3])
    );
    for mode in ["once", "poll", "stream", "updates-only"] {
        let result = peer::client(port, mode, None).await.unwrap();
        assert!(result["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "sync_response"));
    }
    assert_eq!(peer::client(port, "error", None).await.unwrap()["code"], 7);
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(id.as_u32())),
            None,
        )
        .await;
    assert!(logs.iter().any(|l| l.event_type == "gnmi_set_request"
        && l.request["request"]["update"][0]["value"]["value"] == u64::MAX.to_string()));
    assert!(logs.iter().any(|l| l.event_type == "gnmi_poll_request"));
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn independent_gnmic_cli_capabilities_get_set_once_and_verified_tls() {
    let directory = tempfile::tempdir().unwrap();
    let (cert, key, ca) = peer::certificate(directory.path()).await.unwrap();
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        peer::handlers(),
        json!({"use_tls":true,"cert_file":cert,"key_file":key}),
    )
    .await
    .unwrap();
    for (scenario, args) in [
        vec!["capabilities"],
        vec!["get", "--path", "openconfig:/system[name=eth0]"],
        vec![
            "set",
            "--delete",
            "/old",
            "--replace",
            "/new:::string:::updated",
            "--update",
            "/counter:::uint:::18446744073709551615",
        ],
        vec![
            "subscribe",
            "--mode",
            "once",
            "--path",
            "openconfig:/system[name=eth0]",
            "--qos",
            "0",
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let (success, out, err) = peer::gnmic(port, &args, Some(&ca)).await.unwrap();
        assert!(success, "{out}{err}");
        let values: Vec<serde_json::Value> = serde_json::Deserializer::from_str(&out)
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        match scenario {
            0 => {
                assert_eq!(values[0]["gNMIVersion"], "0.10.0");
                assert!(values[0]["supportedEncodings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == "PROTO"));
            }
            1 => {
                assert_eq!(
                    values[0]["notification"][0]["update"][0]["val"]["uintVal"],
                    "42"
                );
                assert_eq!(
                    values[0]["notification"][0]["update"][0]["path"]["elem"][0]["key"]["name"],
                    "eth0"
                );
            }
            2 => {
                assert_eq!(
                    values[0]["response"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v["op"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    vec!["DELETE", "REPLACE", "UPDATE"]
                );
                assert_eq!(
                    values[0]["response"][2]["path"]["elem"][0]["name"],
                    "counter"
                );
            }
            3 => {
                assert_eq!(values.len(), 2);
                assert_eq!(values[0]["update"]["update"][0]["val"]["uintVal"], "42");
                assert_eq!(values[1]["syncResponse"], true);
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(
        peer::client(port, "normal", Some(&ca)).await.unwrap()["counter"],
        42
    );
    state.remove_server(id).await.unwrap();
}
