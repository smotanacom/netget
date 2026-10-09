//! NetGet's NETCONF client against the netconf 2.1.0 Python server — an independent
//! implementation on Paramiko — over base 1.0 and base 1.1. The client reads, writes, reads
//! the write back and closes, every step chosen by its own event handlers; the server's own
//! log of what it handled is the check. Fails, never skips, when the peer is absent.
use crate::helpers::netconf::*;
use netget::state::{AccessLogOwner, ClientStatus};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

struct PythonServer {
    child: tokio::process::Child,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    port: u16,
    host_key_sha256: String,
}

async fn python_server() -> PythonServer {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .args(["server", "admin", "secret"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("start the netconf 2.1.0 server");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("netconf server did not start")
        .unwrap()
        .expect("netconf server exited");
    let banner: Value = serde_json::from_str(&first).unwrap();
    let key =
        russh_keys::parse_public_key_base64(banner["host_key_b64"].as_str().unwrap()).unwrap();
    PythonServer {
        child,
        lines,
        port: banner["port"].as_u64().unwrap() as u16,
        host_key_sha256: format!("SHA256:{}", key.fingerprint()),
    }
}

fn read_write_read_close() -> Vec<Value> {
    vec![
        json!({"event_pattern":"netconf_connected","handler":{"type":"static","actions":[{"type":"netconf_rpc","operation":"get-config","source":"running"}]}}),
        json!({"event_pattern":"netconf_rpc_reply","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "op=e['operation']\n",
            "if op=='get-config' and 'changed' not in e.get('data_xml',''):\n",
            "    a=[{'type':'netconf_rpc','operation':'edit-config','target':'running','config_xml':'<demo xmlns=\"urn:netget:netconf-peer\"><label>changed by netget</label></demo>'}]\n",
            "elif op=='edit-config':\n",
            "    a=[{'type':'netconf_rpc','operation':'get','filter_xml':'<demo xmlns=\"urn:netget:netconf-peer\"/>'}]\n",
            "elif op=='get':\n",
            "    a=[{'type':'netconf_rpc','operation':'get-config','source':'running'}]\n",
            "elif op=='get-config':\n",
            "    a=[{'type':'netconf_rpc','operation':'close-session'}]\n",
            "else:\n",
            "    a=[]\n",
            "print(json.dumps({'actions':a}))\n"
        )}}),
    ]
}

async fn against_python_server(versions: &[&str], expected_base: &str) {
    let mut peer = python_server().await;
    let state = state();
    let params = json!({"username":"admin","password":"secret","host_key_sha256":peer.host_key_sha256,"base_versions":versions});
    let cid = client_in(
        &state,
        format!("127.0.0.1:{}", peer.port),
        read_write_read_close(),
        params,
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "netconf_connected", 1).await;
    assert_eq!(connected[0].request["base_version"], expected_base);
    let replies = logs(&state, owner, "netconf_rpc_reply", 5).await;
    let ops: Vec<_> = replies
        .iter()
        .map(|r| r.request["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        ops,
        [
            "get-config",
            "edit-config",
            "get",
            "get-config",
            "close-session"
        ]
    );
    let first = replies[0].request["data_xml"].as_str().unwrap();
    assert!(
        first.contains("fixture α") && first.contains("owner=\"fixture\""),
        "{first}"
    );
    assert_eq!(replies[1].request["ok"], true);
    assert!(replies[2].request["data_xml"]
        .as_str()
        .unwrap()
        .contains("changed by netget"));
    assert!(replies[3].request["data_xml"]
        .as_str()
        .unwrap()
        .contains("changed by netget"));
    assert_eq!(replies[4].request["ok"], true);
    // What the independent server says it handled, and on which framing.
    let mut handled = Vec::new();
    while handled.len() < 4 {
        let line = tokio::time::timeout(Duration::from_secs(10), peer.lines.next_line())
            .await
            .expect("server log stalled")
            .unwrap()
            .expect("server exited");
        handled.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    let seen: Vec<_> = handled.iter().map(|h| h["rpc"].as_str().unwrap()).collect();
    assert_eq!(seen, ["get-config", "edit-config", "get", "get-config"]);
    assert!(
        handled.iter().all(|h| h["base"] == expected_base),
        "{handled:?}"
    );
    assert!(
        wait_until(Duration::from_secs(10), || async {
            state
                .get_client(cid)
                .await
                .is_none_or(|c| c.status == ClientStatus::Disconnected)
        })
        .await,
        "close-session did not end the client"
    );
    drop(peer.child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(5), peer.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reads_writes_and_closes_against_netconf_210_over_base_11() {
    against_python_server(&["1.0", "1.1"], "1.1").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reads_writes_and_closes_against_netconf_210_over_base_10() {
    against_python_server(&["1.0"], "1.0").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_refuses_the_independent_server_when_the_host_key_pin_differs() {
    let peer = python_server().await;
    let state = state();
    let wrong = host_key();
    let params = json!({"username":"admin","password":"secret","host_key_sha256":wrong.sha256});
    let outcome = client_in(&state, format!("127.0.0.1:{}", peer.port), vec![], params).await;
    assert!(
        outcome.is_err(),
        "a mismatched host key must fail the handshake"
    );
    drop(peer);
}
