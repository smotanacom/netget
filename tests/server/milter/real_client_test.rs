//! Independent MTAs against NetGet's filter, failing rather than skipping when absent: OpenDKIM's
//! miltertest (C, driven by `miltertest.lua`) and emersion/go-milter's client. Peers from
//! `install_peers.py`.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

pub fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/milter/install_peers.py <root> and export what it prints")
    })
}

async fn run(bin: PathBuf, args: &[&str]) -> (bool, String, String) {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&bin)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("{} deadline", bin.display()))
    .unwrap_or_else(|e| panic!("{}: {e}", bin.display()));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn miltertest() {
    let (_state, _id, addr) = start(handlers()).await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/milter/miltertest.lua"
    );
    let port = format!("port={}", addr.port());
    let (ok, out, err) = run(env_path("NETGET_MILTERTEST"), &["-D", &port, "-s", script]).await;
    assert!(ok, "miltertest failed\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("all checks passed"), "{out}\n{err}");
}

#[tokio::test]
async fn go_milter_client() {
    let (_state, _id, addr) = start(handlers()).await;
    let a = addr.to_string();
    let (ok, out, err) = run(env_path("NETGET_MILTER_GO_PEER"), &["client", &a]).await;
    assert!(
        ok,
        "go-milter client failed\nstdout:\n{out}\nstderr:\n{err}"
    );
    let got: Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(
        got,
        json!({
            "connect": "continue",
            "helo": "continue",
            "mail": "continue",
            "rcpt_spam": "reject",
            "rcpt": "continue",
            "header": "continue",
            "eoh": "continue",
            "final": "accept",
            "modifications": [
                "add_header X-NetGet: checked",
                "change_header Subject[1]: [netget] hello",
                "add_rcpt <audit@example.com>",
            ],
            "mail_spammer": "reply 550 5.7.1 Sender rejected by policy",
        })
    );
}
