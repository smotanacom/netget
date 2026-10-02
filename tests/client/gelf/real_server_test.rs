use super::e2e_test::{send, start};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
#[tokio::test]
async fn pinned_official_graylog_readers_decode_both_transports_all_compressions_and_chunks() {
    for (transport, compression) in [
        ("udp", "none"),
        ("udp", "gzip"),
        ("udp", "zlib"),
        ("tcp", "none"),
    ] {
        let binary = std::env::var("NETGET_GELF_READER")
            .expect("NETGET_GELF_READER missing; run tests/server/gelf/install_peers.py");
        let mut child = tokio::process::Command::new(binary)
            .args([transport, "2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("official Graylog peer executable");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let marker = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("official peer exited before bind");
        let addr = marker
            .strip_prefix("NETGET_ADDR ")
            .expect("peer bind marker");
        let (state, id) = start(
            addr.into(),
            json!({"type":"static","actions":[]}),
            json!({"transport":transport,"compression":compression}),
        )
        .await;
        for short in [
            "independent 温度".to_string(),
            "chunked abcdefghijklmnopqrstuvwxyz ".repeat(150),
        ] {
            assert!(matches!(send(&state,id,json!({"host":"netget","short_message":short,"full_message":"line\nnext","timestamp":1700000000.25,"level":6,"additional_fields":{"service":"api","count":42}})).await,ClientSendOutcome::Sent{..}));
            let marker = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .expect("official reader observation");
            let got: Value = serde_json::from_str(
                marker
                    .strip_prefix("NETGET_MESSAGE ")
                    .expect("observation marker"),
            )
            .unwrap();
            assert_eq!(got["version"], "1.1");
            assert_eq!(got["host"], "netget");
            assert_eq!(got["short_message"], short);
            assert_eq!(got["timestamp"], 1700000000.25);
            assert_eq!(got["level"], 6);
            assert_eq!(got["full_message"], "line\nnext");
            assert_eq!(
                got["additional_fields"],
                json!({"_service":"api","_count":42})
            );
        }
        state.remove_client(id).await;
        let result = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(result.success());
    }
}
