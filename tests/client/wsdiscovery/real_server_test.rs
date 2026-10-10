//! NetGet's WS-Discovery client against two independent target services, both found on the
//! multicast group (239.255.255.250:3702), failing rather than skipping without them:
//!
//! - **wsdd**, the WSD host daemon Linux distributions ship so Windows sees Samba hosts
//!   (`apt-get install wsdd`). It answers a Probe for `wsdp:Device` without addresses and
//!   gives them on Resolve. (It sends Hello and Bye with `IP_MULTICAST_LOOP` off, so they never
//!   reach a listener on the same host; announcements are tested against python instead.)
//! - **python WSDiscovery** publishing one ONVIF-shaped service with a scope
//!   (`pip install WSDiscovery`; the interpreter is `NETGET_WSD_PYTHON`, default `python3`).
//!
//! Each test is a chain the model drives from what the peer said. wsdd's match without
//! addresses makes it resolve, and the ResolveMatch is wsdd's own answer, so the Resolve NetGet
//! sent named the endpoint wsdd matched. Python's Hello makes it probe; a scope that matches is
//! followed by one that does not, and only the first finds the service. No LLM calls.
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

const WSDD: InstallHint = InstallHint {
    brew: "wsdd",
    apt: "wsdd",
};
const DEVICE: &str = "{http://schemas.xmlsoap.org/ws/2006/02/devprof}Device";
const NVT: &str = "{http://www.onvif.org/ver10/network/wsdl}NetworkVideoTransmitter";
const WSDD_UUID: &str = "5e7d0c1a-0000-4000-8000-00000000d5dd";
const PUBLISHED: &str = "urn:uuid:cccccccc-0000-4000-8000-0000000000c3";
const OFFICE: &str = "onvif://www.onvif.org/location/office";

/// Every test here owns UDP 3702 on the group for its duration, so the answers it counts are
/// the ones its own peer gave.
static GROUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn client(
    remote: &str,
    params: Value,
    handlers: Vec<Value>,
) -> anyhow::Result<(AppState, ClientId)> {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "wsdiscovery".into(),
        remote_addr: Some(remote.into()),
        instruction: Some("Find services".into()),
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

fn has(matches: &Value, endpoint: &str) -> Option<Value> {
    matches["matches"]
        .as_array()?
        .iter()
        .find(|m| m["endpoint_reference"] == endpoint)
        .cloned()
}

/// A wsdd match without addresses → resolve it.
const ON_WSDD: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
ME='urn:uuid:5e7d0c1a-0000-4000-8000-00000000d5dd'
print(json.dumps({'actions':[{'type':'wsd_resolve','endpoint_reference':m['endpoint_reference'],'wait_ms':1500} for m in e['matches'] if m['endpoint_reference']==ME and not m['xaddrs']]}))"#;

#[tokio::test]
async fn netget_finds_and_resolves_wsdd() {
    let _group = GROUP.lock().await;
    let wsdd = RealServer::builder("wsdd", WSDD)
        .args(["-4", "-v", "-s", "-U", WSDD_UUID, "-n", "NETGETTEST"])
        .without_tcp_readiness()
        .ready_when_log_matches("joined multicast group 239.255.255.250")
        .startup_timeout(Duration::from_secs(20))
        .start()
        .await
        .expect("start wsdd");
    let me = format!("urn:uuid:{WSDD_UUID}");
    let (state, id) = client(
        "",
        json!({}),
        vec![
            json!({"event_pattern":"wsd_ready","handler":{"type":"static","actions":[
                {"type":"wsd_probe","types":["wsdp:Device"],"wait_ms":1500}]}}),
            json!({"event_pattern":"wsd_probe_matches","handler":{"type":"script","language":"python","code":ON_WSDD}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ],
    )
    .await
    .expect("connect");
    let ready = wait_match(&state, id, "wsd_ready", |_| true).await;
    assert_eq!(ready["target"], "239.255.255.250:3702", "{ready}");

    // wsdd matches `wsdp:Device` by its literal text, so being matched at all proves the Types
    // went out with the conventional prefix.
    let probe = wait_match(&state, id, "wsd_probe_matches", |e| has(e, &me).is_some()).await;
    let matched = has(&probe, &me).unwrap();
    assert!(
        matched["types"]
            .as_array()
            .unwrap()
            .contains(&json!(DEVICE)),
        "{probe}"
    );
    assert_eq!(matched["xaddrs"], json!([]), "{probe}\n{}", wsdd.log());
    // wsdd gives its address only on Resolve, so this is the answer to the model's resolve.
    let resolved = wait_match(&state, id, "wsd_resolve_matches", |e| {
        e["endpoint_reference"] == me
    })
    .await;
    let target = has(&resolved, &me).unwrap_or_else(|| panic!("{resolved}\n{}", wsdd.log()));
    let xaddr = target["xaddrs"][0].as_str().unwrap_or_default();
    assert!(
        xaddr.starts_with("http://") && xaddr.ends_with(&format!(":5357/{WSDD_UUID}")),
        "{resolved}"
    );
}

const PUBLISHER: &str = r#"import sys, time
from wsdiscovery import QName, Scope
from wsdiscovery.publishing import ThreadedWSPublishing
epr, ns_local, scope, xaddr = sys.argv[1:5]
ns, local = ns_local[1:].split('}')
p = ThreadedWSPublishing(uuid_=epr)
p.start()
p.publishService(types=[QName(ns, local)], scopes=[Scope(scope)], xAddrs=[xaddr])
print('READY', flush=True)
sys.stdin.read()
p.clearLocalServices()  # Bye
time.sleep(1)
"#;

/// The python interpreter with `wsdiscovery`, or a failure saying how to get one.
fn python() -> String {
    let python = std::env::var("NETGET_WSD_PYTHON").unwrap_or_else(|_| "python3".into());
    let python = find_binary(&python)
        .map(|p| p.display().to_string())
        .unwrap_or(python);
    let ok = std::process::Command::new(&python)
        .args(["-c", "import wsdiscovery"])
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok, "{python} cannot import wsdiscovery: pip install WSDiscovery, or point NETGET_WSD_PYTHON at a venv's python");
    python
}

async fn publisher() -> (tokio::process::Child, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("publish.py");
    std::fs::write(&script, PUBLISHER).unwrap();
    let mut child = tokio::process::Command::new(python())
        .args([
            "-I",
            script.to_str().unwrap(),
            PUBLISHED,
            NVT,
            OFFICE,
            "http://192.0.2.30/onvif/device_service",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await;
    assert!(
        matches!(&ready, Ok(Ok(Some(l))) if l == "READY"),
        "the WSDiscovery publisher did not start ({ready:?})"
    );
    (child, dir)
}

/// The publisher's Hello → a probe whose scope prefix-matches the service's; on that answer,
/// one whose scope does not.
const ON_PUBLISHER: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
ME='urn:uuid:cccccccc-0000-4000-8000-0000000000c3'
a=[]
if t=='wsd_announcement' and e['message']=='Hello' and (e.get('service') or {}).get('endpoint_reference')==ME:
  a=[{'type':'wsd_probe','types':['dn:NetworkVideoTransmitter'],'scopes':['onvif://www.onvif.org/location'],'wait_ms':1500}]
elif t=='wsd_probe_matches' and e['scopes']==['onvif://www.onvif.org/location']:
  a=[{'type':'wsd_probe','types':[e['types'][0]],'scopes':['onvif://www.onvif.org/location/kitchen'],'wait_ms':1500}]
print(json.dumps({'actions':a}))"#;

#[tokio::test]
async fn announcements_and_scopes_against_python_wsdiscovery() {
    let _group = GROUP.lock().await;
    // Listening before the publisher starts, so its Hello is heard.
    let (state, id) = client(
        "",
        json!({"version": "2005/04", "listen_announcements": true}),
        vec![
            json!({"event_pattern":"wsd_ready","handler":{"type":"static","actions":[]}}),
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":ON_PUBLISHER}}),
        ],
    )
    .await
    .expect("connect");
    wait_match(&state, id, "wsd_ready", |_| true).await;
    let (mut publisher, _dir) = publisher().await;
    let hello = wait_match(&state, id, "wsd_announcement", |e| {
        e["message"] == "Hello" && e["service"]["endpoint_reference"] == PUBLISHED
    })
    .await;
    assert_eq!(hello["service"]["types"], json!([NVT]), "{hello}");

    let first = wait_match(&state, id, "wsd_probe_matches", |e| {
        e["scopes"] == json!(["onvif://www.onvif.org/location"])
    })
    .await;
    let service = has(&first, PUBLISHED).unwrap_or_else(|| panic!("{first}"));
    assert_eq!(service["types"], json!([NVT]), "{first}");
    assert_eq!(service["scopes"], json!([OFFICE]), "{first}");
    assert_eq!(
        service["xaddrs"],
        json!(["http://192.0.2.30/onvif/device_service"]),
        "{first}"
    );
    // The second probe exists only because the model answered the first.
    let second = wait_match(&state, id, "wsd_probe_matches", |e| {
        e["scopes"] == json!(["onvif://www.onvif.org/location/kitchen"])
    })
    .await;
    assert!(has(&second, PUBLISHED).is_none(), "{second}");

    // An injected resolve, answered by the publisher with its address.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"wsd_resolve","endpoint_reference":PUBLISHED,"wait_ms":1500}),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}")
    };
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(
        has(&detail, PUBLISHED).map(|s| s["xaddrs"].clone()),
        Some(json!(["http://192.0.2.30/onvif/device_service"])),
        "{detail}"
    );
    // And one the client refuses before anything is sent.
    let refused = state
        .send_to_client(
            id,
            json!({"type":"wsd_probe","types":["nope:Thing"]}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(&refused, ClientSendOutcome::Rejected { error } if error.contains("nope")),
        "{refused:?}"
    );

    // Stopping the service: python says Bye, and the client hears it.
    drop(publisher.stdin.take());
    wait_match(&state, id, "wsd_announcement", |e| {
        e["message"] == "Bye" && e["service"]["endpoint_reference"] == PUBLISHED
    })
    .await;
    let _ = publisher.wait().await;
}
