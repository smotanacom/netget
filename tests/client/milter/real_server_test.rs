//! NetGet's Milter client against independent filters, failing rather than skipping when absent:
//! a pymilter filter (Sendmail's libmilter, through its Python binding) and emersion/go-milter's
//! server. Each must see the transaction the client's handlers drive and answer it — the
//! modifications that come back are the filter's own.
use super::session_test::{chain_handlers, client, events, executed, send, wait_for};
use crate::helpers::real_server::{InstallHint, RealServer};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/milter/install_peers.py <root> and export what it prints")
    })
}

async fn drive(filter: &RealServer, tag: &str, subject: &str, protocol: serde_json::Value) {
    let (state, id) = client(filter.addr(), chain_handlers()).await;
    for stage in ["connect", "helo", "mail", "rcpt"] {
        let r = wait_for(&state, id, stage).await;
        assert_eq!(r["decision"], "continue", "{stage}: {r}\n{}", filter.log());
    }
    let m = wait_for(&state, id, "message").await;
    assert_eq!(m["decision"], "accept", "{m}\n{}", filter.log());
    let negotiated = events(&state, id, "milter_negotiated").await;
    assert_eq!(negotiated[0]["protocol"], protocol, "{negotiated:?}");
    let mut mods = m["modifications"].as_array().unwrap().clone();
    mods.sort_by_key(|v| v["kind"].as_str().unwrap().to_string());
    assert_eq!(
        mods,
        vec![
            json!({"kind":"add_header","name":tag,"value":"seen"}),
            json!({"kind":"add_rcpt","recipient":"<audit@example.com>"}),
            json!({"kind":"change_header","index":1,"name":"Subject","value":subject}),
        ],
        "{}",
        filter.log()
    );
    // A second transaction: a refused recipient, abandoned, then a refused sender.
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_mail","sender":"<alice@example.com>"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "continue", "{r}");
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_rcpt","recipient":"<spam@example.net>"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "reject", "{r}");
    assert!(matches!(
        send(&state, id, json!({"type":"milter_abort"})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let r = executed(
        send(
            &state,
            id,
            json!({"type":"milter_mail","sender":"<spammer@bad.example>"}),
        )
        .await,
    );
    assert_eq!(r["decision"], "replycode", "{r}");
    assert!(r["text"].as_str().unwrap().starts_with("550 5.7.1 "), "{r}");
}

#[tokio::test]
async fn netget_against_pymilter() {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/client/milter/pymilter_filter.py"
    );
    let python = env_path("NETGET_MILTER_PYTHON");
    let filter = RealServer::builder(
        python.to_str().unwrap(),
        InstallHint {
            brew: "pymilter (pip install pymilter)",
            apt: "python3-milter",
        },
    )
    .args([script, "{port}"])
    .ready_when_log_matches("pymilter filter starting")
    .startup_timeout(Duration::from_secs(30))
    .start()
    .await
    .expect("start the pymilter filter");
    // libmilter leaves out every stage the filter has no callback for (body, end of headers,
    // DATA) and takes SKIP whenever it is offered: the client must honour all four.
    drive(
        &filter,
        "X-Py-Milter",
        "[py] hello",
        json!(["no_body", "no_eoh", "no_data", "skip"]),
    )
    .await;
}

#[tokio::test]
async fn netget_against_go_milter() {
    let peer = env_path("NETGET_MILTER_GO_PEER");
    let filter = RealServer::builder(
        peer.to_str().unwrap(),
        InstallHint {
            brew: "go (then tests/server/milter/install_peers.py)",
            apt: "golang (then tests/server/milter/install_peers.py)",
        },
    )
    .args(["server", "127.0.0.1:{port}"])
    .ready_when_log_matches("READY")
    .startup_timeout(Duration::from_secs(30))
    .start()
    .await
    .expect("start the go-milter filter");
    drive(&filter, "X-Go-Milter", "[go] hello", json!([])).await;
}
