//! NetGet's LMTP client against an independent server: aiosmtpd 1.4.6's LMTP class, unchanged,
//! driven by `peer.py`. Fails rather than skips when aiosmtpd is missing; run
//! `install_peers.py` and set NETGET_LMTP_PYTHON, or install aiosmtpd for python3.
//!
//! The handler's message is asserted from the server's side of the wire (aiosmtpd's own record
//! of the envelope and content), and each per-recipient result from NetGet's side.
use super::session_test::{start, wait_log};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn python() -> PathBuf {
    std::env::var_os("NETGET_LMTP_PYTHON")
        .map(PathBuf::from)
        .or_else(|| crate::helpers::real_server::find_binary("python3"))
        .expect("python3 with aiosmtpd 1.4.6 is required: run tests/client/lmtp/install_peers.py and set NETGET_LMTP_PYTHON")
}

#[tokio::test]
async fn netget_delivers_to_aiosmtpd_lmtp() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record.jsonl");
    let peer = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/client/lmtp/peer.py");
    let mut child = tokio::process::Command::new(python())
        .args(["-I", peer, record.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the aiosmtpd peer");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut ready))
        .await
        .expect("aiosmtpd peer did not start")
        .unwrap();
    let port: u16 = match ready.trim().strip_prefix("READY ") {
        Some(port) => port.parse().unwrap(),
        None => {
            let output = child.wait_with_output().await.unwrap();
            panic!(
                "aiosmtpd peer failed (is aiosmtpd 1.4.6 installed?): {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let (state, id) = start(
        format!("127.0.0.1:{port}"),
        json!({"lhlo_domain":"netget.test"}),
        json!([{"type":"lmtp_send","from":"s@example.test",
            "to":["alice@example.test","nobody@example.test","full@example.test"],
            "subject":"Independent","body":"Hello aiosmtpd\n.dotted"}]),
    )
    .await;
    let result = wait_log(&state, id, "4.2.2 Mailbox full").await;
    assert!(
        result.contains("Delivered to alice@example.test"),
        "{result}"
    );
    assert!(result.contains("5.1.1 No such user here"), "{result}");
    assert!(
        result.contains(r#""delivered":["alice@example.test"]"#),
        "{result}"
    );
    let recorded = std::fs::read_to_string(&record).unwrap();
    let message: Value = serde_json::from_str(recorded.lines().next().unwrap()).unwrap();
    assert_eq!(message["mail_from"], "s@example.test");
    assert_eq!(
        message["rcpt_tos"],
        json!(["alice@example.test", "full@example.test"])
    );
    let content = message["content"].as_str().unwrap();
    assert!(content.contains("Subject: Independent\r\n"), "{content:?}");
    assert!(content.contains("@netget.test>"), "{content:?}");
    assert!(
        content.ends_with("\r\n\r\nHello aiosmtpd\r\n.dotted\r\n"),
        "aiosmtpd must have unstuffed the leading dot: {content:?}"
    );
    state.remove_client(id).await;
    child.kill().await.unwrap();
}
