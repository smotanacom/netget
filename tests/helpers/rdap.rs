//! RDAP fixtures: a registry policy, server and client through the shared forms, the pinned
//! independent peers, and access-log waits.
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

/// A small registry: example.com, ns1.example.com, 192.0.2.0/24, AS64496, entity EX-REG,
/// a domain search, help, a referral for moved.example and 404 for everything else.
pub(crate) fn registry_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"rdap_query","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "t=e['query_type']; v=e.get('value')\n",
            "ev=[{'eventAction':'registration','eventDate':'1995-08-14T04:00:00Z'}]\n",
            "dom={'objectClassName':'domain','handle':'EX-1','ldhName':'example.com','status':['active'],'events':ev,",
            "     'nameservers':[{'objectClassName':'nameserver','ldhName':'ns1.example.com'}],",
            "     'entities':[{'objectClassName':'entity','handle':'EX-REG','roles':['registrant']}]}\n",
            "if t=='domain' and v=='example.com':\n    a={'object':dom}\n",
            "elif t=='domain' and v=='moved.example':\n    a={'redirect':'http://127.0.0.1:9/rdap/domain/moved.example'}\n",
            "elif t=='nameserver' and v=='ns1.example.com':\n    a={'object':{'objectClassName':'nameserver','handle':'NS-1','ldhName':'ns1.example.com','ipAddresses':{'v4':['192.0.2.53']}}}\n",
            "elif t=='ip' and v in ('192.0.2.0/24','192.0.2.7'):\n    a={'object':{'objectClassName':'ip network','handle':'NET-1','startAddress':'192.0.2.0','endAddress':'192.0.2.255','ipVersion':'v4','name':'TEST-NET-1'}}\n",
            "elif t=='autnum' and v=='64496':\n    a={'object':{'objectClassName':'autnum','handle':'AS64496','startAutnum':64496,'endAutnum':64496,'name':'DOC-AS'}}\n",
            "elif t=='entity' and v=='EX-REG':\n    a={'object':{'objectClassName':'entity','handle':'EX-REG','roles':['registrant']}}\n",
            "elif t=='domains':\n    a={'results':[dom]}\n",
            "elif t=='help':\n    a={'object':{'notices':[{'title':'NetGet RDAP','description':['A test registry']}]}}\n",
            "elif t=='autnum' and v=='64511':\n    a={'error':{'code':403,'title':'Forbidden','description':['private range']}}\n",
            "else:\n    a={'not_found':True}\n",
            "a['type']='rdap_response'\n",
            "print(json.dumps({'actions':[a]}))\n"
        )}}),
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
        protocol: "rdap".into(),
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
) -> ClientId {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "rdap".into(),
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
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            assert!(!matches!(
                state.get_client(id).await.map(|c| c.status),
                Some(ClientStatus::Error(_))
            ));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    id
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(20), async {
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

fn peer(var: &str, what: &str) -> PathBuf {
    PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var} must name {what} from tests/server/rdap/install_peers.sh; this evidence never skips")))
}
pub(crate) fn openrdap() -> PathBuf {
    peer("NETGET_OPENRDAP", "OpenRDAP 0.10.2 rdap")
}
pub(crate) fn icann_rdap() -> PathBuf {
    peer("NETGET_ICANN_RDAP", "ICANN rdap 1.0.0")
}
pub(crate) fn icann_rdap_srv() -> PathBuf {
    peer("NETGET_ICANN_RDAP_SRV", "ICANN rdap-srv 1.0.0")
}
pub(crate) fn icann_rdap_srv_data() -> PathBuf {
    peer("NETGET_ICANN_RDAP_SRV_DATA", "ICANN rdap-srv-data 1.0.0")
}
