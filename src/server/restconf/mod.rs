//! RESTCONF (RFC 8040) server. Rust owns discovery, the API root, the YANG library from the
//! declared modules, data-resource paths, query parameters, media types and the error document;
//! the handler is the datastore and answers every data request and operation.
pub mod actions;
pub mod path;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Map, Value as Json};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const MAX_BODY: usize = 1024 * 1024;
pub const YANG_LIBRARY_VERSION: &str = "2019-01-04";
const MEDIA: &str = "application/yang-data+json";
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const DATA_METHODS: &str = "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS";

struct Shared {
    ctx: SpawnContext,
    modules: Vec<Json>,
    operations: Vec<String>,
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let mut modules = Vec::new();
    for m in p
        .map(|p| p.get_optional_array("modules"))
        .transpose()?
        .flatten()
        .into_iter()
        .flatten()
    {
        let name = m["name"]
            .as_str()
            .filter(|n| !n.is_empty() && n.len() <= 128)
            .ok_or_else(|| anyhow::anyhow!("each module has a name"))?;
        anyhow::ensure!(modules.len() < 256, "at most 256 modules");
        modules.push(json!({"name": name, "revision": m["revision"].as_str().unwrap_or(""), "namespace": m["namespace"].as_str().unwrap_or(""), "conformance-type": "implement"}));
    }
    let mut operations = Vec::new();
    for o in p
        .map(|p| p.get_optional_array("operations"))
        .transpose()?
        .flatten()
        .into_iter()
        .flatten()
    {
        let o = o
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("operations are module:rpc names"))?;
        anyhow::ensure!(
            path::parse(o).is_ok_and(|s| s.len() == 1),
            "{o:?} is not module:rpc"
        );
        operations.push(o.to_owned());
    }
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("RESTCONF at http://{local}/restconf"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        modules,
        operations,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "RESTCONF", Some(&shared.ctx.status_tx)).await {
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
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let svc = child.clone();
                    let service = service_fn(move |req| {
                        let shared = svc.clone();
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
                            .debug(format!("RESTCONF connection {id}: {e}"));
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

fn reply(status: u16, content_type: Option<&str>, body: Vec<u8>) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Some(ct) = content_type.and_then(|c| header::HeaderValue::from_str(c).ok()) {
        r.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    r
}

fn json_reply(status: u16, body: &Json) -> Reply {
    reply(
        status,
        Some(MEDIA),
        serde_json::to_vec(body).unwrap_or_default(),
    )
}

/// An RFC 8040 error document.
fn error(
    status: Option<u16>,
    tag: &str,
    error_type: &str,
    message: &str,
    error_path: Option<&str>,
) -> Reply {
    let status = status.unwrap_or_else(|| {
        actions::ERROR_TAGS
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, s)| *s)
            .unwrap_or(500)
    });
    let mut e = json!({"error-type": error_type, "error-tag": tag});
    if !message.is_empty() {
        e["error-message"] = json!(message);
    }
    if let Some(p) = error_path {
        e["error-path"] = json!(p);
    }
    json_reply(status, &json!({"ietf-restconf:errors": {"error": [e]}}))
}

fn allow(methods: &str) -> Reply {
    let mut r = reply(200, None, vec![]);
    if let Ok(v) = header::HeaderValue::from_str(methods) {
        r.headers_mut().insert(header::ALLOW, v);
    }
    r
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("RESTCONF connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
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

/// Whether the client accepts JSON (no Accept header does).
fn accepts_json(req: &Request<Incoming>) -> bool {
    match req
        .headers()
        .get(header::ACCEPT)
        .and_then(|a| a.to_str().ok())
    {
        None => true,
        Some(a) => a
            .split(',')
            .map(|m| m.split(';').next().unwrap_or("").trim())
            .any(|m| {
                matches!(
                    m,
                    "application/yang-data+json"
                        | "application/json"
                        | "*/*"
                        | "application/*"
                        | ""
                )
            }),
    }
}

async fn body(req: Request<Incoming>) -> Result<Option<Json>, Reply> {
    let has_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|c| c.to_str().ok())
        .map(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
    let bytes = match Limited::new(req.into_body(), MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return Err(error(
                Some(413),
                "too-big",
                "transport",
                "the body is over 1 MiB",
                None,
            ))
        }
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    match has_type.as_deref() {
        Some("application/yang-data+json" | "application/json") => {}
        _ => {
            return Err(error(
                Some(415),
                "invalid-value",
                "protocol",
                "bodies are application/yang-data+json",
                None,
            ))
        }
    }
    match serde_json::from_slice::<Json>(&bytes) {
        Ok(v) if v.is_object() => Ok(Some(v)),
        _ => Err(error(
            None,
            "malformed-message",
            "protocol",
            "the body is not a YANG JSON object",
            None,
        )),
    }
}

fn query(q: Option<&str>) -> Result<Json, Reply> {
    let mut out = Map::new();
    for pair in q.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = urlencoding::decode(v)
            .map(|s| s.into_owned())
            .unwrap_or_default();
        let ok = match k {
            "depth" => v == "unbounded" || v.parse::<u32>().is_ok_and(|d| (1..=65535).contains(&d)),
            "content" => matches!(v.as_str(), "all" | "config" | "nonconfig"),
            "with-defaults" => matches!(
                v.as_str(),
                "report-all" | "trim" | "explicit" | "report-all-tagged"
            ),
            "insert" => matches!(v.as_str(), "first" | "last" | "before" | "after"),
            "fields" | "point" => !v.is_empty() && v.len() <= 1024,
            _ => false,
        };
        if !ok {
            return Err(error(
                Some(400),
                "invalid-value",
                "protocol",
                &format!("query parameter {k:?} is unknown or has a bad value"),
                None,
            ));
        }
        out.insert(k.to_owned(), json!(v));
    }
    Ok(Json::Object(out))
}

async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Json, Reply> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RestconfProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            let status = if matches!(
                crate::utils::WireFailure::classify(&e),
                crate::utils::WireFailure::Overloaded
            ) {
                503
            } else {
                500
            };
            return Err(error(
                Some(status),
                "operation-failed",
                "application",
                crate::utils::WireFailure::classify(&e).text(),
                None,
            ));
        }
    };
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match (result.failures.is_empty(), answers.len()) {
        (true, 1) => Ok(answers.remove(0)),
        (true, 0) => {
            outcome(ctx, id, operation, "model_silent");
            Err(error(
                Some(500),
                "operation-failed",
                "application",
                "the server could not answer",
                None,
            ))
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Err(error(
                Some(500),
                "operation-failed",
                "application",
                "the server could not answer",
                None,
            ))
        }
    }
}

fn answer_error(a: &Json) -> Reply {
    error(
        a["status"].as_u64().map(|s| s as u16),
        a["error_tag"].as_str().unwrap_or("operation-failed"),
        a["error_type"].as_str().unwrap_or("protocol"),
        a["message"].as_str().unwrap_or(""),
        a["error_path"].as_str(),
    )
}

async fn route(shared: &Shared, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    if !accepts_json(&req) {
        return error(
            Some(406),
            "invalid-value",
            "protocol",
            "only application/yang-data+json is served",
            None,
        );
    }
    let readonly = |m: &Method| matches!(*m, Method::GET | Method::HEAD);
    let head = method == Method::HEAD;
    let strip = |r: Reply| {
        if head {
            Response::from_parts(r.into_parts().0, Full::new(Bytes::new()))
        } else {
            r
        }
    };
    match path.as_str() {
        "/.well-known/host-meta" | "/.well-known/host-meta.json" if readonly(&method) => {
            let wants_json = path.ends_with(".json")
                || req
                    .headers()
                    .get(header::ACCEPT)
                    .and_then(|a| a.to_str().ok())
                    .is_some_and(|a| a.contains("json"));
            strip(if wants_json {
                reply(
                    200,
                    Some("application/json"),
                    serde_json::to_vec(
                        &json!({"links": [{"rel": "restconf", "href": "/restconf"}]}),
                    )
                    .unwrap_or_default(),
                )
            } else {
                reply(200, Some("application/xrd+xml"), b"<XRD xmlns='http://docs.oasis-open.org/ns/xri/xrd-1.0'>\n  <Link rel='restconf' href='/restconf'/>\n</XRD>\n".to_vec())
            })
        }
        "/restconf" | "/restconf/" if readonly(&method) => strip(json_reply(
            200,
            &json!({"ietf-restconf:restconf": {"data": {}, "operations": {}, "yang-library-version": YANG_LIBRARY_VERSION}}),
        )),
        "/restconf/yang-library-version" if readonly(&method) => strip(json_reply(
            200,
            &json!({"ietf-restconf:yang-library-version": YANG_LIBRARY_VERSION}),
        )),
        p if readonly(&method) && p.starts_with("/restconf/data/ietf-yang-library:") => {
            strip(yang_library(
                shared,
                p.trim_start_matches("/restconf/data/ietf-yang-library:"),
            ))
        }
        "/restconf/operations" | "/restconf/operations/" if readonly(&method) => {
            let ops: Map<String, Json> = shared
                .operations
                .iter()
                .map(|o| (o.clone(), json!([null])))
                .collect();
            strip(json_reply(200, &json!({"ietf-restconf:operations": ops})))
        }
        p if p.starts_with("/restconf/operations/") => {
            if method == Method::OPTIONS {
                return allow("POST, OPTIONS");
            }
            if method != Method::POST {
                return error(
                    Some(405),
                    "operation-not-supported",
                    "protocol",
                    "operations are invoked with POST",
                    None,
                );
            }
            operation(
                shared,
                id,
                p.trim_start_matches("/restconf/operations/").to_owned(),
                req,
            )
            .await
        }
        p if p == "/restconf/data" || p.starts_with("/restconf/data/") => {
            if method == Method::OPTIONS {
                return allow(DATA_METHODS);
            }
            let raw = p
                .trim_start_matches("/restconf/data")
                .trim_start_matches('/')
                .to_owned();
            data(shared, id, method, raw, req).await
        }
        _ if path.starts_with("/restconf") => error(
            Some(404),
            "invalid-value",
            "protocol",
            "no such resource",
            None,
        ),
        _ => reply(404, Some("text/plain"), b"not found\n".to_vec()),
    }
}

/// The YANG library (RFC 7895 modules-state) is generated from the declared modules.
fn yang_library(shared: &Shared, rest: &str) -> Reply {
    let state =
        json!({"module-set-id": format!("{:x}", shared.modules.len()), "module": shared.modules});
    match rest.trim_end_matches('/') {
        "" | "modules-state" => json_reply(200, &json!({"ietf-yang-library:modules-state": state})),
        "modules-state/module" => {
            json_reply(200, &json!({"ietf-yang-library:module": shared.modules}))
        }
        "modules-state/module-set-id" => json_reply(
            200,
            &json!({"ietf-yang-library:module-set-id": state["module-set-id"]}),
        ),
        _ => error(
            Some(404),
            "invalid-value",
            "protocol",
            "no such YANG library resource",
            None,
        ),
    }
}

async fn operation(
    shared: &Shared,
    id: ConnectionId,
    name: String,
    req: Request<Incoming>,
) -> Reply {
    let segments = match path::parse(&name) {
        Ok(s) if s.len() == 1 && s[0].keys.is_none() => s,
        _ => {
            return error(
                Some(400),
                "invalid-value",
                "protocol",
                "operations/ names one module:rpc",
                None,
            )
        }
    };
    let module = segments[0].module.clone().unwrap_or_default();
    let input = match body(req).await {
        Ok(b) => b.map(|b| match b {
            Json::Object(mut m)
                if m.len() == 1
                    && m.keys()
                        .next()
                        .is_some_and(|k| k.ends_with(":input") || k == "input") =>
            {
                m.values_mut().next().map(Json::take).unwrap_or_default()
            }
            other => other,
        }),
        Err(r) => return r,
    };
    let mut data = json!({"operation": name});
    if let Some(i) = input {
        data["input"] = i;
    }
    let a = match ask(
        shared,
        id,
        Event::new(&actions::OPERATION_EVENT, data),
        "operation",
    )
    .await
    {
        Ok(a) => a,
        Err(r) => return r,
    };
    match a["type"].as_str() {
        Some("restconf_output") => {
            outcome(&shared.ctx, id, "operation", "model_answer");
            match a
                .get("output")
                .filter(|o| o.as_object().is_some_and(|m| !m.is_empty()))
            {
                Some(o) => json_reply(200, &json!({format!("{module}:output"): o})),
                None => reply(204, None, vec![]),
            }
        }
        Some("restconf_error") => {
            outcome(&shared.ctx, id, "operation", "model_reject");
            answer_error(&a)
        }
        _ => {
            outcome(&shared.ctx, id, "operation", "fail_closed_invalid_reply");
            error(
                Some(500),
                "operation-failed",
                "application",
                "the server could not answer",
                None,
            )
        }
    }
}

async fn data(
    shared: &Shared,
    id: ConnectionId,
    method: Method,
    raw: String,
    req: Request<Incoming>,
) -> Reply {
    let segments = match path::parse(&raw) {
        Ok(s) => s,
        Err(e) => return error(Some(400), "invalid-value", "protocol", &e.to_string(), None),
    };
    let q = match query(req.uri().query()) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let edit = matches!(method, Method::POST | Method::PUT | Method::PATCH);
    if !edit && !matches!(method, Method::GET | Method::HEAD | Method::DELETE) {
        let mut r = error(
            Some(405),
            "operation-not-supported",
            "protocol",
            "method not supported on a data resource",
            None,
        );
        if let Ok(v) = header::HeaderValue::from_str(DATA_METHODS) {
            r.headers_mut().insert(header::ALLOW, v);
        }
        return r;
    }
    if matches!(method, Method::DELETE | Method::PUT | Method::PATCH) && segments.is_empty() {
        return error(
            Some(405),
            "operation-not-supported",
            "protocol",
            "the datastore itself cannot be replaced or deleted",
            None,
        );
    }
    let body = match body(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if edit && body.is_none() {
        return error(
            None,
            "malformed-message",
            "protocol",
            "this method needs a YANG JSON body",
            None,
        );
    }
    let mut event = json!({"method": method.as_str(), "path": raw, "target": path::to_json(&segments), "query": q});
    if let Some(b) = body {
        event["body"] = b;
    }
    let operation = method.as_str().to_ascii_lowercase();
    let a = match ask(
        shared,
        id,
        Event::new(&actions::DATA_EVENT, event),
        &operation,
    )
    .await
    {
        Ok(a) => a,
        Err(r) => return r,
    };
    let ctx = &shared.ctx;
    match (a["type"].as_str(), &method) {
        (Some("restconf_error"), _) => {
            outcome(ctx, id, &operation, "model_reject");
            answer_error(&a)
        }
        (Some("restconf_data"), &Method::GET | &Method::HEAD) => {
            outcome(ctx, id, &operation, "model_answer");
            let r = json_reply(200, &a["data"]);
            if method == Method::HEAD {
                Response::from_parts(r.into_parts().0, Full::new(Bytes::new()))
            } else {
                r
            }
        }
        (Some("restconf_ok"), m) if *m != Method::GET && *m != Method::HEAD => {
            outcome(ctx, id, &operation, "model_answer");
            let status = a["status"]
                .as_u64()
                .map(|s| s as u16)
                .unwrap_or(if *m == Method::POST { 201 } else { 204 });
            let mut r = reply(status, None, vec![]);
            if status == 201 {
                let loc = a["location"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| raw.clone());
                if let Ok(v) = header::HeaderValue::from_str(&format!(
                    "/restconf/data/{}",
                    loc.trim_start_matches('/')
                )) {
                    r.headers_mut().insert(header::LOCATION, v);
                }
            }
            r
        }
        _ => {
            outcome(ctx, id, &operation, "fail_closed_invalid_reply");
            error(
                Some(500),
                "operation-failed",
                "application",
                "the server could not answer",
                None,
            )
        }
    }
}
