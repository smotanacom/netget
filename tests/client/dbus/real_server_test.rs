//! NetGet's D-Bus client on **dbus-daemon**, the reference message bus, run unprivileged from
//! a config in a temp dir. On the bus: a python **dbus-next** service (`net.example.Service`)
//! that greets, records what it is told to a file and announces it as a signal, and
//! **dbus-send**, which calls the name NetGet owns. Fails rather than skips without them
//! (`apt-get install dbus-daemon dbus-bin python3-dbus-next`).
//!
//! The chain: own `net.netget.Client`, subscribe to the service's signals, call Greet; on the
//! reply, call Record with the greeting the service returned plus a word of its own. The
//! service's file then holds that text — the model acted on the server's answer, asserted by
//! the server's own side — and the Announced signal comes back as `dbus_signal`. No LLM calls.
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

const DBUS_DAEMON: InstallHint = InstallHint {
    brew: "dbus",
    apt: "dbus-daemon",
};

const BUS_CONF: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={dir}/bus</listen>
  <listen>tcp:host=127.0.0.1,port={port}</listen>
  <auth>EXTERNAL</auth>
  <auth>ANONYMOUS</auth>
  <allow_anonymous/>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

const SERVICE: &str = r#"import asyncio, sys, time
from dbus_next.aio import MessageBus
from dbus_next.service import ServiceInterface, method, signal
class Service(ServiceInterface):
    def __init__(self, path):
        super().__init__('net.example.Service')
        self.path = path
    @method()
    def Greet(self, name: 's') -> 's':
        return 'hello, ' + name
    @method()
    def Record(self, text: 's'):
        with open(self.path, 'a') as f:
            f.write(text + '\n')
        self.Announced(text)
    @signal()
    def Announced(self, text) -> 's':
        return text
    @method()
    def Slow(self) -> 's':
        time.sleep(3)
        return 'late'
async def main():
    bus = await MessageBus(bus_address=sys.argv[1]).connect()
    bus.export('/net/example/Service', Service(sys.argv[2]))
    await bus.request_name('net.example.Service')
    print('READY', flush=True)
    await asyncio.get_running_loop().create_future()
asyncio.run(main())
"#;

pub async fn bus() -> RealServer {
    RealServer::builder("dbus-daemon", DBUS_DAEMON)
        .config_file("bus.conf", BUS_CONF)
        .args([
            "--config-file={dir}/bus.conf",
            "--nofork",
            "--print-address",
        ])
        .ready_when_log_matches("unix:path=")
        .startup_timeout(Duration::from_secs(20))
        .start()
        .await
        .expect("start dbus-daemon")
}

/// The python service, running until dropped; the file it records to.
async fn service(bus: &RealServer) -> (tokio::process::Child, std::path::PathBuf) {
    let script = bus.dir().join("service.py");
    std::fs::write(&script, SERVICE).unwrap();
    let record = bus.dir().join("record.txt");
    let python = find_binary("python3").expect("python3 is required");
    let mut child = tokio::process::Command::new(python)
        .args([
            "-I",
            script.to_str().unwrap(),
            &format!("unix:path={}", bus.dir().join("bus").display()),
            record.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await;
    assert!(
        matches!(&ready, Ok(Ok(Some(l))) if l == "READY"),
        "the dbus-next service did not start ({ready:?}); apt-get install python3-dbus-next"
    );
    (child, record)
}

pub async fn client(
    remote: String,
    params: Value,
    handlers: Vec<Value>,
) -> anyhow::Result<(AppState, ClientId)> {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "dbus".into(),
        remote_addr: Some(remote),
        instruction: Some("Work on the bus".into()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    Ok((state, id))
}

pub async fn wait_match(
    state: &AppState,
    id: ClientId,
    event_type: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let hit = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .filter(|e| e["event_type"] == event_type)
                .map(|e| e["request"].clone())
                .find(|r| pred(r));
            if let Some(e) = hit {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("no matching {event_type} event"))
}

const ON_REPLY: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
a=[]
if e['member']=='Greet':
  a=[{'type':'dbus_call','destination':'net.example.Service','path':'/net/example/Service','interface':'net.example.Service','member':'Record','signature':'s','args':[e['values'][0]+' acknowledged']}]
print(json.dumps({'actions':a}))"#;

const ON_CALL: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
a=[]
if e['member']=='Ping':
  a=[{'type':'dbus_return','signature':'s','values':['pong:'+e['args'][0]]}]
print(json.dumps({'actions':a}))"#;

fn chain() -> Vec<Value> {
    vec![
        json!({"event_pattern":"dbus_connected","handler":{"type":"static","actions":[
            {"type":"dbus_request_name","name":"net.netget.Client"},
            {"type":"dbus_add_match","rule":"type='signal',interface='net.example.Service'"},
            {"type":"dbus_call","destination":"net.example.Service","path":"/net/example/Service","interface":"net.example.Service","member":"Greet","signature":"s","args":["netget"]}]}}),
        json!({"event_pattern":"dbus_reply","handler":{"type":"script","language":"python","code":ON_REPLY}}),
        json!({"event_pattern":"dbus_method_call","handler":{"type":"script","language":"python","code":ON_CALL}}),
        json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
    ]
}

async fn dbus_send(address: &str, args: &[&str]) -> (bool, String) {
    let bin = find_binary("dbus-send").expect("dbus-send is required: apt-get install dbus-bin");
    let out = tokio::process::Command::new(bin)
        .arg(format!("--bus={address}"))
        .args(args)
        .output()
        .await
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

fn send_outcome(o: ClientSendOutcome) -> String {
    match o {
        ClientSendOutcome::Executed { detail } => detail,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn netget_on_dbus_daemon() {
    let bus = bus().await;
    let (_service, record) = service(&bus).await;
    let address = format!("unix:path={}", bus.dir().join("bus").display());
    let (state, id) = client(
        String::new(),
        json!({"socket_path": bus.dir().join("bus"), "timeout_ms": 1000}),
        chain(),
    )
    .await
    .expect("connect");

    let connected = wait_match(&state, id, "dbus_connected", |_| true).await;
    assert_eq!(connected["mechanism"], "EXTERNAL", "{connected}");
    assert!(
        connected["unique_name"].as_str().unwrap().starts_with(':'),
        "{connected}"
    );
    wait_match(&state, id, "dbus_reply", |r| {
        r["member"] == "RequestName" && r["values"] == json!([1])
    })
    .await;
    // The service recorded what the model built from the service's own reply.
    let signal = wait_match(&state, id, "dbus_signal", |s| s["member"] == "Announced").await;
    assert_eq!(
        signal["args"],
        json!(["hello, netget acknowledged"]),
        "{signal}"
    );
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "hello, netget acknowledged\n"
    );

    // dbus-daemon routes a call to the name NetGet owns; the model answers it.
    let (ok, out) = dbus_send(
        &address,
        &[
            "--print-reply",
            "--dest=net.netget.Client",
            "/net/netget/Client",
            "net.netget.Client.Ping",
            "string:bus",
        ],
    )
    .await;
    assert!(
        ok && out.contains("string \"pong:bus\""),
        "{out}\n{}",
        bus.log()
    );
    // A call the model does not answer is refused, not left to time out.
    let (ok, out) = dbus_send(
        &address,
        &[
            "--print-reply",
            "--reply-timeout=10000",
            "--dest=net.netget.Client",
            "/net/netget/Client",
            "net.netget.Client.Silent",
        ],
    )
    .await;
    assert!(
        !ok && out
            .contains("org.freedesktop.DBus.Error.Failed: No answer was produced for this call"),
        "{out}"
    );

    // Injected calls: a method the service lacks, and one slower than the client's timeout.
    let missing = send_outcome(
        state
            .send_to_client(id, json!({"type":"dbus_call","destination":"net.example.Service","path":"/net/example/Service","interface":"net.example.Service","member":"Nope"}), Duration::from_secs(20))
            .await
            .unwrap(),
    );
    assert!(
        missing.contains("org.freedesktop.DBus.Error.UnknownMethod"),
        "{missing}"
    );
    let slow = send_outcome(
        state
            .send_to_client(id, json!({"type":"dbus_call","destination":"net.example.Service","path":"/net/example/Service","interface":"net.example.Service","member":"Slow"}), Duration::from_secs(20))
            .await
            .unwrap(),
    );
    assert!(
        slow.contains("org.freedesktop.DBus.Error.NoReply"),
        "{slow}"
    );
}

#[tokio::test]
async fn anonymous_over_tcp() {
    let bus = bus().await;
    let (state, id) = client(
        bus.addr(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})],
    )
    .await
    .expect("connect");
    let connected = wait_match(&state, id, "dbus_connected", |_| true).await;
    // Over TCP dbus-daemon cannot check EXTERNAL, so the client fell back to ANONYMOUS.
    assert_eq!(connected["mechanism"], "ANONYMOUS", "{connected}");
    let names = send_outcome(
        state
            .send_to_client(id, json!({"type":"dbus_call","destination":"org.freedesktop.DBus","path":"/org/freedesktop/DBus","interface":"org.freedesktop.DBus","member":"ListNames"}), Duration::from_secs(20))
            .await
            .unwrap(),
    );
    let unique = connected["unique_name"].as_str().unwrap();
    assert!(
        names.contains(unique) && names.contains("org.freedesktop.DBus"),
        "{names}"
    );
}
