//! Independent MessagePack-RPC clients against NetGet's server, failing rather than skipping
//! when absent: Neovim (rpcrequest/rpcnotify over sockconnect, its own C msgpack-rpc) and
//! ugorji/go's MsgpackSpecRpc net/rpc codec (through `tests/client/msgpack_rpc/peer`).
//! `tests/client/msgpack_rpc/install_peers.py` prints NETGET_MSGPACK_NVIM and
//! NETGET_MSGPACK_PEER.
use super::wire_test::{handler_saw, handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/msgpack_rpc/install_peers.py <root> and export what it prints"))
}

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&program)
            .args(args)
            .stdin(std::process::Stdio::null())
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

const NVIM_SCRIPT: &str = r#"local ch = vim.fn.sockconnect('tcp', _G.arg[1], {rpc = true})
local out = {}
out.add = vim.rpcrequest(ch, 'add', 2, 3)
out.echo = vim.rpcrequest(ch, 'echo', {nested = {1, 'two', true}}, 'é')
local ok, err = pcall(vim.rpcrequest, ch, 'missing')
out.ok = ok
out.err = tostring(err)
vim.rpcnotify(ch, 'log', 'from nvim')
out.after = vim.rpcrequest(ch, 'add', 40, 2)
io.stdout:write(vim.json.encode(out) .. "\n")
"#;

#[tokio::test]
async fn neovim_client() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("client.lua");
    std::fs::write(&script, NVIM_SCRIPT).unwrap();
    let out = run(
        env_path("NETGET_MSGPACK_NVIM"),
        &[
            "--headless",
            "--clean",
            "-l",
            script.to_str().unwrap(),
            &addr.to_string(),
        ],
    )
    .await;
    assert_eq!(
        (out["add"].clone(), out["after"].clone(), out["ok"].clone()),
        (json!(5), json!(42), json!(false)),
        "{out}"
    );
    assert_eq!(out["echo"], json!([{"nested": [1, "two", true]}, "é"]));
    assert!(
        out["err"]
            .as_str()
            .unwrap()
            .contains("no such method: missing"),
        "{out}"
    );
    assert!(
        handler_saw(&state, id, r#""params":["from nvim"]"#).await,
        "the notification reached the handler"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn ugorji_go_client() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let out = run(
        env_path("NETGET_MSGPACK_PEER"),
        &["client", &addr.to_string()],
    )
    .await;
    assert_eq!(
        out,
        json!({"add": 5, "echo": ["go", [1, 2]], "missing_error": "no such method: missing"})
    );
    state.remove_server(id).await;
}
