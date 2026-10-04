//! OCPP fixtures: a CSMS policy, a charge-point script, server and client through the shared
//! forms, the pinned python ocpp peer and access-log waits.
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

/// Answers every core call of either version; DataTransfer is NotSupported.
pub(crate) fn csms_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"ocpp_call","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys,datetime\n",
            "e=json.load(sys.stdin)['event']\n",
            "now=datetime.datetime.now(datetime.timezone.utc).isoformat()\n",
            "a=e['action']; v=e['ocpp_version']\n",
            "r={'BootNotification':{'status':'Accepted','currentTime':now,'interval':10},'Heartbeat':{'currentTime':now},",
            "   'StatusNotification':{},'MeterValues':{},'StopTransaction':{},'TransactionEvent':{},",
            "   'StartTransaction':{'transactionId':7,'idTagInfo':{'status':'Accepted'}},",
            "   'Authorize':{'idTagInfo':{'status':'Accepted'}} if v=='1.6' else {'idTokenInfo':{'status':'Accepted'}}}.get(a)\n",
            "out={'type':'ocpp_call_result','payload':r} if r is not None else {'type':'ocpp_call_error','code':'NotSupported','description':'not offered'}\n",
            "print(json.dumps({'actions':[out]}))\n"
        )}}),
        json!({"event_pattern":"ocpp_call_response","handler":{"type":"static","actions":[]}}),
    ]
}

/// A charge point that walks the 1.6 or 2.0.1 core workflow from its own events and accepts
/// every central-system call.
pub(crate) fn charge_point_policy(version: &str) -> Vec<Value> {
    let chain = if version == "1.6" {
        concat!(
            "import json,sys,datetime\n",
            "e=json.load(sys.stdin)['event']\n",
            "now=datetime.datetime.now(datetime.timezone.utc).isoformat()\n",
            "nxt={None:('BootNotification',{'chargePointVendor':'NetGet','chargePointModel':'Sim-1'}),\n",
            " 'BootNotification':('Heartbeat',{}),'Heartbeat':('StatusNotification',{'connectorId':1,'errorCode':'NoError','status':'Available'}),\n",
            " 'StatusNotification':('Authorize',{'idTag':'TAG1'}),'Authorize':('StartTransaction',{'connectorId':1,'idTag':'TAG1','meterStart':0,'timestamp':now}),\n",
            " 'StartTransaction':('StopTransaction',{'transactionId':(e.get('payload') or {}).get('transactionId',0),'meterStop':900,'timestamp':now})}.get(e.get('action'))\n",
            "print(json.dumps({'actions':[{'type':'ocpp_call','action':nxt[0],'payload':nxt[1]}] if nxt else []}))\n"
        )
    } else {
        concat!(
            "import json,sys,datetime\n",
            "e=json.load(sys.stdin)['event']\n",
            "now=datetime.datetime.now(datetime.timezone.utc).isoformat()\n",
            "nxt={None:('BootNotification',{'chargingStation':{'model':'Sim-1','vendorName':'NetGet'},'reason':'PowerUp'}),\n",
            " 'BootNotification':('Heartbeat',{}),'Heartbeat':('StatusNotification',{'timestamp':now,'connectorStatus':'Available','evseId':1,'connectorId':1}),\n",
            " 'StatusNotification':('Authorize',{'idToken':{'idToken':'TAG1','type':'ISO14443'}}),\n",
            " 'Authorize':('TransactionEvent',{'eventType':'Started','timestamp':now,'triggerReason':'Authorized','seqNo':0,'transactionInfo':{'transactionId':'T9'}})}.get(e.get('action'))\n",
            "print(json.dumps({'actions':[{'type':'ocpp_call','action':nxt[0],'payload':nxt[1]}] if nxt else []}))\n"
        )
    };
    vec![
        json!({"event_pattern":"ocpp_connected","handler":{"type":"script","language":"python","code":chain}}),
        json!({"event_pattern":"ocpp_call_response","handler":{"type":"script","language":"python","code":chain}}),
        json!({"event_pattern":"ocpp_csms_call","handler":{"type":"static","actions":[{"type":"ocpp_call_result","payload":{"status":"Accepted"}}]}}),
    ]
}

pub(crate) fn quiet_charge_point() -> Vec<Value> {
    vec![
        json!({"event_pattern":"ocpp_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"ocpp_call_response","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"ocpp_csms_call","handler":{"type":"static","actions":[{"type":"ocpp_call_result","payload":{"status":"Accepted"}}]}}),
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
        protocol: "ocpp".into(),
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
    handlers: Vec<Value>,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "ocpp".into(),
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
    .map_err(|_| anyhow::anyhow!("OCPP client did not connect"))??;
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

pub(crate) fn python() -> String {
    std::env::var("NETGET_OCPP_PYTHON").expect("NETGET_OCPP_PYTHON must name the Python from tests/server/ocpp/install_peers.py (ocpp 2.1.0); this evidence never skips")
}
pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/ocpp/peer.py")
}
