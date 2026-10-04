//! JMAP (RFC 8620, RFC 8621) server. Rust owns the session resource, authentication, request
//! validation and limits, capabilities, accounts, Core/echo, result references and creation ids;
//! the handler is the store and answers every other method call.
pub mod actions;
pub mod request;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use request::{method_error, Problem};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const MAX_BODY: usize = 1024 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const TLS_TIMEOUT: Duration = Duration::from_secs(10);

struct Shared {
    ctx: SpawnContext,
    /// (id, name), the first primary.
    accounts: Vec<(String, String)>,
    users: BTreeMap<String, String>,
    tokens: BTreeMap<String, String>,
    scheme: &'static str,
    state: String,
}

fn string_map(
    p: Option<&crate::protocol::StartupParams>,
    name: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if let Some(m) = p
        .map(|p| p.get_optional_object(name))
        .transpose()?
        .flatten()
    {
        anyhow::ensure!(m.len() <= 64, "at most 64 entries in {name}");
        for (k, v) in m {
            let v = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{name} values are strings"))?;
            anyhow::ensure!(
                !k.is_empty() && !v.is_empty(),
                "{name} entries are not empty"
            );
            out.insert(k.clone(), v.to_owned());
        }
    }
    Ok(out)
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let mut accounts = Vec::new();
    for a in p
        .map(|p| p.get_optional_array("accounts"))
        .transpose()?
        .flatten()
        .into_iter()
        .flatten()
    {
        let id = a["id"]
            .as_str()
            .filter(|i| {
                !i.is_empty()
                    && i.len() <= 255
                    && i.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| anyhow::anyhow!("each account has an id of letters, digits, - and _"))?;
        let name = a["name"].as_str().unwrap_or(id);
        anyhow::ensure!(accounts.len() < 64, "at most 64 accounts");
        accounts.push((id.to_owned(), name.to_owned()));
    }
    if accounts.is_empty() {
        accounts.push(("a1".into(), "netget".into()));
    }
    let users = string_map(p, "users")?;
    let tokens = string_map(p, "api_tokens")?;
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    let tls_on = p
        .map(|p| p.get_optional_bool("tls"))
        .transpose()?
        .flatten()
        .unwrap_or(true);
    let tls = match (tls_on, get("tls_cert_file")?, get("tls_key_file")?) {
        (false, None, None) => None,
        (false, _, _) => anyhow::bail!("tls is false but a certificate was given"),
        (true, Some(c), Some(k)) => Some(tokio_rustls::TlsAcceptor::from(
            crate::server::tls_cert_manager::load_tls_config_from_files(&c, &k)?,
        )),
        (true, None, None) => {
            let spec = crate::server::tls_cert_manager::CertificateSpec {
                common_name: "localhost".into(),
                san_dns_names: vec!["localhost".into()],
                validity_days: 30,
                organization: Some("NetGet".into()),
                organizational_unit: Some("JMAP".into()),
            };
            let (cert, key) = crate::server::tls_cert_manager::generate_self_signed_cert(&spec)?;
            let pem = cert.pem();
            ctx.state
                .with_server_mut(ctx.server_id, |s| {
                    s.set_protocol_field("certificate_pem".into(), json!(pem))
                })
                .await;
            Some(tokio_rustls::TlsAcceptor::from(
                crate::server::tls_cert_manager::create_rustls_server_config(&cert, &key)?,
            ))
        }
        _ => anyhow::bail!("tls_cert_file and tls_key_file go together"),
    };
    let scheme = if tls.is_some() { "https" } else { "http" };
    let state = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        accounts.hash(&mut h);
        format!("{:x}", h.finish())
    };
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "JMAP session at {scheme}://{local}/.well-known/jmap"
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        accounts,
        users,
        tokens,
        scheme,
        state,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "JMAP", Some(&shared.ctx.status_tx)).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
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
            let tls = tls.clone();
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
                    let served = match tls {
                        Some(acceptor) => {
                            match tokio::time::timeout(TLS_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(s)) => builder
                                    .serve_connection(TokioIo::new(s), service)
                                    .await
                                    .map_err(|e| e.to_string()),
                                Ok(Err(e)) => Err(format!("TLS handshake: {e}")),
                                Err(_) => Err("TLS handshake timed out".into()),
                            }
                        }
                        None => builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                            .map_err(|e| e.to_string()),
                    };
                    if let Err(e) = served {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("JMAP connection {id}: {e}"));
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

fn reply(status: u16, content_type: &str, body: Vec<u8>) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(ct) = header::HeaderValue::from_str(content_type) {
        r.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache, no-store"),
    );
    r
}

fn json_reply(status: u16, body: &Value) -> Reply {
    reply(
        status,
        "application/json",
        serde_json::to_vec(body).unwrap_or_default(),
    )
}

fn problem(status: u16, kind: &str, detail: &str) -> Reply {
    reply(
        status,
        "application/problem+json",
        serde_json::to_vec(&json!({"type": kind, "status": status, "detail": detail}))
            .unwrap_or_default(),
    )
}

fn log_decision(ctx: &SpawnContext, id: ConnectionId, what: &str, decision: &str) {
    let line = format!("JMAP connection {id} {what} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(line);
    } else {
        log.info(line);
    }
}

async fn handle(shared: &Shared, id: ConnectionId, req: Request<Incoming>) -> Reply {
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

/// The authenticated user, None when credentials are required and wrong or missing.
fn authenticate(shared: &Shared, req: &Request<Incoming>) -> Option<String> {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|a| a.to_str().ok());
    if shared.users.is_empty() && shared.tokens.is_empty() {
        return Some(
            given
                .and_then(basic)
                .map(|(u, _)| u)
                .unwrap_or_else(|| shared.accounts[0].1.clone()),
        );
    }
    let given = given?;
    if let Some(token) = given.strip_prefix("Bearer ") {
        return shared.tokens.get(token.trim()).cloned();
    }
    let (user, password) = basic(given)?;
    (shared.users.get(&user) == Some(&password)).then_some(user)
}

fn basic(header: &str) -> Option<(String, String)> {
    let raw = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (u, p) = text.split_once(':')?;
    Some((u.to_owned(), p.to_owned()))
}

async fn route(shared: &Shared, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let path = req.uri().path().to_owned();
    let Some(user) = authenticate(shared, &req) else {
        log_decision(
            &shared.ctx,
            id,
            &path,
            "protocol_refusal reason=unauthenticated",
        );
        let mut r = problem(401, "about:blank", "authentication is required");
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Basic realm=\"JMAP\""),
        );
        return r;
    };
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| h.len() <= 255 && !h.contains(['/', ' ']))
        .unwrap_or("localhost")
        .to_owned();
    match (req.method(), path.as_str()) {
        (&Method::GET, "/.well-known/jmap" | "/jmap/session") => {
            json_reply(200, &session(shared, &host, &user))
        }
        (&Method::POST, "/jmap" | "/jmap/") => api(shared, id, &user, req).await,
        (_, p)
            if p.starts_with("/jmap/upload/")
                || p.starts_with("/jmap/download/")
                || p.starts_with("/jmap/eventsource") =>
        {
            problem(
                501,
                "about:blank",
                "blobs and push are not implemented by this server",
            )
        }
        (_, "/.well-known/jmap" | "/jmap" | "/jmap/" | "/jmap/session") => {
            problem(405, "about:blank", "method not allowed")
        }
        _ => problem(404, "about:blank", "no such resource"),
    }
}

fn session(shared: &Shared, host: &str, user: &str) -> Value {
    let base = format!("{}://{host}", shared.scheme);
    let mut accounts = Map::new();
    for (id, name) in &shared.accounts {
        accounts.insert(id.clone(), json!({
            "name": name,
            "isPersonal": true,
            "isReadOnly": false,
            "accountCapabilities": {
                request::MAIL: {"maxMailboxesPerEmail": null, "maxMailboxDepth": 10, "maxSizeMailboxName": 255, "maxSizeAttachmentsPerEmail": 50_000_000, "emailQuerySortOptions": ["receivedAt", "sentAt", "size", "from", "to", "subject"], "mayCreateTopLevelMailbox": true},
                request::SUBMISSION: {"maxDelayedSend": 0, "submissionExtensions": {}},
                request::VACATION: {},
            },
        }));
    }
    let primary = &shared.accounts[0].0;
    json!({
        "capabilities": {
            request::CORE: {
                "maxSizeUpload": 0,
                "maxConcurrentUpload": 1,
                "maxSizeRequest": MAX_BODY,
                "maxConcurrentRequests": 4,
                "maxCallsInRequest": request::MAX_CALLS,
                "maxObjectsInGet": request::MAX_OBJECTS_IN_GET,
                "maxObjectsInSet": request::MAX_OBJECTS_IN_SET,
                "collationAlgorithms": ["i;ascii-numeric", "i;ascii-casemap", "i;octet"],
            },
            request::MAIL: {},
            request::SUBMISSION: {},
            request::VACATION: {},
        },
        "accounts": accounts,
        "primaryAccounts": {request::MAIL: primary, request::SUBMISSION: primary, request::VACATION: primary},
        "username": user,
        "apiUrl": format!("{base}/jmap/"),
        "downloadUrl": format!("{base}/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "eventSourceUrl": format!("{base}/jmap/eventsource/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
        "state": shared.state,
    })
}

fn problem_reply(p: &Problem) -> Reply {
    reply(
        400,
        "application/problem+json",
        serde_json::to_vec(&p.body()).unwrap_or_default(),
    )
}

async fn api(shared: &Shared, id: ConnectionId, user: &str, req: Request<Incoming>) -> Reply {
    let bytes = match Limited::new(req.into_body(), MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return problem_reply(&Problem {
                kind: "limit",
                detail: format!("requests are at most {MAX_BODY} bytes"),
                limit: Some("maxSizeRequest"),
            })
        }
    };
    let request = match request::parse(&bytes) {
        Ok(r) => r,
        Err(p) => {
            log_decision(
                &shared.ctx,
                id,
                "request",
                &format!("protocol_refusal reason={}", p.kind),
            );
            return problem_reply(&p);
        }
    };
    let mut created = request.created_ids.clone().unwrap_or_default();
    let mut responses: Vec<(String, Value, String)> = Vec::new();
    for call in &request.calls {
        let answers = match prepare(shared, &request.using, call, &responses, &created) {
            Err(e) => vec![("error".to_owned(), e)],
            Ok(args) if call.name == "Core/echo" => vec![(call.name.clone(), Value::Object(args))],
            Ok(args) => answer(shared, id, user, call, args).await,
        };
        for (name, args) in answers {
            request::record_created(&args, &mut created);
            responses.push((name, args, call.id.clone()));
        }
    }
    let mut body = json!({
        "methodResponses": responses.into_iter().map(|(n, a, i)| json!([n, a, i])).collect::<Vec<_>>(),
        "sessionState": shared.state,
    });
    if request.created_ids.is_some() {
        body["createdIds"] = Value::Object(created);
    }
    json_reply(200, &body)
}

/// Everything Rust decides about a call before the handler sees it.
fn prepare(
    shared: &Shared,
    using: &[String],
    call: &request::Call,
    earlier: &[(String, Value, String)],
    created: &Map<String, Value>,
) -> Result<Map<String, Value>, Value> {
    let args = request::resolve_references(&call.arguments, earlier)?;
    let args = match request::substitute_creation_ids(&Value::Object(args), created) {
        Value::Object(m) => m,
        _ => unreachable!("an object stays an object"),
    };
    let Some(capability) = request::capability(&call.name) else {
        return Err(method_error(
            "unknownMethod",
            &format!("{} is not a method of this server", call.name),
        ));
    };
    if !using.iter().any(|u| u == capability) {
        return Err(method_error(
            "unknownMethod",
            &format!("{} needs {capability} in using", call.name),
        ));
    }
    if capability != request::CORE {
        let account = args
            .get("accountId")
            .and_then(Value::as_str)
            .ok_or_else(|| method_error("invalidArguments", "accountId is required"))?;
        if !shared.accounts.iter().any(|(a, _)| a == account) {
            return Err(method_error("accountNotFound", account));
        }
    }
    if let Some(e) = request::check_limits(&call.name, &args) {
        return Err(e);
    }
    Ok(args)
}

/// One handler turn for a call: its responses as (name, arguments).
async fn answer(
    shared: &Shared,
    id: ConnectionId,
    user: &str,
    call: &request::Call,
    args: Map<String, Value>,
) -> Vec<(String, Value)> {
    let ctx = &shared.ctx;
    let account = args.get("accountId").cloned();
    let event = Event::new(
        &actions::METHOD_CALL_EVENT,
        json!({"method": call.name, "account_id": account, "arguments": args, "call_id": call.id, "username": user}),
    );
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::JmapProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            log_decision(ctx, id, &call.name, "fail_closed_llm_error");
            let kind = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                "serverUnavailable"
            } else {
                "serverFail"
            };
            return vec![("error".into(), method_error(kind, ""))];
        }
    };
    if !result.failures.is_empty() {
        log_decision(ctx, id, &call.name, "fail_closed_invalid_reply");
        return vec![("error".into(), method_error("serverFail", ""))];
    }
    let mut out = Vec::new();
    for r in result.protocol_results {
        let ActionResult::Custom { data, .. } = r else {
            continue;
        };
        if data["type"] == "jmap_method_error" {
            log_decision(ctx, id, &call.name, "model_reject");
            out.push((
                "error".to_owned(),
                method_error(
                    data["error_type"].as_str().unwrap_or("serverFail"),
                    data["description"].as_str().unwrap_or_default(),
                ),
            ));
        } else {
            let mut a = data["arguments"].clone();
            if let (Some(acc), Some(m)) = (&account, a.as_object_mut()) {
                m.entry("accountId").or_insert_with(|| acc.clone());
            }
            let name = data["method"].as_str().unwrap_or(&call.name).to_owned();
            out.push((name, a));
        }
    }
    if out.is_empty() {
        log_decision(ctx, id, &call.name, "model_silent");
        return vec![("error".into(), method_error("serverFail", ""))];
    }
    if out.iter().all(|(n, _)| n != "error") {
        log_decision(ctx, id, &call.name, "model_answer");
    }
    out
}
