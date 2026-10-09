//! Independent RCON clients against NetGet's server, each failing rather than skipping when
//! absent: gorcon/rcon v1.4.0's client (Go, through `tests/client/rcon/peer`) and the Python
//! rcon package 2.4.9. `tests/client/rcon/install_peers.py` builds and installs both and
//! prints NETGET_RCON_PEER and NETGET_RCON_PYTHON.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{var} is required: run tests/client/rcon/install_peers.py <root> and export what it prints"))
}

async fn run(program: PathBuf, args: &[&str]) -> String {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
}

#[tokio::test]
async fn gorcon_client_logs_in_and_runs_commands() {
    let (state, id, addr) = start(handlers(), json!({"password":"hunter2"})).await;
    let peer = env_path("NETGET_RCON_PEER");
    let out: Value = serde_json::from_str(
        run(
            peer.clone(),
            &["client", &addr.to_string(), "hunter2", "players", "say hi"],
        )
        .await
        .trim(),
    )
    .unwrap();
    assert_eq!(
        out["responses"],
        json!(["ran: players", "ran: say hi"]),
        "{out}"
    );
    assert!(out.get("error").is_none(), "{out}");
    let refused: Value = serde_json::from_str(
        run(peer, &["client", &addr.to_string(), "wrong", "players"])
            .await
            .trim(),
    )
    .unwrap();
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("authentication failed")),
        "{refused}"
    );
    state.remove_server(id).await;
}

const PYTHON_CLIENT: &str = "import json, sys\nfrom rcon.source import Client\nfrom rcon.exceptions import WrongPassword\nport = int(sys.argv[1])\nout = {}\nwith Client('127.0.0.1', port, passwd='hunter2') as c:\n    out['status'] = c.run('status')\n    out['say'] = c.run('say', 'hello', 'world')\ntry:\n    with Client('127.0.0.1', port, passwd='wrong') as c:\n        c.run('status')\n    out['refused'] = False\nexcept WrongPassword:\n    out['refused'] = True\nprint(json.dumps(out))";

/// The Python rcon package reads each packet through a fresh buffered `socket.makefile()`, so
/// when two packets arrive in one TCP read (srcds' empty response and the AUTH_RESPONSE) it
/// discards the second and waits forever. It can only talk reliably to a server that answers
/// with one packet at a time, which is the minecraft dialect.
#[tokio::test]
async fn python_rcon_logs_in_and_runs_commands() {
    let (state, id, addr) = start(
        handlers(),
        json!({"password":"hunter2","dialect":"minecraft"}),
    )
    .await;
    let python = env_path("NETGET_RCON_PYTHON");
    let raw = run(
        python,
        &["-I", "-c", PYTHON_CLIENT, &addr.port().to_string()],
    )
    .await;
    let out: Value = serde_json::from_str(raw.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {raw}"));
    assert_eq!(
        out,
        json!({"status":"ran: status","say":"ran: say hello world","refused":true})
    );
    state.remove_server(id).await;
}
