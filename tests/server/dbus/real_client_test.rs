//! Independent D-Bus clients against NetGet's D-Bus server, failing rather than skipping when
//! absent:
//!
//! * **dbus-send** (libdbus, the reference implementation) peer-to-peer over TCP, which can
//!   only complete ANONYMOUS there;
//! * **gdbus** (GLib's GDBus) as a bus client over the Unix socket, with EXTERNAL checked
//!   against the socket's peer credentials, Hello and a call to the bus itself;
//! * **python dbus-next** (asyncio), which says Hello, requests a name, and receives the
//!   signal the policy emits alongside its return.
//!
//! Each client decodes NetGet's replies with its own unmarshaller, so a wrong alignment,
//! length or signature is its error, not ours. No LLM calls.
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

/// Ping answers pong:<arg>; Sum adds an int array; Echo returns its arguments unchanged; Notify
/// returns and emits Changed; Deny refuses; Silent answers nothing.
pub const POLICY: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
m=e['member']; a=e['args']
if m=='Ping': out=[{'type':'dbus_return','signature':'s','values':['pong:'+(a[0] if a else '')]}]
elif m=='Sum': out=[{'type':'dbus_return','signature':'i','values':[sum(a[0])]}]
elif m=='Echo': out=[{'type':'dbus_return','signature':e['signature'],'values':a}]
elif m=='Notify': out=[{'type':'dbus_return','signature':'','values':[]},{'type':'dbus_emit_signal','path':'/net/netget/Demo','interface':'net.netget.Demo','member':'Changed','signature':'s','values':['changed:'+a[0]]}]
elif m=='Deny': out=[{'type':'dbus_error','name':'net.netget.Error.Denied','message':'nope'}]
else: out=[]
print(json.dumps({'actions':out}))"#;

pub async fn start(socket: &Path, extra: Value) -> (AppState, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let mut params = json!({"socket_path": socket});
    for (k, v) in extra.as_object().into_iter().flatten() {
        params[k] = v.clone();
    }
    let id = ServerForm {
        protocol: "dbus".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer D-Bus calls".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}})]),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, port)
}

/// Run a tool to completion; (success, stdout + stderr).
pub async fn run(bin: &str, args: &[&str]) -> (bool, String) {
    let path = crate::helpers::real_server::find_binary(bin).unwrap_or_else(|| {
        panic!("{bin} is required: apt-get install dbus-bin libglib2.0-bin python3-dbus-next")
    });
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(path)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("{bin} {args:?} did not finish"))
    .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test]
async fn dbus_send_peer_to_peer_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port) = start(&dir.path().join("bus"), json!({})).await;
    let peer = format!("--peer=tcp:host=127.0.0.1,port={port}");
    let send = |method: &'static str, args: Vec<&'static str>| {
        let peer = peer.clone();
        async move {
            let mut all = vec![
                peer.as_str(),
                "--print-reply",
                "--reply-timeout=20000",
                "/net/netget/Demo",
            ];
            let full = format!("net.netget.Demo.{method}");
            all.push(&full);
            all.extend(args);
            run("dbus-send", &all).await
        }
    };
    let (ok, out) = send("Ping", vec!["string:hello"]).await;
    assert!(ok && out.contains("string \"pong:hello\""), "{out}");
    let (ok, out) = send("Sum", vec!["array:int32:1,2,39"]).await;
    assert!(ok && out.contains("int32 42"), "{out}");
    // Every container type, round-tripped through JSON and back: libdbus decodes our bytes.
    let (ok, out) = send(
        "Echo",
        vec![
            "dict:string:int32:a,1,bb,2",
            "variant:double:2.5",
            "objpath:/x/y",
            "uint64:18446744073709551615",
            "boolean:true",
        ],
    )
    .await;
    assert!(ok, "{out}");
    for needle in [
        "string \"a\"",
        "int32 1",
        "string \"bb\"",
        "int32 2",
        "variant",
        "double 2.5",
        "object path \"/x/y\"",
        "uint64 18446744073709551615",
        "boolean true",
    ] {
        assert!(out.contains(needle), "{needle} missing from {out}");
    }
    let (ok, out) = send("Deny", vec![]).await;
    assert!(
        !ok && out.contains("net.netget.Error.Denied: nope"),
        "{out}"
    );
    // No answer from the model: an error, not the caller's timeout.
    let (ok, out) = send("Silent", vec![]).await;
    assert!(
        !ok && out
            .contains("org.freedesktop.DBus.Error.Failed: No answer was produced for this call"),
        "{out}"
    );
}

#[tokio::test]
async fn gdbus_as_a_bus_client_over_the_unix_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bus");
    let (_state, _port) = start(&socket, json!({"allow_anonymous": false})).await;
    let address = format!("unix:path={}", socket.display());
    let (ok, out) = run(
        "gdbus",
        &[
            "call",
            "--address",
            &address,
            "--dest",
            "net.netget.Demo",
            "--object-path",
            "/net/netget/Demo",
            "--method",
            "net.netget.Demo.Ping",
            "gdbus",
        ],
    )
    .await;
    assert!(ok && out.trim() == "('pong:gdbus',)", "{out}");
    let (ok, out) = run(
        "gdbus",
        &[
            "call",
            "--address",
            &address,
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.ListNames",
        ],
    )
    .await;
    assert!(
        ok && out.contains("'org.freedesktop.DBus'") && out.contains("':1."),
        "{out}"
    );
    let (ok, out) = run(
        "gdbus",
        &[
            "call",
            "--address",
            &address,
            "--dest",
            "net.netget.Demo",
            "--object-path",
            "/net/netget/Demo",
            "--method",
            "net.netget.Demo.Deny",
        ],
    )
    .await;
    assert!(!ok && out.contains("net.netget.Error.Denied"), "{out}");
}

const DBUS_NEXT: &str = r#"import asyncio, json, sys
from dbus_next.aio import MessageBus
from dbus_next import Message, MessageType
async def main():
    bus = await MessageBus(bus_address=sys.argv[1]).connect()
    got = asyncio.get_running_loop().create_future()
    def handler(m):
        if m.message_type == MessageType.SIGNAL and m.member == 'Changed' and not got.done():
            got.set_result(m.body)
    bus.add_message_handler(handler)
    owned = await bus.request_name('net.netget.Py')
    reply = await bus.call(Message(destination='net.netget.Demo', path='/net/netget/Demo', interface='net.netget.Demo', member='Notify', signature='s', body=['from-python']))
    body = await asyncio.wait_for(got, 10)
    print(json.dumps({'unique': bus.unique_name, 'owned': owned.value, 'reply': reply.message_type.name, 'signal': body}))
asyncio.run(main())
"#;

#[tokio::test]
async fn python_dbus_next_receives_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bus");
    let (_state, _port) = start(&socket, json!({})).await;
    let script = dir.path().join("client.py");
    std::fs::write(&script, DBUS_NEXT).unwrap();
    let (ok, out) = run(
        "python3",
        &[
            "-I",
            script.to_str().unwrap(),
            &format!("unix:path={}", socket.display()),
        ],
    )
    .await;
    assert!(ok, "{out}");
    let result: Value = serde_json::from_str(out.lines().last().unwrap_or_default())
        .unwrap_or_else(|_| panic!("{out}"));
    assert!(
        result["unique"].as_str().unwrap().starts_with(":1."),
        "{result}"
    );
    assert_eq!(
        result["owned"], 1,
        "RequestName answered PRIMARY_OWNER: {result}"
    );
    assert_eq!(result["reply"], "METHOD_RETURN", "{result}");
    assert_eq!(result["signal"], json!(["changed:from-python"]), "{result}");
}
