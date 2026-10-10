//! NetGet's TR-069 device against **GenieACS** 1.2.16 (genieacs-cwmp and genieacs-nbi) over
//! **MongoDB** 8.0.4. NetGet informs with BOOTSTRAP and BOOT and GenieACS registers it; then
//! tasks queued through GenieACS's own northbound API with `connection_request` make GenieACS
//! knock on NetGet's connection-request URL, NetGet opens a session, and GenieACS sends the
//! RPCs it needs (discovering the data model as it goes). A python chain answers them from a
//! small table, and the NBI is read back. `tests/server/tr069/install_peers.py` prints
//! `NETGET_GENIEACS` and `NETGET_MONGOD`; the test fails rather than skips without them.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// The device's data model, answered from a table; anything else is fault 9005.
const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
D={'Device.DeviceInfo.SoftwareVersion':['2.1',False],'Device.DeviceInfo.HardwareVersion':['hw1',False],
   'Device.ManagementServer.PeriodicInformInterval':[300,True]}
def objs(names):
  s=set()
  for n in names:
    p=n.split('.')
    for k in range(1,len(p)): s.add('.'.join(p[:k])+'.')
  return s
a=[]
if t=='tr069_rpc':
  m=e['method']; g=e['arguments']
  if m=='GetParameterValues':
    out={}
    for n in g['names']:
      hit={k:v[0] for k,v in D.items() if k==n or (n.endswith('.') and k.startswith(n)) or n==''}
      if not hit: out=None; break
      out.update(hit)
    a=[{'type':'tr069_parameter_values','parameters':out}] if out is not None else [{'type':'tr069_fault','code':9005,'message':'Invalid parameter name'}]
  elif m=='GetParameterNames':
    path=g['path']; allp=list(D)+sorted(objs(D))
    under=[k for k in allp if k.startswith(path) and k!=path]
    if g['next_level']:
      under=[k for k in under if '.' not in k[len(path):].rstrip('.')]
    a=[{'type':'tr069_parameter_names','parameters':[{'name':k,'writable':(D.get(k,[0,False])[1])} for k in sorted(under)]}]
  else:
    a=[{'type':'tr069_done','status':0}]
print(json.dumps({'actions':a}))"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/tr069/install_peers.py <dir> and export what it prints"
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

/// A process in its own group, which is killed whole on drop (GenieACS forks workers).
struct Group(tokio::process::Child);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

fn spawn(program: &str, args: &[&str], envs: &[(&str, String)], log: &std::path::Path) -> Group {
    let out = std::fs::File::create(log).unwrap();
    let mut c = tokio::process::Command::new(program);
    c.args(args)
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .process_group(0)
        .kill_on_drop(true);
    Group(c.spawn().unwrap_or_else(|e| panic!("{program}: {e}")))
}

struct Acs {
    _procs: Vec<Group>,
    cwmp: u16,
    nbi: u16,
    dir: tempfile::TempDir,
}

async fn genieacs() -> Acs {
    let dir = tempfile::tempdir().unwrap();
    let (mongo, cwmp, nbi) = (free_port(), free_port(), free_port());
    std::fs::create_dir_all(dir.path().join("db")).unwrap();
    let mongod = spawn(
        &env("NETGET_MONGOD"),
        &[
            "--dbpath",
            dir.path().join("db").to_str().unwrap(),
            "--port",
            &mongo.to_string(),
            "--bind_ip",
            "127.0.0.1",
        ],
        &[],
        &dir.path().join("mongod.log"),
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", mongo))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("mongod never listened");
    let home = env("NETGET_GENIEACS");
    let common = vec![
        (
            "GENIEACS_MONGODB_CONNECTION_URL",
            format!("mongodb://127.0.0.1:{mongo}/genieacs"),
        ),
        ("GENIEACS_EXT_DIR", dir.path().display().to_string()),
        ("GENIEACS_CWMP_WORKER_PROCESSES", "1".to_string()),
        ("GENIEACS_NBI_WORKER_PROCESSES", "1".to_string()),
        ("GENIEACS_CWMP_PORT", cwmp.to_string()),
        ("GENIEACS_CWMP_INTERFACE", "127.0.0.1".to_string()),
        ("GENIEACS_NBI_PORT", nbi.to_string()),
        ("GENIEACS_NBI_INTERFACE", "127.0.0.1".to_string()),
    ];
    let cwmp_proc = spawn(
        "node",
        &[&format!("{home}/bin/genieacs-cwmp")],
        &common,
        &dir.path().join("cwmp.log"),
    );
    let nbi_proc = spawn(
        "node",
        &[&format!("{home}/bin/genieacs-nbi")],
        &common,
        &dir.path().join("nbi.log"),
    );
    let acs = Acs {
        _procs: vec![mongod, cwmp_proc, nbi_proc],
        cwmp,
        nbi,
        dir,
    };
    let up = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let nbi_up = http()
                .get(format!("http://127.0.0.1:{nbi}/devices/"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if nbi_up
                && tokio::net::TcpStream::connect(("127.0.0.1", cwmp))
                    .await
                    .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    if up.is_err() {
        panic!("GenieACS never came up:\n{}", acs.logs());
    }
    acs
}

impl Acs {
    fn logs(&self) -> String {
        ["mongod.log", "cwmp.log", "nbi.log"]
            .iter()
            .map(|f| {
                format!(
                    "== {f}\n{}",
                    std::fs::read_to_string(self.dir.path().join(f)).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
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

/// Queue a task with a connection request; GenieACS answers 200 once the device ran it.
async fn task(acs: &Acs, device: &str, body: Value) -> (u16, String) {
    let r = http()
        // The device id has no characters a path needs escaped.
        .post(format!(
            "http://127.0.0.1:{}/devices/{device}/tasks?connection_request&timeout=20000",
            acs.nbi
        ))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap_or_default())
}

async fn device_doc(acs: &Acs, device: &str) -> Value {
    let r = http()
        .get(format!("http://127.0.0.1:{}/devices/", acs.nbi))
        .query(&[("query", json!({"_id": device}).to_string())])
        .send()
        .await
        .unwrap();
    let docs: Value = r.json().await.unwrap();
    docs[0].clone()
}

#[tokio::test]
async fn netget_is_a_device_genieacs_manages() {
    let acs = genieacs().await;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "tr069".into(),
        remote_addr: Some(format!("http://127.0.0.1:{}/", acs.cwmp)),
        instruction: Some("Be a small home gateway".into()),
        startup_params: Some(json!({"serial_number": "NETGET0042", "events": ["0 BOOTSTRAP", "1 BOOT"]})),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");
    let device = "4E4554-NetGetCPE-NETGET0042";

    let boot = wait_event(&state, id, |e| e["event_type"] == "tr069_session").await;
    assert_eq!(boot["ok"], true, "{boot}\n{}", acs.logs());
    assert_eq!(boot["events"], json!(["0 BOOTSTRAP", "1 BOOT"]));
    let doc = device_doc(&acs, device).await;
    assert_eq!(
        doc["_id"],
        device,
        "GenieACS did not register the device: {doc}\n{}",
        acs.logs()
    );
    assert!(
        doc["Device"]["ManagementServer"]["ConnectionRequestURL"]["_value"]
            .as_str()
            .unwrap_or_default()
            .starts_with("http://127.0.0.1:"),
        "{doc}"
    );

    // A read: GenieACS knocks on the connection-request URL, NetGet opens a session and
    // answers what it is asked; the value lands in GenieACS's database.
    let (status, body) = task(&acs, device, json!({"name": "getParameterValues", "parameterNames": ["Device.DeviceInfo.SoftwareVersion"]})).await;
    assert_eq!(
        status,
        200,
        "the task did not complete: {body}\n{}",
        acs.logs()
    );
    let cr = wait_event(&state, id, |e| {
        e["event_type"] == "tr069_session"
            && e["request"]["events"] == json!(["6 CONNECTION REQUEST"])
    })
    .await;
    assert_eq!(cr["ok"], true, "{cr}");
    let doc = device_doc(&acs, device).await;
    assert_eq!(
        doc["Device"]["DeviceInfo"]["SoftwareVersion"]["_value"], "2.1",
        "{doc}"
    );

    // A write: GenieACS sends SetParameterValues with the value and type NBI was given.
    let (status, body) = task(
        &acs,
        device,
        json!({"name": "setParameterValues", "parameterValues": [["Device.ManagementServer.PeriodicInformInterval", 600, "xsd:unsignedInt"]]}),
    )
    .await;
    assert_eq!(
        status,
        200,
        "the task did not complete: {body}\n{}",
        acs.logs()
    );
    let set = wait_event(&state, id, |e| {
        e["event_type"] == "tr069_rpc" && e["request"]["method"] == "SetParameterValues"
    })
    .await;
    assert_eq!(
        set["arguments"]["parameters"],
        json!([{"name": "Device.ManagementServer.PeriodicInformInterval", "value": "600", "type": "xsd:unsignedInt"}]),
        "{set}"
    );
    let doc = device_doc(&acs, device).await;
    assert_eq!(
        doc["Device"]["ManagementServer"]["PeriodicInformInterval"]["_value"], 600,
        "{doc}"
    );
}
