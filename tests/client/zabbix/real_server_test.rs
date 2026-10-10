//! NetGet's Zabbix client against Zabbix 7.0's own components: **zabbix_agentd** answers passive
//! checks (a supported key, the agent's configured hostname, and an unsupported key), and
//! **zabbix_proxy** (SQLite) receives sender data, logging at DebugLevel 4 the request it parsed
//! and the host and key it then looked up. A python chain is the model. The binaries come from
//! the Zabbix 7.0 packages (`zabbix-agent`, `zabbix-proxy-sqlite3` from repo.zabbix.com); the
//! test fails rather than skips without them. No LLM calls.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

const AGENT_CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='zabbix_ready': a=[{'type':'zabbix_get','key':'agent.ping'}]
elif t=='zabbix_value' and e['key']=='agent.ping': a=[{'type':'zabbix_get','key':'agent.hostname'}]
elif t=='zabbix_value' and e['key']=='agent.hostname': a=[{'type':'zabbix_get','key':'no.such.key'}]
print(json.dumps({'actions':a}))"#;

const SENDER_CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']
a=[{'type':'zabbix_send','values':[{'host':'netget-host','key':'netget.status','value':'ok'},{'host':'netget-host','key':'netget.load','value':0.5}]}] if t=='zabbix_ready' else []
print(json.dumps({'actions':a}))"#;

fn binary(var: &str, default: &str) -> PathBuf {
    let p = std::env::var(var)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(default));
    assert!(
        p.exists(),
        "{} is required ({var} or {default}): install zabbix-agent and zabbix-proxy-sqlite3 7.0 from repo.zabbix.com",
        p.display()
    );
    p
}

/// Zabbix refuses a ListenPort above 32767, below the usual ephemeral range: pick a free one
/// from 20000-32767.
fn free_port() -> u16 {
    loop {
        let port = 20000 + rand::random::<u16>() % 12768;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// A Zabbix daemon in the foreground with a config of its own. It forks its workers, so it
/// runs in a process group of its own and the whole group is killed on drop.
struct Daemon {
    child: tokio::process::Child,
    port: u16,
    log: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

async fn daemon(bin: PathBuf, dir: &Path, name: &str, extra: &str) -> Daemon {
    let port = free_port();
    let log = dir.join(format!("{name}.log"));
    let conf = dir.join(format!("{name}.conf"));
    std::fs::write(
        &conf,
        format!(
            "ListenPort={port}\nListenIP=127.0.0.1\nLogFile={}\nPidFile={}\nAllowRoot=1\n{extra}",
            log.display(),
            dir.join(format!("{name}.pid")).display()
        ),
    )
    .unwrap();
    let child = tokio::process::Command::new(bin)
        .arg("-c")
        .arg(&conf)
        .arg("-f")
        .stdout(std::fs::File::create(dir.join(format!("{name}.out"))).unwrap())
        .stderr(std::fs::File::create(dir.join(format!("{name}.err"))).unwrap())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let up = tokio::time::timeout(Duration::from_secs(30), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        up.is_ok(),
        "{name} never listened:\n{}\n{}",
        std::fs::read_to_string(&log).unwrap_or_default(),
        std::fs::read_to_string(dir.join(format!("{name}.err"))).unwrap_or_default()
    );
    Daemon { child, port, log }
}

async fn client(state: &AppState, port: u16, chain: &str) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    ClientForm {
        protocol: "zabbix".into(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        instruction: Some("Watch the agent".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":chain}}),
        ]),
        ..Default::default()
    }
    .create(state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect")
}

async fn wait_event(state: &AppState, id: ClientId, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
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
async fn netget_queries_zabbix_agentd() {
    let dir = tempfile::tempdir().unwrap();
    let agent = daemon(
        binary("NETGET_ZABBIX_AGENTD", "/usr/sbin/zabbix_agentd"),
        dir.path(),
        "agent",
        "Server=127.0.0.1\nHostname=netget-agent\nStartAgents=2\n",
    )
    .await;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let id = client(&state, agent.port, AGENT_CHAIN).await;
    let key = |k: &'static str| {
        move |e: &Value| e["event_type"] == "zabbix_value" && e["request"]["key"] == k
    };
    assert_eq!(
        wait_event(&state, id, key("agent.ping")).await,
        json!({"key": "agent.ping", "supported": true, "value": "1"})
    );
    assert_eq!(
        wait_event(&state, id, key("agent.hostname")).await["value"],
        "netget-agent"
    );
    let unsupported = wait_event(&state, id, key("no.such.key")).await;
    assert_eq!(unsupported["supported"], false, "{unsupported}");
    assert_eq!(
        unsupported["error"], "Unsupported item key.",
        "{unsupported}"
    );
    // A key with a line break is refused before anything is sent.
    let outcome = state
        .send_to_client(
            id,
            json!({"type": "zabbix_get", "key": "agent.ping\nsystem.run[id]"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ClientSendOutcome::Rejected { .. }),
        "{outcome:?}"
    );
    let _ = agent.log;
}

#[tokio::test]
async fn netget_sends_to_zabbix_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let extra = format!(
        "Server=127.0.0.1:1\nHostname=netget-proxy\nDBName={}\nSocketDir={}\nDebugLevel=4\n",
        dir.path().join("proxy.db").display(),
        dir.path().display()
    );
    let proxy = daemon(
        binary("NETGET_ZABBIX_PROXY", "/usr/sbin/zabbix_proxy"),
        dir.path(),
        "proxy",
        &extra,
    )
    .await;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let id = client(&state, proxy.port, SENDER_CHAIN).await;
    let sent = wait_event(&state, id, |e| e["event_type"] == "zabbix_sent").await;
    // The proxy has no configuration from a server, so it knows neither item: it parsed both
    // and refused both, and says so in its own summary.
    assert_eq!(sent["response"], "success", "{sent}");
    assert_eq!(
        (
            sent["processed"].clone(),
            sent["failed"].clone(),
            sent["total"].clone()
        ),
        (json!(0), json!(2), json!(2)),
        "{sent}"
    );
    let log = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
            if log.contains("cannot retrieve key \"netget.load\"") {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the proxy never looked the items up:\n{}",
            std::fs::read_to_string(&proxy.log).unwrap_or_default()
        )
    });
    assert!(
        log.contains(r#"trapper got '{"request":"sender data","data":[{"host":"netget-host","key":"netget.status","value":"ok"},{"host":"netget-host","key":"netget.load","value":"0.5"}]}'"#),
        "the proxy logged another request:\n{log}"
    );
    assert!(
        log.contains("cannot retrieve key \"netget.status\" on host \"netget-host\""),
        "{log}"
    );
}
