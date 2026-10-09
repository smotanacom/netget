#![allow(dead_code)]
use netget::{
    cli::management::ClientForm,
    state::{AccessLogOwner, AppState, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
pub fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}
pub fn static_handler(event: &str, actions: Value) -> Value {
    json!({"event_pattern":event,"handler":{"type":"static","actions":actions}})
}
pub async fn refused_start(state: &AppState, addr: String, params: Value) -> String {
    let (tx, _) = mpsc::unbounded_channel();
    ClientForm {
        protocol: "vault".into(),
        remote_addr: Some(addr),
        startup_params: Some(params),
        event_handlers: Some(vec![static_handler("*", json!([]))]),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect_err("startup must refuse before client becomes available")
    .to_string()
}
pub async fn client(
    state: &AppState,
    addr: String,
    params: Value,
    handlers: Vec<Value>,
) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "vault".into(),
        remote_addr: Some(addr),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(id).await {
            if state.get_client(id).await.is_some_and(|client| {
                matches!(client.status, netget::state::ClientStatus::Error(_))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    id
}
pub async fn connected_client(state: &AppState, addr: String) -> ClientId {
    let id = client(state, addr, json!({}), vec![static_handler("*", json!([]))]).await;
    event(state, id, "vault_connected", 0).await;
    id
}
pub async fn latest(state: &AppState, id: ClientId) -> u64 {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| e.id)
        .max()
        .unwrap_or(0)
}
pub async fn event(state: &AppState, id: ClientId, name: &str, after: u64) -> (u64, Value) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.id > after && e.event_type == name)
                .min_by_key(|e| e.id)
            {
                return (e.id, e.request.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match result {
        Ok(event) => event,
        Err(_) => panic!(
            "Vault event {name} after {after}; events/errors: {:?}",
            state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .take(12)
                .map(|e| (e.id, &e.event_type, e.request.get("error")))
                .collect::<Vec<_>>()
        ),
    }
}
pub async fn send(state: &AppState, id: ClientId, mut action: Value) {
    if action.get("type").is_none() {
        action["type"] = json!("vault_request");
    }
    let outcome = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Executed { .. }
        ),
        "{outcome:?}"
    );
}
pub async fn request(state: &AppState, id: ClientId, action: Value) -> Value {
    let after = latest(state, id).await;
    send(state, id, action).await;
    event(state, id, "vault_response", after).await.1
}

pub async fn rejected(state: &AppState, id: ClientId, action: Value, needle: &str) {
    let result = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    match result {
        netget::state::client_handles::ClientSendOutcome::Rejected { error } => {
            assert!(error.contains(needle), "{error}")
        }
        other => panic!("{other:?}"),
    }
}
pub async fn failed(state: &AppState, id: ClientId, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let netget::state::ClientStatus::Error(e) =
                state.get_client(id).await.unwrap().status
            {
                assert!(e.contains(needle), "{e}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

pub const TOKEN: &str = "hvs.fixture-session-token-59";
pub const PASSWORD: &str = "fixture-login-password-59";
pub const TIME: &str = "2026-10-02T12:34:56.123456Z";
pub fn envelope(data: Value) -> Value {
    json!({"request_id":"fixture-request","lease_id":"","lease_duration":0,"renewable":false,"data":data,"auth":null,"wrap_info":null,"warnings":null})
}
pub fn seal() -> Value {
    json!({"type":"shamir","initialized":true,"sealed":false,"t":1,"n":1,"progress":0,"nonce":"","version":"2.0.0","cluster_name":"fixture","cluster_id":"fixture-cluster","storage_type":"inmem"})
}
pub fn health() -> Value {
    json!({"initialized":true,"sealed":false,"standby":false,"performance_standby":false,"server_time_utc":1700000000,"version":"2.0.0"})
}
pub fn version() -> Value {
    json!({"version":2,"created_time":TIME,"deletion_time":"","destroyed":false,"custom_metadata":null})
}
pub fn authentication() -> Value {
    let mut v = envelope(json!(null));
    v["auth"] = json!({"client_token":TOKEN,"accessor":"fixture-accessor","policies":["default","fixture"],"token_policies":["default","fixture"],"metadata":{"username":"reader"},"lease_duration":3600,"renewable":true,"entity_id":"fixture-entity","token_type":"service","orphan":true,"num_uses":0,"mfa_requirement":null});
    v
}
#[derive(Clone)]
pub struct Seen {
    pub path: String,
    pub method: String,
    pub token: Option<String>,
    pub body: Value,
}
pub struct MockPeer {
    pub address: String,
    mode: std::sync::Arc<std::sync::Mutex<String>>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MockPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MockPeer {
    pub fn mode(&self, mode: &str) {
        *self.mode.lock().unwrap() = mode.into();
    }
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    pub async fn wait_for(&self, path: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.seen().iter().any(|r| r.path.starts_with(path)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("mock peer must observe the requested route");
    }
    pub async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
pub async fn mock_peer(initial: &str) -> MockPeer {
    use http_body_util::{BodyExt, Full, Limited, StreamBody};
    use hyper::{body::Frame, service::service_fn, Response};
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };
    type Body = http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, Infallible>;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mode = Arc::new(Mutex::new(initial.to_owned()));
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let (mode_task, seen_task) = (mode.clone(), seen.clone());
    let task = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted=listener.accept()=>{
                    let (socket,_)=accepted.unwrap();
                    let (mode,seen)=(mode_task.clone(),seen_task.clone());
                    peers.spawn(async move {
                        let service=service_fn(move |request:hyper::Request<hyper::body::Incoming>| {
                            let (mode,seen)=(mode.clone(),seen.clone());
                            async move {
                                let path=request.uri().to_string();
                                let method=request.method().to_string();
                                let token=request.headers().get("X-Vault-Token").map(|v|v.to_str().unwrap().to_owned());
                                let bytes=Limited::new(request.into_body(),1024*1024).collect().await.unwrap().to_bytes();
                                let body=serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                                { let mut seen=seen.lock().unwrap();assert!(seen.len()<64,"mock request bound");seen.push(Seen {path:path.clone(),method:method.clone(),token:token.clone(),body}); }
                                let mode=mode.lock().unwrap().clone();
                                let mut status=200;let mut content_type="application/json";let mut encoding="identity";
                                let value=if path=="/v1/sys/seal-status" {
                                    let mut v=seal();
                                    if mode=="probe_reflect" {v["version"]=json!(TOKEN);}
                                    if mode=="probe_invalid" {v["t"]=json!(TOKEN);}
                                    v
                                }
                                else if path.starts_with("/v1/auth/") {
                                    let mut v=authentication();
                                    if mode=="bad_login" {status=403;v=json!({"errors":[format!("reflected {PASSWORD} {TOKEN}")]});}
                                    if mode=="mfa" {v["auth"]["mfa_requirement"]=json!({"mfa_request_id":"challenge"});}
                                    if mode=="reflect" {v["auth"]["metadata"][TOKEN]=json!(PASSWORD);v["warnings"]=json!([format!("reflected {PASSWORD} {TOKEN}")]);}
                                    v
                                } else if path=="/v1/sys/health" {
                                    if mode=="stall" {std::future::pending::<()>().await;}
                                    if let Some(code)=mode.strip_prefix("health_") {status=code.parse().unwrap();}
                                    if mode=="http_error" {status=403;json!({"errors":[format!("reflected {PASSWORD} {TOKEN}")]})}
                                    else if mode=="http_multi" {status=403;json!({"errors":["first","second"]})}
                                    else if mode=="http_empty" {status=404;json!({"errors":[]})}
                                    else if mode=="malformed" {json!({"sealed":"false"})}
                                    else if mode=="redirect" {status=307;json!({"errors":["redirect refused"]})}
                                    else {health()}
                                } else if path=="/v1/sys/leader" {json!({"ha_enabled":false,"is_self":false,"leader_address":"","leader_cluster_address":"","performance_standby":false})}
                                else if path.contains("/data/") {
                                    if token.is_none() {status=403;json!({"errors":["permission denied"]})}
                                    else if method=="PUT" {envelope(version())}
                                    else {envelope(json!({"data":{"answer":42,"note":PASSWORD,"reflected_token":TOKEN},"metadata":version()}))}
                                } else if path.contains("list=true") {envelope(json!({"keys":["app","folder/"]}))}
                                else {envelope(json!({"created_time":TIME,"updated_time":TIME,"current_version":2,"oldest_version":0,"max_versions":0,"cas_required":false,"delete_version_after":"0s","custom_metadata":null,"versions":{"2":{"created_time":TIME,"deletion_time":"","destroyed":false}}}))};
                                if path=="/v1/sys/health" && mode=="content_type" {content_type="text/plain";}
                                if path=="/v1/sys/health" && mode=="compressed" {encoding="gzip";}
                                let body:Body=if path=="/v1/sys/health" && mode=="stall_body" {
                                    use futures::StreamExt;
                                    StreamBody::new(futures::stream::once(async {Ok::<_,Infallible>(Frame::data(bytes::Bytes::from_static(b"{\"partial\":")))}).chain(futures::stream::pending())).boxed_unsync()
                                } else {
                                    let bytes=if path=="/v1/sys/health" && mode=="large" {vec![b'x';netget::client::vault::api::MAX_BODY+1]} else {serde_json::to_vec(&value).unwrap()};
                                    Full::new(bytes::Bytes::from(bytes)).boxed_unsync()
                                };
                                Ok::<_,Infallible>(Response::builder().status(status).header("Content-Type",content_type).header("Content-Encoding",encoding).header("Location","/v1/should-not-follow").body(body).unwrap())
                            }
                        });
                        let _=hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(socket),service).await;
                    });
                },
                finished=peers.join_next(),if !peers.is_empty()=>{let _=finished;}
            }
        }
    });
    MockPeer {
        address,
        mode,
        seen,
        task,
    }
}
