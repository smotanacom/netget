//! DMTF Redfish service over HTTP/1.1. Rust serves the service root, the OData documents, the
//! SessionService and the TaskService and owns every envelope rule; the handler owns logins
//! and every other resource.
pub mod actions;
pub mod model;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub const DEFAULT_PRODUCT: &str = "NetGet Redfish Service";
pub const DEFAULT_VENDOR: &str = "NetGet";
pub const DEFAULT_UUID: &str = "6e657467-6574-4000-8000-726564666973";
pub const DEFAULT_AUTH: &str = "required";
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(1800);
pub const DEFAULT_TASK_SECS: u64 = 2;
const MAX_SESSIONS: usize = 64;
const MAX_TASKS: usize = 256;
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

struct Session {
    id: String,
    user: String,
    role: String,
    last_used: Instant,
}

struct Task {
    final_state: String,
    done_at: Instant,
    start: String,
    messages: Vec<Value>,
    result: Option<Value>,
}

struct Shared {
    ctx: SpawnContext,
    product: String,
    vendor: String,
    uuid: String,
    auth_required: bool,
    session_timeout: Duration,
    sessions: Mutex<HashMap<String, Session>>,
    basic: Mutex<HashMap<u64, Instant>>,
    basic_key: std::collections::hash_map::RandomState,
    tasks: Mutex<HashMap<String, Task>>,
    seq: AtomicU64,
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str, d: &str| -> anyhow::Result<String> {
        Ok(p.map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| d.to_owned()))
    };
    let product = s("product", DEFAULT_PRODUCT)?;
    let vendor = s("vendor", DEFAULT_VENDOR)?;
    let uuid = s("uuid", DEFAULT_UUID)?;
    anyhow::ensure!(
        product.len() <= 256 && vendor.len() <= 256,
        "product and vendor are at most 256 bytes"
    );
    anyhow::ensure!(uuid::Uuid::parse_str(&uuid).is_ok(), "uuid must be a UUID");
    let auth = s("auth", DEFAULT_AUTH)?;
    anyhow::ensure!(
        auth == "required" || auth == "none",
        "auth must be required or none"
    );
    let timeout = p
        .map(|p| p.get_optional_u64("session_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_SESSION_TIMEOUT.as_secs());
    anyhow::ensure!(
        (30..=86_400).contains(&timeout),
        "session_timeout_secs must be 30..=86400"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Redfish service at http://{local}{} (auth {auth})",
        model::ROOT
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        product,
        vendor,
        uuid,
        auth_required: auth == "required",
        session_timeout: Duration::from_secs(timeout),
        sessions: Mutex::default(),
        basic: Mutex::default(),
        basic_key: Default::default(),
        tasks: Mutex::default(),
        seq: AtomicU64::new(1),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "Redfish", Some(&shared.ctx.status_tx)).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: local,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: now,
                        status: ConnectionStatus::Active,
                        status_changed_at: now,
                        protocol_info: ProtocolConnectionInfo::empty(),
                    },
                )
                .await;
            let child = shared.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let svc_shared = child.clone();
                    let service = service_fn(move |req| {
                        let shared = svc_shared.clone();
                        async move { Ok::<_, Infallible>(handle(&shared, id, req).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(64)
                        .max_buf_size(32 * 1024);
                    if let Err(e) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("Redfish connection {id}: {e}"));
                    }
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

type Reply = Response<Full<Bytes>>;

fn reply(status: u16, body: Option<&Value>, headers: &[(&'static str, String)]) -> Reply {
    let bytes = body
        .map(|b| serde_json::to_vec(b).unwrap_or_default())
        .unwrap_or_default();
    let mut r = Response::new(Full::new(Bytes::from(bytes)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = r.headers_mut();
    if body.is_some() {
        h.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    h.insert("OData-Version", header::HeaderValue::from_static("4.0"));
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    for (k, v) in headers {
        if let Ok(v) = header::HeaderValue::from_str(v) {
            h.insert(*k, v);
        }
    }
    r
}

fn error(status: u16, message_id: &str, message: &str) -> Reply {
    reply(status, Some(&model::error_body(message_id, message)), &[])
}

fn named_error(name: &str) -> Reply {
    let (status, id, message) = model::error_named(name).expect("known error name");
    error(status, id, message)
}

fn not_allowed(allow: &'static str) -> Reply {
    let mut r = named_error("operation_not_allowed");
    r.headers_mut()
        .insert(header::ALLOW, header::HeaderValue::from_static(allow));
    r
}

fn unauthorized() -> Reply {
    let mut r = error(
        401,
        "NoValidSession",
        "There is no valid session established with the implementation.",
    );
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Basic realm=\"NetGet Redfish\""),
    );
    r
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("Redfish connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn handle(shared: &Arc<Shared>, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let ctx = &shared.ctx;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(req.uri().to_string().len() as u64),
            None,
            Some(1),
            None,
        )
        .await;
    let response = route(shared, id, req).await;
    let sent = hyper::body::Body::size_hint(response.body())
        .exact()
        .unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    response
}

fn service_root(shared: &Shared) -> Value {
    json!({
        "@odata.id": model::ROOT,
        "@odata.type": "#ServiceRoot.v1_16_1.ServiceRoot",
        "Id": "RootService",
        "Name": "Root Service",
        "RedfishVersion": model::REDFISH_VERSION,
        "UUID": shared.uuid,
        "Product": shared.product,
        "Vendor": shared.vendor,
        "Systems": model::link("/redfish/v1/Systems"),
        "Chassis": model::link("/redfish/v1/Chassis"),
        "Managers": model::link("/redfish/v1/Managers"),
        "AccountService": model::link("/redfish/v1/AccountService"),
        "SessionService": model::link("/redfish/v1/SessionService"),
        "TaskService": model::link("/redfish/v1/TaskService"),
        "Links": {"Sessions": model::link("/redfish/v1/SessionService/Sessions")},
        "ProtocolFeaturesSupported": {"ExpandQuery": {"ExpandAll": false}, "FilterQuery": false, "SelectQuery": false, "OnlyMemberQuery": false}
    })
}

const ODATA_SETS: &[(&str, &str)] = &[
    ("Systems", "/redfish/v1/Systems"),
    ("Chassis", "/redfish/v1/Chassis"),
    ("Managers", "/redfish/v1/Managers"),
    ("AccountService", "/redfish/v1/AccountService"),
    ("SessionService", "/redfish/v1/SessionService"),
    ("TaskService", "/redfish/v1/TaskService"),
];

fn metadata_xml() -> String {
    let mut refs = String::new();
    for ns in [
        "ServiceRoot_v1",
        "ComputerSystemCollection",
        "ComputerSystem_v1",
        "ChassisCollection",
        "Chassis_v1",
        "ManagerCollection",
        "Manager_v1",
        "SessionService_v1",
        "SessionCollection",
        "Session_v1",
        "TaskService_v1",
        "TaskCollection",
        "Task_v1",
        "AccountService_v1",
        "SensorCollection",
        "Sensor_v1",
    ] {
        let alias = ns.trim_end_matches("_v1");
        refs.push_str(&format!(
            "  <edmx:Reference Uri=\"http://redfish.dmtf.org/schemas/v1/{ns}.xml\">\n    <edmx:Include Namespace=\"{alias}\"/>\n  </edmx:Reference>\n"
        ));
    }
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<edmx:Edmx xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\" Version=\"4.0\">\n{refs}  <edmx:DataServices>\n    <Schema xmlns=\"http://docs.oasis-open.org/odata/ns/edm\" Namespace=\"Service\">\n      <EntityContainer Name=\"Service\" Extends=\"ServiceRoot.v1_16_1.ServiceContainer\"/>\n    </Schema>\n  </edmx:DataServices>\n</edmx:Edmx>\n")
}

/// Who is asking, or the 401 to send. `Ok(None)` when authentication is off.
async fn authenticate(
    shared: &Shared,
    id: ConnectionId,
    req: &Request<Incoming>,
) -> Result<Option<String>, Reply> {
    if !shared.auth_required {
        return Ok(None);
    }
    if let Some(token) = req
        .headers()
        .get("X-Auth-Token")
        .and_then(|v| v.to_str().ok())
    {
        let mut sessions = shared.sessions.lock().await;
        sessions.retain(|_, s| s.last_used.elapsed() < shared.session_timeout);
        return match sessions.get_mut(token) {
            Some(s) => {
                s.last_used = Instant::now();
                Ok(Some(s.user.clone()))
            }
            None => Err(unauthorized()),
        };
    }
    let basic = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .ok()
        })
        .and_then(|b| String::from_utf8(b).ok());
    let Some((user, password)) = basic.as_deref().and_then(|b| b.split_once(':')) else {
        return Err(unauthorized());
    };
    let key = {
        let mut h = shared.basic_key.build_hasher();
        h.write(user.as_bytes());
        h.write_u8(0);
        h.write(password.as_bytes());
        h.finish()
    };
    {
        let mut cache = shared.basic.lock().await;
        cache.retain(|_, until| until.elapsed() < shared.session_timeout);
        if cache.contains_key(&key) {
            return Ok(Some(user.to_owned()));
        }
    }
    match login(shared, id, user, password, "basic").await {
        Some(_) => {
            let mut cache = shared.basic.lock().await;
            if cache.len() < MAX_SESSIONS {
                cache.insert(key, Instant::now());
            }
            Ok(Some(user.to_owned()))
        }
        None => Err(unauthorized()),
    }
}

/// Ask the handler about credentials. `Some(role)` only on an explicit accept: silence, a
/// failure or anything invalid is a refusal.
async fn login(
    shared: &Shared,
    id: ConnectionId,
    user: &str,
    password: &str,
    method: &str,
) -> Option<String> {
    let ctx = &shared.ctx;
    let event = Event::new(
        &actions::LOGIN_EVENT,
        json!({"user_name": user, "password": password, "method": method}),
    );
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RedfishProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            outcome(ctx, id, "login", "fail_closed_llm_error");
            return None;
        }
    };
    let answers = answers(
        result.protocol_results,
        &["redfish_login_accept", "redfish_login_reject"],
    );
    match answers.as_slice() {
        [a] if a["type"] == "redfish_login_accept" && result.failures.is_empty() => {
            outcome(ctx, id, "login", "model_answer");
            Some(a["role"].as_str().unwrap_or("Administrator").to_owned())
        }
        [a] if a["type"] == "redfish_login_reject" => {
            outcome(ctx, id, "login", "model_reject");
            None
        }
        [] => {
            outcome(ctx, id, "login", "model_silent");
            None
        }
        _ => {
            outcome(ctx, id, "login", "fail_closed_invalid_reply");
            None
        }
    }
}

fn answers(results: Vec<ActionResult>, allowed: &[&str]) -> Vec<Value> {
    let mut out = Vec::new();
    for r in results {
        match r {
            ActionResult::Custom { name, data } if allowed.contains(&name.as_str()) => {
                out.push(data)
            }
            ActionResult::Multiple(items) => out.extend(answers(items, allowed)),
            _ => {}
        }
    }
    out
}

async fn read_json(req: Request<Incoming>) -> Result<Value, Reply> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.starts_with("application/json") {
        return Err(error(
            415,
            "GeneralError",
            "The request body must be application/json.",
        ));
    }
    let body = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(req.into_body(), model::MAX_BODY_BYTES).collect(),
    )
    .await
    {
        Ok(Ok(b)) => b.to_bytes(),
        _ => {
            return Err(error(
                413,
                "GeneralError",
                "The request body is unreadable or over 1 MiB.",
            ))
        }
    };
    match serde_json::from_slice::<Value>(&body) {
        Ok(v @ Value::Object(_)) if model::budget_ok(&v) => Ok(v),
        _ => Err(error(400, "MalformedJSON", "The request body submitted was malformed JSON and could not be parsed by the receiving service.")),
    }
}

async fn route(shared: &Arc<Shared>, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let raw = req.uri().path().to_owned();
    let method = req.method().clone();
    if raw == "/redfish" || raw == "/redfish/" {
        return match method {
            Method::GET => reply(200, Some(&json!({"v1": model::ROOT})), &[]),
            _ => not_allowed("GET"),
        };
    }
    let Some(path) = model::normalize(&raw) else {
        return named_error("resource_not_found");
    };
    if let Some(v) = req
        .headers()
        .get("OData-Version")
        .and_then(|v| v.to_str().ok())
    {
        if v.trim() != "4.0" {
            return error(412, "GeneralError", "Only OData-Version 4.0 is supported.");
        }
    }
    // Unauthenticated documents.
    match (path.as_str(), &method) {
        (model::ROOT, &Method::GET) => return reply(200, Some(&service_root(shared)), &[]),
        (model::ROOT, _) => return not_allowed("GET"),
        ("/redfish/v1/odata", &Method::GET) => {
            let sets: Vec<Value> = ODATA_SETS
                .iter()
                .map(|(n, u)| json!({"name": n, "kind": "Singleton", "url": u}))
                .chain([json!({"name": "Service", "kind": "Singleton", "url": model::ROOT})])
                .collect();
            return reply(
                200,
                Some(&json!({"@odata.context": "/redfish/v1/$metadata", "value": sets})),
                &[],
            );
        }
        ("/redfish/v1/$metadata", &Method::GET) => {
            let mut r = Response::new(Full::new(Bytes::from(metadata_xml())));
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/xml"),
            );
            r.headers_mut()
                .insert("OData-Version", header::HeaderValue::from_static("4.0"));
            return r;
        }
        ("/redfish/v1/SessionService/Sessions", &Method::POST) => {
            return create_session(shared, id, req).await
        }
        _ => {}
    }
    let user = match authenticate(shared, id, &req).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    if let Some(r) = rust_owned(shared, &path, &method).await {
        return r;
    }
    let if_match = req
        .headers()
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let (kind, body) = match method {
        Method::GET => ("read", None),
        Method::DELETE => ("delete", None),
        Method::PATCH | Method::POST => {
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let kind = if method == Method::PATCH {
                "update"
            } else if model::split_action(&path).is_some() {
                "action"
            } else {
                "create"
            };
            (kind, Some(body))
        }
        _ => return not_allowed("GET, PATCH, POST, DELETE"),
    };
    let mut data = json!({"method": method.as_str(), "kind": kind, "path": path, "body": body, "user_name": user, "if_match": if_match});
    if let Some((resource, action)) = model::split_action(&path) {
        data["action"] = json!(action);
        data["resource_path"] = json!(resource);
    }
    resource_request(shared, id, &path, kind, data).await
}

async fn create_session(shared: &Arc<Shared>, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let body = match read_json(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let field = |k: &str| {
        body[k]
            .as_str()
            .filter(|s| s.len() <= 256)
            .map(str::to_owned)
    };
    let (Some(user), Some(password)) = (field("UserName"), field("Password")) else {
        return error(
            400,
            "PropertyMissing",
            "UserName and Password are required.",
        );
    };
    if !shared.auth_required {
        return error(
            405,
            "OperationNotAllowed",
            "Authentication is not enabled on this service.",
        );
    }
    if shared.sessions.lock().await.len() >= MAX_SESSIONS {
        return error(503, "SessionLimitExceeded", "The session establishment failed due to the number of simultaneous sessions exceeding the limit of the implementation.");
    }
    let Some(role) = login(shared, id, &user, &password, "session").await else {
        return unauthorized();
    };
    let sid = shared.seq.fetch_add(1, Ordering::Relaxed).to_string();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let location = format!("/redfish/v1/SessionService/Sessions/{sid}");
    let resource = session_resource(&sid, &user, &role);
    shared.sessions.lock().await.insert(
        token.clone(),
        Session {
            id: sid,
            user,
            role,
            last_used: Instant::now(),
        },
    );
    reply(
        201,
        Some(&resource),
        &[("X-Auth-Token", token), ("Location", location)],
    )
}

fn session_resource(id: &str, user: &str, role: &str) -> Value {
    json!({
        "@odata.id": format!("/redfish/v1/SessionService/Sessions/{id}"),
        "@odata.type": "#Session.v1_7_1.Session",
        "Id": id,
        "Name": "User Session",
        "UserName": user,
        "Roles": [role],
    })
}

/// The SessionService and TaskService, which Rust serves itself. `None` for everything else.
async fn rust_owned(shared: &Shared, path: &str, method: &Method) -> Option<Reply> {
    const SESSIONS: &str = "/redfish/v1/SessionService/Sessions";
    const TASKS: &str = "/redfish/v1/TaskService/Tasks";
    const MONITORS: &str = "/redfish/v1/TaskService/TaskMonitors";
    let get = method == Method::GET;
    Some(match path {
        "/redfish/v1/SessionService" if get => reply(
            200,
            Some(&json!({
                "@odata.id": "/redfish/v1/SessionService",
                "@odata.type": "#SessionService.v1_1_9.SessionService",
                "Id": "SessionService",
                "Name": "Session Service",
                "ServiceEnabled": true,
                "SessionTimeout": shared.session_timeout.as_secs(),
                "Sessions": model::link(SESSIONS),
            })),
            &[],
        ),
        "/redfish/v1/SessionService" => not_allowed("GET"),
        SESSIONS if get => {
            let mut sessions = shared.sessions.lock().await;
            sessions.retain(|_, s| s.last_used.elapsed() < shared.session_timeout);
            let mut ids: Vec<&String> = sessions.values().map(|s| &s.id).collect();
            ids.sort();
            let members = ids
                .into_iter()
                .map(|i| model::link(&format!("{SESSIONS}/{i}")))
                .collect();
            reply(
                200,
                Some(&model::collection(
                    SESSIONS,
                    "SessionCollection",
                    "Session Collection",
                    members,
                )),
                &[],
            )
        }
        SESSIONS => not_allowed("GET, POST"),
        p if p.starts_with(&format!("{SESSIONS}/")) => {
            let sid = &p[SESSIONS.len() + 1..];
            let mut sessions = shared.sessions.lock().await;
            let token = sessions
                .iter()
                .find(|(_, s)| s.id == sid)
                .map(|(t, _)| t.clone());
            match (token, method) {
                (None, _) => named_error("resource_not_found"),
                (Some(t), &Method::GET) => {
                    let s = &sessions[&t];
                    reply(200, Some(&session_resource(&s.id, &s.user, &s.role)), &[])
                }
                (Some(t), &Method::DELETE) => {
                    sessions.remove(&t);
                    reply(204, None, &[])
                }
                _ => not_allowed("GET, DELETE"),
            }
        }
        "/redfish/v1/TaskService" if get => reply(
            200,
            Some(&json!({
                "@odata.id": "/redfish/v1/TaskService",
                "@odata.type": "#TaskService.v1_2_1.TaskService",
                "Id": "TaskService",
                "Name": "Task Service",
                "ServiceEnabled": true,
                "CompletedTaskOverWritePolicy": "Oldest",
                "Tasks": model::link(TASKS),
            })),
            &[],
        ),
        "/redfish/v1/TaskService" => not_allowed("GET"),
        TASKS if get => {
            let tasks = shared.tasks.lock().await;
            let mut ids: Vec<&String> = tasks.keys().collect();
            ids.sort_by_key(|i| i.parse::<u64>().unwrap_or(0));
            let members = ids
                .into_iter()
                .map(|i| model::link(&format!("{TASKS}/{i}")))
                .collect();
            reply(
                200,
                Some(&model::collection(
                    TASKS,
                    "TaskCollection",
                    "Task Collection",
                    members,
                )),
                &[],
            )
        }
        TASKS => not_allowed("GET"),
        p if p.starts_with(&format!("{TASKS}/")) || p.starts_with(&format!("{MONITORS}/")) => {
            let monitor = p.starts_with(MONITORS);
            let tid = p.rsplit('/').next().unwrap_or("");
            if !get {
                return Some(not_allowed("GET"));
            }
            let tasks = shared.tasks.lock().await;
            let Some(t) = tasks.get(tid) else {
                return Some(named_error("resource_not_found"));
            };
            let done = Instant::now() >= t.done_at;
            let (state, percent) = if done {
                (t.final_state.as_str(), 100)
            } else {
                ("Running", 50)
            };
            let resource = model::task_resource(
                tid,
                state,
                percent,
                if done { &t.messages } else { &[] },
                &t.start,
            );
            if !monitor {
                reply(200, Some(&resource), &[])
            } else if !done {
                reply(
                    202,
                    Some(&resource),
                    &[("Location", p.to_owned()), ("Retry-After", "1".into())],
                )
            } else {
                match &t.result {
                    Some(body) => reply(200, Some(body), &[]),
                    None => reply(204, None, &[]),
                }
            }
        }
        _ => return None,
    })
}

async fn resource_request(
    shared: &Arc<Shared>,
    id: ConnectionId,
    path: &str,
    kind: &str,
    data: Value,
) -> Reply {
    let ctx = &shared.ctx;
    let failed = |decision: &str, e: anyhow::Error| {
        outcome(ctx, id, kind, decision);
        let failure = crate::utils::wire_failure::WireFailure::classify(&e);
        if failure.is_overloaded() {
            error(503, "ServiceTemporarilyUnavailable", failure.text())
        } else {
            error(500, "GeneralError", failure.text())
        }
    };
    let event = Event::new(&actions::REQUEST_EVENT, data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RedfishProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return failed("fail_closed_llm_error", e),
    };
    if !result.failures.is_empty() {
        return failed(
            "fail_closed_invalid_reply",
            anyhow::anyhow!("invalid handler answer"),
        );
    }
    let list = answers(
        result.protocol_results,
        &[
            "redfish_resource",
            "redfish_no_content",
            "redfish_task",
            "redfish_error",
        ],
    );
    let answer = match list.as_slice() {
        [a] => a.clone(),
        [] => return failed("model_silent", anyhow::anyhow!("no handler answer")),
        _ => {
            return failed(
                "fail_closed_invalid_reply",
                anyhow::anyhow!("more than one answer"),
            )
        }
    };
    let built: anyhow::Result<(Reply, &str)> = async {
        actions::check_answer(&answer)?;
        match (answer["type"].as_str().unwrap_or_default(), kind) {
            ("redfish_error", _) => {
                let (status, mid, default) =
                    model::error_named(answer["error"].as_str().unwrap_or_default())
                        .expect("checked");
                Ok((
                    error(status, mid, answer["message"].as_str().unwrap_or(default)),
                    "model_reject",
                ))
            }
            ("redfish_resource", "read" | "update") => {
                let mut resource = answer["resource"].clone();
                model::check_resource(path, &mut resource)?;
                Ok((reply(200, Some(&resource), &[]), "model_answer"))
            }
            ("redfish_resource", "create") => {
                let mut resource = answer["resource"].clone();
                let rid = resource["@odata.id"]
                    .as_str()
                    .and_then(model::normalize)
                    .ok_or_else(|| anyhow::anyhow!("a created resource needs its @odata.id"))?;
                anyhow::ensure!(
                    rid.strip_prefix(&format!("{path}/"))
                        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/')),
                    "a created resource must be a member of {path}"
                );
                model::check_resource(&rid, &mut resource)?;
                Ok((
                    reply(201, Some(&resource), &[("Location", rid)]),
                    "model_answer",
                ))
            }
            ("redfish_resource", "action") => {
                let resource = answer["resource"].clone();
                Ok((reply(200, Some(&resource), &[]), "model_answer"))
            }
            ("redfish_no_content", "update" | "action" | "delete") => {
                Ok((reply(204, None, &[]), "model_answer"))
            }
            ("redfish_task", "update" | "action" | "create" | "delete") => {
                let tid = shared.seq.fetch_add(1, Ordering::Relaxed).to_string();
                let secs = answer["complete_after_secs"]
                    .as_u64()
                    .unwrap_or(DEFAULT_TASK_SECS);
                let start = now_rfc3339();
                let task = Task {
                    final_state: answer["final_state"]
                        .as_str()
                        .unwrap_or("Completed")
                        .to_owned(),
                    done_at: Instant::now() + Duration::from_secs(secs),
                    start: start.clone(),
                    messages: model::messages_from(answer.get("messages"))?,
                    result: answer.get("result").filter(|r| r.is_object()).cloned(),
                };
                let mut tasks = shared.tasks.lock().await;
                if tasks.len() >= MAX_TASKS {
                    let oldest = tasks
                        .keys()
                        .min_by_key(|k| k.parse::<u64>().unwrap_or(0))
                        .cloned();
                    if let Some(k) = oldest {
                        tasks.remove(&k);
                    }
                }
                tasks.insert(tid.clone(), task);
                let monitor = format!("/redfish/v1/TaskService/TaskMonitors/{tid}");
                let resource = model::task_resource(&tid, "Running", 0, &[], &start);
                Ok((
                    reply(
                        202,
                        Some(&resource),
                        &[("Location", monitor), ("Retry-After", "1".into())],
                    ),
                    "model_answer",
                ))
            }
            (t, k) => anyhow::bail!("{t} does not answer a {k} request"),
        }
    }
    .await;
    match built {
        Ok((r, decision)) => {
            outcome(ctx, id, kind, decision);
            r
        }
        Err(e) => failed("fail_closed_invalid_reply", e),
    }
}
