//! NetGet's Pulsar client against **Apache Pulsar 4.0.6** in standalone mode (the real
//! broker, BookKeeper and metadata store in one JVM). NetGet looks the topics up, subscribes
//! and publishes; the official Python client sits on the other side, publishing what NetGet
//! must receive and reading what NetGet published in answer. `tests/server/pulsar/
//! install_peers.py` prints `NETGET_PULSAR_HOME` and `NETGET_PULSAR_PYTHON`; the test fails
//! rather than skips without them. No LLM calls: a python chain is the model.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// Connected → subscribe to inbox; each message → publish its answer to outbox.
const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='pulsar_connected': a=[{'type':'pulsar_subscribe','topic':'inbox','subscription':'netget'}]
elif t=='pulsar_message': a=[{'type':'pulsar_produce','topic':'outbox','payload':'netget saw '+e['payload'],'properties':{'n':e['properties'].get('n','?')},'key':'answer'}]
print(json.dumps({'actions':a}))"#;

/// The Python side: subscribe to outbox, publish to inbox, read the answers.
const PEER: &str = r#"
import json, sys, time, pulsar
c = pulsar.Client(sys.argv[1], operation_timeout_seconds=30)
out = c.subscribe('outbox', 'peer')
print(json.dumps({'step': 'ready'}), flush=True)
sys.stdin.readline()
p = c.create_producer('inbox')
for n in range(2):
    p.send(f'hello {n}'.encode(), properties={'n': str(n)})
got = []
for _ in range(2):
    m = out.receive(timeout_millis=30000)
    got.append({'data': m.data().decode(), 'properties': m.properties(), 'key': m.partition_key()})
    out.acknowledge(m)
print(json.dumps({'step': 'done', 'got': got}), flush=True)
c.close()
"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/pulsar/install_peers.py <dir> and export what it prints"
    );
    v
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Pulsar standalone on loopback ports of its own, in a temp dir; killed on drop.
struct Standalone {
    child: tokio::process::Child,
    port: u16,
    dir: tempfile::TempDir,
}

impl Drop for Standalone {
    fn drop(&mut self) {
        // bin/pulsar execs java, so the child is the JVM itself.
        let _ = self.child.start_kill();
    }
}

async fn standalone() -> Standalone {
    let home = env("NETGET_PULSAR_HOME");
    let dir = tempfile::tempdir().unwrap();
    let (port, web, bk) = (free_port(), free_port(), free_port());
    let conf = std::fs::read_to_string(format!("{home}/conf/standalone.conf")).unwrap();
    let conf: String = conf
        .lines()
        .map(|l| match l.split_once('=').map(|(k, _)| k) {
            Some("brokerServicePort") => format!("brokerServicePort={port}"),
            Some("webServicePort") => format!("webServicePort={web}"),
            Some("bindAddress") => "bindAddress=127.0.0.1".into(),
            Some("advertisedAddress") => "advertisedAddress=127.0.0.1".into(),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let conf_path = dir.path().join("standalone.conf");
    std::fs::write(&conf_path, conf).unwrap();
    let log = std::fs::File::create(dir.path().join("pulsar.out")).unwrap();
    let child = tokio::process::Command::new(format!("{home}/bin/pulsar"))
        .arg("standalone")
        .arg("-c")
        .arg(&conf_path)
        .args(["-nss", "-nfw"])
        .arg(format!(
            "--metadata-url=rocksdb://{}",
            dir.path().join("meta").display()
        ))
        .arg(format!(
            "--bookkeeper-dir={}",
            dir.path().join("bk").display()
        ))
        .arg(format!("--bookkeeper-port={bk}"))
        .env("PULSAR_LOG_DIR", dir.path().join("logs"))
        .env(
            "PULSAR_MEM",
            "-Xms256m -Xmx768m -XX:MaxDirectMemorySize=512m",
        )
        .current_dir(dir.path())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .expect("bin/pulsar");
    let s = Standalone { child, port, dir };
    let ready = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let ok = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://127.0.0.1:{web}/admin/v2/namespaces/public"))
                .send()
                .await
                .ok()
                .filter(|r| r.status().is_success());
            if let Some(r) = ok {
                if r.text()
                    .await
                    .unwrap_or_default()
                    .contains("public/default")
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    if ready.is_err() {
        panic!(
            "Pulsar standalone never came up:\n{}",
            std::fs::read_to_string(s.dir.path().join("pulsar.out")).unwrap_or_default()
        );
    }
    s
}

async fn wait_event(state: &AppState, id: ClientId, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let hit = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .find(|e| pred(e))
                .map(|e| e["request"].clone());
            if let Some(e) = hit {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no matching event")
}

#[tokio::test]
async fn netget_consumes_and_produces_on_apache_pulsar() {
    let broker = standalone().await;
    let url = format!("pulsar://127.0.0.1:{}", broker.port);
    let mut peer = tokio::process::Command::new(env("NETGET_PULSAR_PYTHON"))
        .args(["-c", PEER, &url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut lines = tokio::io::BufReader::new(peer.stdout.take().unwrap()).lines();
    // The C++ library logs to stdout too: the peer's own lines are the JSON ones.
    let mut next_line = async || -> Value {
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let l = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the python peer ended");
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    return v;
                }
            }
        })
        .await
        .expect("the python peer said nothing")
    };
    assert_eq!(next_line().await["step"], "ready");

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "pulsar".into(),
        remote_addr: Some(url.clone()),
        instruction: Some("Answer every message on inbox".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");
    let connected = wait_event(&state, id, |e| e["event_type"] == "pulsar_connected").await;
    assert!(
        connected["server_version"]
            .as_str()
            .unwrap()
            .starts_with("Pulsar Server"),
        "{connected}"
    );
    let sub = wait_event(&state, id, |e| e["event_type"] == "pulsar_subscribed").await;
    assert_eq!(sub["ok"], true, "{sub}");
    assert_eq!(sub["topic"], "persistent://public/default/inbox");

    // Now the peer publishes; NetGet hears both and answers each on outbox.
    peer.stdin
        .as_mut()
        .unwrap()
        .write_all(b"go\n")
        .await
        .unwrap();
    let done = next_line().await;
    assert_eq!(done["step"], "done", "{done}");
    let got = done["got"].as_array().unwrap();
    let mut seen: Vec<&str> = got.iter().map(|m| m["data"].as_str().unwrap()).collect();
    seen.sort();
    assert_eq!(seen, ["netget saw hello 0", "netget saw hello 1"], "{done}");
    for m in got {
        assert_eq!(m["key"], "answer", "{m}");
        let n = m["data"].as_str().unwrap().rsplit(' ').next().unwrap();
        assert_eq!(m["properties"], json!({"n": n}), "{m}");
    }
    let msg = wait_event(&state, id, |e| e["event_type"] == "pulsar_message").await;
    assert_eq!(msg["subscription"], "netget", "{msg}");
    assert_eq!(msg["encoding"], "utf8", "{msg}");
    let produced = wait_event(&state, id, |e| e["event_type"] == "pulsar_produced").await;
    assert_eq!(produced["ok"], true, "{produced}");
    assert!(produced["message_id"]["ledger_id"].is_u64(), "{produced}");

    // By hand: a hex payload, received by nobody but stored by the broker.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"pulsar_produce","topic":"persistent://public/default/raw","payload":"00ff10","encoding":"hex"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Executed { detail } if detail.contains("\"ok\":true")),
        "{outcome:?}"
    );
    // Refused before the wire.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"pulsar_produce","topic":"a/b","payload":"x"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { .. }),
        "{outcome:?}"
    );
}
