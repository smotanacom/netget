//! NetGet's MessagePack-RPC client against two independent servers, failing rather than
//! skipping when absent: Neovim with --listen (its API over MessagePack-RPC: eval, commands,
//! variables, an error, a buffer handle as an extension, and a subscribed notification), and
//! ugorji/go's MsgpackSpecRpc net/rpc server through `peer/`.
use super::session_test::{call, client, wait_log};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn netget_against_neovim() {
    let nvim = std::env::var("NETGET_MSGPACK_NVIM").expect("NETGET_MSGPACK_NVIM is required: run tests/client/msgpack_rpc/install_peers.py <root> and export what it prints");
    let server = RealServer::builder(
        &nvim,
        InstallHint {
            brew: "neovim",
            apt: "neovim",
        },
    )
    .args(["--headless", "--clean", "--listen", "127.0.0.1:{port}"])
    .startup_timeout(Duration::from_secs(30))
    .start()
    .await
    .expect("start nvim --listen");
    let (state, id) = client(server.addr()).await;
    let r = call(&state, id, "nvim_eval", json!(["1 + 2"])).await;
    assert_eq!(r["result"], 3, "{r}");
    let r = call(
        &state,
        id,
        "nvim_command",
        json!(["let g:netget = 'hello from netget'"]),
    )
    .await;
    assert_eq!(
        (r["error"].clone(), r["result"].clone()),
        (Value::Null, Value::Null),
        "{r}"
    );
    let r = call(&state, id, "nvim_get_var", json!(["netget"])).await;
    assert_eq!(r["result"], "hello from netget");
    let r = call(&state, id, "nvim_eval", json!(["NoSuchFunction()"])).await;
    assert!(
        r["result"].is_null() && r["error"].to_string().contains("E117"),
        "{r}"
    );
    let r = call(&state, id, "nvim_get_current_buf", json!([])).await;
    assert_eq!(
        r["result"]["$ext"], 0,
        "a Buffer handle is extension 0: {r}"
    );
    call(&state, id, "nvim_subscribe", json!(["netget_ev"])).await;
    call(
        &state,
        id,
        "nvim_command",
        json!(["call rpcnotify(0, 'netget_ev', 'payload', 42)"]),
    )
    .await;
    let note = wait_log(&state, id, r#""method":"netget_ev""#).await;
    assert!(note.contains(r#""params":["payload",42]"#), "{note}");
    state.remove_client(id).await;
}

#[tokio::test]
async fn netget_against_ugorji_go_server() {
    let peer = std::env::var_os("NETGET_MSGPACK_PEER").map(PathBuf::from).expect("NETGET_MSGPACK_PEER is required: run tests/client/msgpack_rpc/install_peers.py <root> and export what it prints");
    let mut child = tokio::process::Command::new(peer)
        .arg("server")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the Go peer");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut ready))
        .await
        .expect("peer did not start")
        .unwrap();
    let addr = ready
        .trim()
        .strip_prefix("READY ")
        .expect("READY line")
        .to_string();
    let (state, id) = client(addr).await;
    let r = call(&state, id, "Arith.Add", json!([[2, 3, 37]])).await;
    assert_eq!(r["result"], 42, "{r}");
    let r = call(&state, id, "Arith.Echo", json!(["hi"])).await;
    assert_eq!(r["result"], "go says hi", "{r}");
    let r = call(&state, id, "Arith.Fail", json!(["now"])).await;
    assert_eq!(
        (r["error"].clone(), r["result"].clone()),
        (json!("arith refuses: now"), Value::Null),
        "{r}"
    );
    state.remove_client(id).await;
    child.kill().await.unwrap();
}
