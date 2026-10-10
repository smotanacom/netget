//! Independent ESMEs against NetGet's SMSC, failing rather than skipping when absent: Python
//! smpplib 2.2.4 and linxGnu/gosmpp v0.3.1 (Go, through `tests/client/smpp/peer`). Each binds
//! with the configured credentials, submits an ASCII message with a receipt, a UCS-2 one and
//! one the handler rejects, and must read the responses, the receipt and both replies.
//! `tests/client/smpp/install_peers.py` prints NETGET_SMPP_PYTHON and NETGET_SMPP_GO_PEER.
use super::wire_test::{credentials, handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/smpp/install_peers.py <root> and export what it prints"))
}

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(&program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

fn sorted(v: &Value) -> Vec<(u64, String)> {
    let mut items: Vec<(u64, String)> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["status"].as_u64().unwrap(),
                x["message_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    items.sort();
    items
}

#[tokio::test]
async fn smpplib_esme() {
    let (state, id, addr) = start(handlers(), credentials()).await;
    let out = run(
        env_path("NETGET_SMPP_PYTHON"),
        &[
            "-I",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/client/smpp/smpplib_peer.py"
            ),
            &addr.port().to_string(),
            "esme1",
            "secret",
        ],
    )
    .await;
    assert_eq!(
        out["responses"],
        json!([{"status": 0, "message_id": "NG00000001"}, {"status": 0, "message_id": "NG00000002"}]),
        "{out}"
    );
    assert_eq!(out["errors"], json!([11]), "ESME_RINVDSTADR: {out}");
    let delivered = out["delivered"].as_array().unwrap();
    assert_eq!(delivered.len(), 3, "{out}");
    let receipt = delivered
        .iter()
        .find(|d| d["esm_class"] == 4)
        .expect("a receipt");
    assert!(
        receipt["text"]
            .as_str()
            .unwrap()
            .starts_with("id:NG00000001 sub:001 dlvrd:001"),
        "{receipt}"
    );
    let replies: Vec<&str> = delivered
        .iter()
        .filter(|d| d["esm_class"] == 0)
        .map(|d| d["text"].as_str().unwrap())
        .collect();
    assert_eq!(replies, ["Got: hello from smpplib", "Got: Привет"]);
    state.remove_server(id).await;
}

#[tokio::test]
async fn gosmpp_esme() {
    let (state, id, addr) = start(handlers(), credentials()).await;
    let out = run(
        env_path("NETGET_SMPP_GO_PEER"),
        &["gosmpp", &addr.to_string(), "esme1", "secret"],
    )
    .await;
    assert!(
        out.get("receiving_error").is_none() && out.get("submit_error").is_none(),
        "{out}"
    );
    assert_eq!(
        sorted(&out["responses"]),
        sorted(
            &json!([{"status": 0, "message_id": "NG00000001"}, {"status": 0, "message_id": "NG00000002"}, {"status": 11, "message_id": ""}])
        ),
        "{out}"
    );
    let delivered = out["delivered"].as_array().unwrap();
    assert_eq!(delivered.len(), 3, "{out}");
    assert!(
        delivered.iter().all(|d| d["acked"] == true),
        "gosmpp answered every deliver_sm"
    );
    assert!(
        delivered
            .iter()
            .any(|d| d["esm_class"] == 4 && d["text"].as_str().unwrap().contains("stat:DELIVRD")),
        "{out}"
    );
    assert!(
        delivered.iter().any(|d| d["text"] == "Got: Привет"),
        "{out}"
    );
    assert!(
        delivered
            .iter()
            .any(|d| d["text"] == "Got: hello from gosmpp"),
        "{out}"
    );
    state.remove_server(id).await;
}
