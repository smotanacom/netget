//! Redfish fixtures: a BMC policy, server and client through the shared forms, the pinned
//! gofish/redfishtool/mockup-server peers and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const BMC_SCRIPT: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
STATE='__STATE__'
if 'password' in e:
    ok = e['user_name']=='admin' and e['password']=='secret'
    print(json.dumps({'actions':[{'type':'redfish_login_accept' if ok else 'redfish_login_reject','role':'Administrator'}]})); sys.exit()
def load():
    try: return json.load(open(STATE))
    except Exception: return {'asset':''}
st=load()
p=e['path']; k=e['kind']
def res(r): return {'type':'redfish_resource','resource':r}
def coll(t,name,members): return res({'@odata.type':'#%s.%s'%(t,t),'Name':name,'Members':[{'@odata.id':m} for m in members]})
def err(n): return {'type':'redfish_error','error':n}
RESET='/redfish/v1/Systems/1/Actions/ComputerSystem.Reset'
ALLOWED=['On','ForceOff','ForceRestart','GracefulShutdown']
system={'@odata.type':'#ComputerSystem.v1_22_0.ComputerSystem','Id':'1','Name':'NetGet Server','PowerState':'On','Model':'NG-1','AssetTag':st['asset'],
 'ProcessorSummary':{'Count':2},'MemorySummary':{'TotalSystemMemoryGiB':64},'Status':{'State':'Enabled','Health':'OK'},
 'Links':{'Chassis':[{'@odata.id':'/redfish/v1/Chassis/1'}],'ManagedBy':[{'@odata.id':'/redfish/v1/Managers/bmc'}]},
 'Actions':{'#ComputerSystem.Reset':{'target':RESET,'ResetType@Redfish.AllowableValues':ALLOWED}}}
def sensor(i,n,r): return {'@odata.type':'#Sensor.v1_9_0.Sensor','Id':i,'Name':n,'Reading':r,'ReadingUnits':'Cel','ReadingType':'Temperature'}
if k=='read':
    a={'/redfish/v1/Systems':coll('ComputerSystemCollection','Computer System Collection',['/redfish/v1/Systems/1']),
       '/redfish/v1/Systems/1':res(system),
       '/redfish/v1/Chassis':coll('ChassisCollection','Chassis Collection',['/redfish/v1/Chassis/1']),
       '/redfish/v1/Chassis/1':res({'@odata.type':'#Chassis.v1_25_0.Chassis','Id':'1','Name':'Rack Chassis','ChassisType':'RackMount','PowerState':'On','Sensors':{'@odata.id':'/redfish/v1/Chassis/1/Sensors'}}),
       '/redfish/v1/Chassis/1/Sensors':coll('SensorCollection','Sensors',['/redfish/v1/Chassis/1/Sensors/CPU1Temp','/redfish/v1/Chassis/1/Sensors/InletTemp']),
       '/redfish/v1/Chassis/1/Sensors/CPU1Temp':res(sensor('CPU1Temp','CPU 1 Temperature',42.5)),
       '/redfish/v1/Chassis/1/Sensors/InletTemp':res(sensor('InletTemp','Inlet Temperature',21.0)),
       '/redfish/v1/Managers':coll('ManagerCollection','Manager Collection',['/redfish/v1/Managers/bmc']),
       '/redfish/v1/Managers/bmc':res({'@odata.type':'#Manager.v1_19_0.Manager','Id':'bmc','Name':'NetGet BMC','ManagerType':'BMC','FirmwareVersion':'1.0.0'}),
       '/redfish/v1/Broken':res({'Id':'x','Name':'no type'})}.get(p, err('resource_not_found'))
elif k=='update' and p=='/redfish/v1/Systems/1':
    body=e['body']
    if set(body)-{'AssetTag'}: a=err('property_not_writable')
    else:
        st['asset']=body['AssetTag']; json.dump(st,open(STATE,'w')); a={'type':'redfish_no_content'}
elif k=='action' and p==RESET:
    t=(e.get('body') or {}).get('ResetType')
    if t is None: a=err('action_parameter_missing')
    elif t not in ALLOWED: a=err('action_parameter_value_not_in_list')
    else: a={'type':'redfish_task','final_state':'Completed','complete_after_secs':1,'messages':['Reset %s completed'%t]}
elif k=='create' and p=='/redfish/v1/AccountService/Accounts':
    a={'type':'redfish_resource','resource':{'@odata.id':'/redfish/v1/AccountService/Accounts/3','@odata.type':'#ManagerAccount.v1_12_0.ManagerAccount','Id':'3','Name':'User Account','UserName':e['body'].get('UserName'),'RoleId':e['body'].get('RoleId','ReadOnly')}}
elif k=='delete' and p=='/redfish/v1/AccountService/Accounts/3':
    a={'type':'redfish_no_content'}
else:
    a=err('operation_not_allowed')
print(json.dumps({'actions':[a]}))
"#;

/// A one-server BMC: admin/secret logs in; Systems/1 (whose AssetTag PATCH persists in
/// `state`), a reset that runs as a one-second task, a chassis with two sensors, a manager,
/// account create/delete; anything else is resource_not_found.
pub(crate) fn bmc_policy(state: &std::path::Path) -> Vec<Value> {
    let code = BMC_SCRIPT.replace("__STATE__", &state.display().to_string());
    vec![
        json!({"event_pattern":"redfish_login","handler":{"type":"script","language":"python","code":code}}),
        json!({"event_pattern":"redfish_request","handler":{"type":"script","language":"python","code":code}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "redfish".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
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
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern":"redfish_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"redfish_response","handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "redfish".into(),
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
    tokio::time::timeout(Duration::from_secs(15), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("Redfish client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if rows.len() >= count {
                rows.sort_by_key(|e| e.id);
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

fn peer_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set from tests/server/redfish/install_peers.py (gofish v0.26.0, redfishtool 1.1.8, Redfish-Mockup-Server 1.3.0); this evidence never skips"))
}
pub(crate) fn gofish() -> String {
    peer_env("NETGET_REDFISH_GOFISH")
}
pub(crate) fn redfishtool() -> String {
    peer_env("NETGET_REDFISH_TOOL")
}
pub(crate) fn python() -> String {
    peer_env("NETGET_REDFISH_PYTHON")
}
pub(crate) fn mockup_dir() -> PathBuf {
    PathBuf::from(peer_env("NETGET_REDFISH_MOCKUP"))
}

/// DMTF's Redfish-Mockup-Server, unchanged, serving its bundled public-rackmount1 mockup on a
/// probed loopback port.
pub(crate) async fn start_mockup() -> super::E2EResult<super::real_server::RealServer> {
    let dir = mockup_dir();
    let script = dir.join("redfishMockupServer.py").display().to_string();
    let mockup = dir.join("public-rackmount1").display().to_string();
    super::real_server::RealServer::builder(
        &python(),
        super::real_server::InstallHint {
            brew: "python (then tests/server/redfish/install_peers.py)",
            apt: "python3 (then tests/server/redfish/install_peers.py)",
        },
    )
    .args([
        script.as_str(),
        "-D",
        mockup.as_str(),
        "-S",
        "-H",
        "127.0.0.1",
        "-p",
        "{port}",
    ])
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
}
