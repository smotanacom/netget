//! LwM2M fixtures: server and device policies, both roles through the shared forms, the pinned
//! Leshan demos (`tests/server/lwm2m/install_peers.py`) and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use super::real_server::{InstallHint, RealServer};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const SERVER_POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='lwm2m_register':
    if e['endpoint']=='intruder': out({'type':'lwm2m_reject'})
    out({'type':'lwm2m_accept'},
        {'type':'lwm2m_read','path':'/3/0/0','format':'text'},
        {'type':'lwm2m_read','path':'/3303/0'},
        {'type':'lwm2m_write','path':'/3/0/14','value':'+02'},
        {'type':'lwm2m_read','path':'/3/0/14','format':'text'},
        {'type':'lwm2m_execute','path':'/3/0/12'},
        {'type':'lwm2m_discover','path':'/3/0'},
        {'type':'lwm2m_read','path':'/42/0'},
        {'type':'lwm2m_observe','path':'/3303/0/5700'})
out()
"#;

/// Accept every device but "intruder" and, on registration, read the manufacturer and the
/// temperature object, write and read back the UTC offset, reset the error code, discover the
/// device object, read an object it lacks, and observe the temperature.
pub(crate) fn server_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": SERVER_POLICY}}),
    ]
}

const DEVICE_POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']; STATE='__STATE__'
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
try: db=json.load(open(STATE))
except Exception: db={'/3/0/14':'+00'}
values={'/3/0/0':'NetGet Device','/3/0/1':'model-x','/3/0/14':db['/3/0/14'],'/3303/0/5700':21.5,'/3303/0/5701':'Cel','/1/0/0':1,'/1/0/1':30}
if k=='lwm2m_read_request':
    p=e['path']
    found=[{'path':q,'value':v} for q,v in values.items() if q==p or q.startswith(p+'/')]
    out({'type':'lwm2m_content','values':found} if found else {'type':'lwm2m_error','code':'not_found'})
if k=='lwm2m_write_request':
    for v in e['values']:
        if v['path']=='/3/0/14': db['/3/0/14']=v['value']
    json.dump(db,open(STATE,'w')); out({'type':'lwm2m_ok'})
if k=='lwm2m_execute_request':
    db['executed']=e['path']; json.dump(db,open(STATE,'w')); out({'type':'lwm2m_ok'})
out()
"#;

/// A device answering reads from fixed values (the UTC offset is writable), accepting executes.
pub(crate) fn device_policy(state_file: &std::path::Path) -> Vec<Value> {
    let code = DEVICE_POLICY.replace("__STATE__", &state_file.display().to_string());
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": code}}),
    ]
}

pub(crate) async fn server_in(state: &AppState) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "lwm2m".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(server_policy()),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (id, addr)
}

pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
    handlers: Vec<Value>,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "lwm2m".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("LwM2M client did not register"))??;
    Ok(id)
}

pub(crate) async fn wait_for(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    want: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .find(|e| e.event_type == kind && want(&e.request))
            {
                break e.request;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for a matching {kind} access-log entry"))
}

fn jar(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must name the jar from tests/server/lwm2m/install_peers.py (Leshan 2.0.0-M15); this evidence never skips"))
}

const JAVA: InstallHint = InstallHint {
    brew: "openjdk (then tests/server/lwm2m/install_peers.py)",
    apt: "openjdk-21-jre-headless (then tests/server/lwm2m/install_peers.py)",
};

/// The Leshan client demo registering with `server` as `endpoint`; stopped with SIGTERM so its
/// shutdown hook deregisters.
pub(crate) async fn start_leshan_client(
    server: SocketAddr,
    endpoint: &str,
) -> super::E2EResult<RealServer> {
    RealServer::builder("java", JAVA)
        .args([
            "-jar".to_owned(),
            jar("NETGET_LWM2M_LESHAN_CLIENT"),
            "-u".into(),
            format!("coap://{server}"),
            "-n".into(),
            endpoint.into(),
            "-lh".into(),
            "127.0.0.1".into(),
        ])
        .without_tcp_readiness()
        .ready_when_log_matches(&format!(r"client\[endpoint:{endpoint}\] started"))
        .graceful_stop(nix::sys::signal::Signal::SIGTERM, Duration::from_secs(10))
        .startup_timeout(Duration::from_secs(60))
        .start()
        .await
}

/// The Leshan server demo with every endpoint on loopback: CoAP on `{port}`, its REST API on
/// `{port4}`. (Its TLS endpoint takes the TCP endpoint's port, so it binds ::1 instead.)
pub(crate) async fn start_leshan_server() -> super::E2EResult<RealServer> {
    RealServer::builder("java", JAVA)
        .args([
            "-jar".to_owned(),
            jar("NETGET_LWM2M_LESHAN_SERVER"),
            "-lh".into(),
            "127.0.0.1".into(),
            "-lp".into(),
            "{port}".into(),
            "-slh".into(),
            "127.0.0.1".into(),
            "-slp".into(),
            "{port1}".into(),
            "-jh".into(),
            "127.0.0.1".into(),
            "-jp".into(),
            "{port2}".into(),
            "-th".into(),
            "127.0.0.1".into(),
            "-tp".into(),
            "{port3}".into(),
            "-tsh".into(),
            "::1".into(),
            "-wh".into(),
            "127.0.0.1".into(),
            "-wp".into(),
            "{port4}".into(),
        ])
        .extra_ports(4)
        .without_tcp_readiness()
        .ready_when_log_matches("Web server started")
        .startup_timeout(Duration::from_secs(60))
        .start()
        .await
}
