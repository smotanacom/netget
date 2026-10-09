//! SCIM 2.0 service (RFC 7643/7644) over HTTP/1.1. Rust owns discovery, request parsing,
//! filters, PATCH paths, sorting, paging, projection and meta; the handler owns the data.
pub mod actions;
pub mod model_bounds;
pub mod query;
pub mod schema;

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
use model_bounds::{error_body, MAX_BODY_BYTES, MAX_PATCH_OPERATIONS, MAX_RESULTS};
use query::{AttrPath, Filter};
use schema::ResourceType;
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const DEFAULT_BASE_PATH: &str = "/scim/v2";
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
const CONTENT_TYPE: &str = "application/scim+json";

struct Shared {
    ctx: SpawnContext,
    base: String,
    token: Option<String>,
    local: SocketAddr,
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let base = p
        .map(|p| p.get_optional_string("base_path"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_BASE_PATH.to_owned());
    let base = base.trim_end_matches('/').to_owned();
    anyhow::ensure!(
        (base.is_empty() || base.starts_with('/'))
            && base.len() <= 128
            && base
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)),
        "base_path must be empty or an absolute path"
    );
    let token = p
        .map(|p| p.get_optional_string("bearer_token"))
        .transpose()?
        .flatten();
    if let Some(t) = &token {
        anyhow::ensure!(
            !t.is_empty() && t.len() <= 512 && t.bytes().all(|b| b.is_ascii_graphic()),
            "bearer_token must be printable ASCII"
        );
    }
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("SCIM service at http://{local}{base}/"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        base,
        token,
        local,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "SCIM", Some(&shared.ctx.status_tx)).await {
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
                            .debug(format!("SCIM connection {id}: {e}"));
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

fn reply(status: u16, body: Option<&Value>, location: Option<String>) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(
        body.map(|b| serde_json::to_vec(b).unwrap_or_default())
            .unwrap_or_default(),
    )));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if body.is_some() {
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(CONTENT_TYPE),
        );
    }
    if let Some(l) = location.and_then(|l| header::HeaderValue::from_str(&l).ok()) {
        r.headers_mut().insert(header::LOCATION, l);
    }
    r
}

fn error(status: u16, scim_type: Option<&str>, detail: &str) -> Reply {
    reply(status, Some(&error_body(status, scim_type, detail)), None)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("SCIM connection {id} operation={operation} decision={decision}");
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

/// Absolute base URL for `meta.location`, from the request's Host.
fn base_url(shared: &Shared, req: &Request<Incoming>) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 255
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b))
        })
        .map(str::to_owned)
        .unwrap_or_else(|| shared.local.to_string());
    format!("http://{host}{}", shared.base)
}

fn query_param(req: &Request<Incoming>, name: &str) -> Option<String> {
    let q = req.uri().query()?;
    q.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == name).then(|| {
            urlencoding::decode(&v.replace('+', " "))
                .map(|c| c.into_owned())
                .unwrap_or_default()
        })
    })
}

async fn read_json(req: Request<Incoming>) -> Result<Value, Reply> {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(ct.starts_with(CONTENT_TYPE) || ct.starts_with("application/json")) {
        return Err(error(
            415,
            None,
            "Content-Type must be application/scim+json",
        ));
    }
    let body = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(req.into_body(), MAX_BODY_BYTES).collect(),
    )
    .await
    {
        Ok(Ok(b)) => b.to_bytes(),
        _ => return Err(error(413, None, "request body unreadable or over 1 MiB")),
    };
    match serde_json::from_slice::<Value>(&body) {
        Ok(v @ Value::Object(_)) if model_bounds::budget_ok(&v) => Ok(v),
        _ => Err(error(
            400,
            Some("invalidSyntax"),
            "the body is not a JSON object within bounds",
        )),
    }
}

struct ListQuery {
    filter: Option<(String, Filter)>,
    sort_by: Option<AttrPath>,
    descending: bool,
    start: usize,
    count: usize,
    attributes: Vec<AttrPath>,
    excluded: Vec<AttrPath>,
}

fn list_query(
    filter: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    start: Option<String>,
    count: Option<String>,
    attributes: Option<String>,
    excluded: Option<String>,
) -> Result<ListQuery, Reply> {
    let bad = |t: &str, e: anyhow::Error| error(400, Some(t), &e.to_string());
    let filter = match filter.filter(|f| !f.trim().is_empty()) {
        Some(f) => Some((
            f.clone(),
            query::parse_filter(&f).map_err(|e| bad("invalidFilter", e))?,
        )),
        None => None,
    };
    let sort_by = sort_by
        .filter(|s| !s.is_empty())
        .map(|s| AttrPath::parse(&s))
        .transpose()
        .map_err(|e| bad("invalidValue", e))?;
    let descending = match sort_order.as_deref() {
        None | Some("") | Some("ascending") => false,
        Some("descending") => true,
        Some(_) => {
            return Err(error(
                400,
                Some("invalidValue"),
                "sortOrder is ascending or descending",
            ))
        }
    };
    let num = |v: Option<String>, d: usize| -> Result<usize, Reply> {
        match v.filter(|v| !v.is_empty()) {
            None => Ok(d),
            Some(v) => v.parse::<i64>().map(|n| n.max(0) as usize).map_err(|_| {
                error(
                    400,
                    Some("invalidValue"),
                    "startIndex and count are integers",
                )
            }),
        }
    };
    Ok(ListQuery {
        filter,
        sort_by,
        descending,
        start: num(start, 1)?.max(1),
        count: num(count, MAX_RESULTS)?.min(MAX_RESULTS),
        attributes: query::parse_list(attributes.as_deref()).map_err(|e| bad("invalidValue", e))?,
        excluded: query::parse_list(excluded.as_deref()).map_err(|e| bad("invalidValue", e))?,
    })
}

fn service_provider_config(shared: &Shared, base: &str) -> Value {
    let schemes = match shared.token {
        Some(_) => {
            json!([{"type": "oauthbearertoken", "name": "OAuth Bearer Token", "description": "Authentication with a bearer token (RFC 6750)", "specUri": "https://www.rfc-editor.org/rfc/rfc6750", "primary": true}])
        }
        None => json!([]),
    };
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "patch": {"supported": true},
        "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
        "filter": {"supported": true, "maxResults": MAX_RESULTS},
        "changePassword": {"supported": false},
        "sort": {"supported": true},
        "etag": {"supported": false},
        "authenticationSchemes": schemes,
        "meta": {"resourceType": "ServiceProviderConfig", "location": format!("{base}/ServiceProviderConfig")}
    })
}

fn list_response(resources: Vec<Value>, total: usize, start: usize) -> Value {
    json!({"schemas": [schema::LIST_RESPONSE], "totalResults": total, "startIndex": start, "itemsPerPage": resources.len(), "Resources": resources})
}

fn authorized(shared: &Shared, req: &Request<Incoming>) -> bool {
    let Some(expected) = &shared.token else {
        return true;
    };
    let got = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    got.len() == expected.len()
        && got
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn route(shared: &Shared, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let path = req.uri().path().to_owned();
    let Some(rest) = path
        .strip_prefix(shared.base.as_str())
        .filter(|r| r.is_empty() || r.starts_with('/'))
    else {
        return error(404, None, "not a SCIM endpoint");
    };
    if !authorized(shared, &req) {
        let mut r = error(401, None, "a valid bearer token is required");
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"NetGet SCIM\""),
        );
        return r;
    }
    let base = base_url(shared, &req);
    let segments: Vec<String> = rest
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            urlencoding::decode(s)
                .map(|c| c.into_owned())
                .unwrap_or_default()
        })
        .collect();
    let method = req.method().clone();
    let get = method == Method::GET;
    let seg: Vec<&str> = segments.iter().map(String::as_str).collect();
    match seg.as_slice() {
        ["ServiceProviderConfig"] if get => {
            return reply(200, Some(&service_provider_config(shared, &base)), None)
        }
        ["ResourceTypes"] if get => {
            let all: Vec<Value> = schema::RESOURCE_TYPES
                .iter()
                .map(|r| schema::resource_type_resource(r, &base))
                .collect();
            let n = all.len();
            return reply(200, Some(&list_response(all, n, 1)), None);
        }
        ["ResourceTypes", name] if get => {
            return match schema::by_name(name) {
                Some(r) => reply(200, Some(&schema::resource_type_resource(r, &base)), None),
                None => error(404, None, "no such resource type"),
            };
        }
        ["Schemas"] if get => {
            let all: Vec<Value> = schema::SCHEMAS
                .iter()
                .map(|s| schema::published(s, &base))
                .collect();
            let n = all.len();
            return reply(200, Some(&list_response(all, n, 1)), None);
        }
        ["Schemas", urn] if get => {
            return match schema::schema(urn) {
                Some(s) => reply(200, Some(&schema::published(s, &base)), None),
                None => error(404, None, "no such schema"),
            };
        }
        ["ServiceProviderConfig" | "ResourceTypes" | "Schemas", ..] => {
            return error(405, None, "discovery endpoints are read-only")
        }
        ["Bulk"] | ["Me", ..] => return error(501, None, "not implemented by this service"),
        [] | [".search"] => {
            let lq = match search_query(&method, req).await {
                Ok(q) => q,
                Err(r) => return r,
            };
            return list(
                shared,
                id,
                schema::RESOURCE_TYPES.iter().collect(),
                lq,
                &base,
            )
            .await;
        }
        _ => {}
    }
    let Some(rt) = seg.first().and_then(|s| schema::by_endpoint(s)) else {
        return error(404, None, "no such endpoint");
    };
    match (seg.len(), seg.get(1).copied()) {
        (1, _) if get => {
            let lq = list_query(
                query_param(&req, "filter"),
                query_param(&req, "sortBy"),
                query_param(&req, "sortOrder"),
                query_param(&req, "startIndex"),
                query_param(&req, "count"),
                query_param(&req, "attributes"),
                query_param(&req, "excludedAttributes"),
            );
            match lq {
                Ok(q) => list(shared, id, vec![rt], q, &base).await,
                Err(r) => r,
            }
        }
        (2, Some(".search")) if method == Method::POST => match search_query(&method, req).await {
            Ok(q) => list(shared, id, vec![rt], q, &base).await,
            Err(r) => r,
        },
        (1, _) if method == Method::POST => {
            let projection = (
                query_param(&req, "attributes"),
                query_param(&req, "excludedAttributes"),
            );
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let resource = match incoming_resource(rt, body) {
                Ok(r) => r,
                Err(r) => return r,
            };
            single(
                shared,
                id,
                rt,
                "create",
                None,
                json!({"resource": resource}),
                &base,
                projection,
            )
            .await
        }
        (1, _) => error(405, None, "use GET or POST on a collection"),
        (2, Some(rid)) => {
            let rid = rid.to_owned();
            if rid.len() > 256 {
                return error(404, None, "no such resource");
            }
            let projection = (
                query_param(&req, "attributes"),
                query_param(&req, "excludedAttributes"),
            );
            match method {
                Method::GET => {
                    single(
                        shared,
                        id,
                        rt,
                        "get",
                        Some(&rid),
                        json!({}),
                        &base,
                        projection,
                    )
                    .await
                }
                Method::DELETE => {
                    single(
                        shared,
                        id,
                        rt,
                        "delete",
                        Some(&rid),
                        json!({}),
                        &base,
                        projection,
                    )
                    .await
                }
                Method::PUT => {
                    let body = match read_json(req).await {
                        Ok(b) => b,
                        Err(r) => return r,
                    };
                    let resource = match incoming_resource(rt, body) {
                        Ok(r) => r,
                        Err(r) => return r,
                    };
                    single(
                        shared,
                        id,
                        rt,
                        "replace",
                        Some(&rid),
                        json!({"resource": resource}),
                        &base,
                        projection,
                    )
                    .await
                }
                Method::PATCH => {
                    let body = match read_json(req).await {
                        Ok(b) => b,
                        Err(r) => return r,
                    };
                    let ops = match patch_operations(&body) {
                        Ok(o) => o,
                        Err(r) => return r,
                    };
                    single(
                        shared,
                        id,
                        rt,
                        "patch",
                        Some(&rid),
                        json!({"operations": ops}),
                        &base,
                        projection,
                    )
                    .await
                }
                _ => error(405, None, "method not allowed on a resource"),
            }
        }
        _ => error(404, None, "no such endpoint"),
    }
}

async fn search_query(method: &Method, req: Request<Incoming>) -> Result<ListQuery, Reply> {
    if *method == Method::GET {
        return list_query(
            query_param(&req, "filter"),
            query_param(&req, "sortBy"),
            query_param(&req, "sortOrder"),
            query_param(&req, "startIndex"),
            query_param(&req, "count"),
            query_param(&req, "attributes"),
            query_param(&req, "excludedAttributes"),
        );
    }
    if *method != Method::POST {
        return Err(error(405, None, "use POST for .search"));
    }
    let body = read_json(req).await?;
    if !body["schemas"]
        .as_array()
        .is_some_and(|s| s.iter().any(|x| x == schema::SEARCH_REQUEST))
    {
        return Err(error(
            400,
            Some("invalidSyntax"),
            "a search body declares the SearchRequest schema",
        ));
    }
    let s = |k: &str| {
        body.get(k).and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            _ => None,
        })
    };
    list_query(
        s("filter"),
        s("sortBy"),
        s("sortOrder"),
        s("startIndex"),
        s("count"),
        s("attributes"),
        s("excludedAttributes"),
    )
}

/// A resource from a create or replace body: schemas must name the type's core schema; id
/// and meta are read-only and dropped.
fn incoming_resource(rt: &ResourceType, mut body: Value) -> Result<Value, Reply> {
    if !body["schemas"].as_array().is_some_and(|s| {
        s.iter().any(|x| {
            x.as_str()
                .is_some_and(|x| x.eq_ignore_ascii_case(rt.schema))
        })
    }) {
        return Err(error(
            400,
            Some("invalidSyntax"),
            &format!("schemas must include {}", rt.schema),
        ));
    }
    if let Some(o) = body.as_object_mut() {
        o.retain(|k, _| !k.eq_ignore_ascii_case("id") && !k.eq_ignore_ascii_case("meta"));
    }
    Ok(body)
}

fn patch_operations(body: &Value) -> Result<Vec<Value>, Reply> {
    let bad = |d: String| error(400, Some("invalidSyntax"), &d);
    if !body["schemas"]
        .as_array()
        .is_some_and(|s| s.iter().any(|x| x == schema::PATCH_OP))
    {
        return Err(bad(format!("a PATCH body declares {}", schema::PATCH_OP)));
    }
    let ops = body
        .get("Operations")
        .or_else(|| body.get("operations"))
        .and_then(Value::as_array)
        .filter(|o| !o.is_empty() && o.len() <= MAX_PATCH_OPERATIONS)
        .ok_or_else(|| {
            bad(format!(
                "Operations must hold 1..{MAX_PATCH_OPERATIONS} operations"
            ))
        })?;
    ops.iter().map(|o| {
        let op = o["op"].as_str().map(str::to_ascii_lowercase).filter(|op| matches!(op.as_str(), "add" | "remove" | "replace"))
            .ok_or_else(|| bad("op must be add, remove or replace".into()))?;
        let path = match o.get("path").filter(|p| !p.is_null()) {
            None => None,
            Some(p) => {
                let p = p.as_str().ok_or_else(|| bad("path must be a string".into()))?;
                Some((p.to_owned(), query::PatchPath::parse(p).map_err(|e| error(400, Some("invalidPath"), &e.to_string()))?))
            }
        };
        if op == "remove" && path.is_none() {
            return Err(error(400, Some("noTarget"), "remove needs a path"));
        }
        if op != "remove" && o.get("value").is_none_or(Value::is_null) {
            return Err(bad(format!("{op} needs a value")));
        }
        if op != "remove" && path.is_none() && !o["value"].is_object() {
            return Err(bad(format!("{op} without a path takes an object value")));
        }
        Ok(json!({"op": op, "path": path.as_ref().map(|p| &p.0), "parsed_path": path.as_ref().map(|p| p.1.to_json()), "value": o.get("value")}))
    }).collect()
}

/// Ask the handler. Failures are logged and returned as the reply to send.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    operation: &str,
    data: Value,
) -> Result<Value, Reply> {
    let ctx = &shared.ctx;
    let failed = |decision: &str, e: anyhow::Error| {
        outcome(ctx, id, operation, decision);
        let failure = crate::utils::wire_failure::WireFailure::classify(&e);
        error(
            if failure.is_overloaded() { 503 } else { 500 },
            None,
            failure.text(),
        )
    };
    let event = Event::new(&actions::REQUEST_EVENT, data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::ScimProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(failed("fail_closed_llm_error", e)),
    };
    if !result.failures.is_empty() {
        return Err(failed(
            "fail_closed_invalid_reply",
            anyhow::anyhow!("invalid handler answer"),
        ));
    }
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match answers.len() {
        1 => {
            let a = answers.remove(0);
            if a["type"] == "scim_error" {
                outcome(ctx, id, operation, "model_reject");
                let status = a["status"].as_u64().unwrap_or(500) as u16;
                return Err(error(
                    status,
                    a["scim_type"].as_str(),
                    a["detail"].as_str().unwrap_or("refused"),
                ));
            }
            Ok(a)
        }
        0 => Err(failed("model_silent", anyhow::anyhow!("no handler answer"))),
        _ => Err(failed(
            "fail_closed_invalid_reply",
            anyhow::anyhow!("more than one answer"),
        )),
    }
}

/// Check a handler resource and write its meta.
fn finish(
    rt: &ResourceType,
    mut r: Value,
    expected_id: Option<&str>,
    base: &str,
) -> anyhow::Result<Value> {
    let rid = r["id"]
        .as_str()
        .filter(|i| !i.is_empty() && i.len() <= 256 && !i.contains('/'))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("a resource needs an id without /"))?;
    if let Some(e) = expected_id {
        anyhow::ensure!(rid == e, "the answer describes {rid}, not {e}");
    }
    let o = r
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("a resource is an object"))?;
    let schemas = o.entry("schemas").or_insert_with(|| json!([rt.schema]));
    let list = schemas
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("schemas must be an array"))?;
    if !list.iter().any(|s| {
        s.as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(rt.schema))
    }) {
        list.insert(0, json!(rt.schema));
    }
    // RFC 7643 §3: schemas names every schema whose attributes the resource carries, so an
    // extension URN follows its object, and an emptied extension object goes with its URN.
    for ext in rt.extensions {
        let present = schema::member_key(&Value::Object(o.clone()), ext);
        let keep = present
            .as_ref()
            .is_some_and(|k| o[k].as_object().is_some_and(|m| !m.is_empty()));
        if let (Some(k), false) = (&present, keep) {
            o.remove(k);
        }
        let list = o
            .get_mut("schemas")
            .and_then(Value::as_array_mut)
            .expect("schemas set above");
        list.retain(|s| !s.as_str().is_some_and(|s| s.eq_ignore_ascii_case(ext)));
        if keep {
            list.push(json!(ext));
        }
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let meta = o.entry("meta").or_insert_with(|| json!({}));
    let meta = meta
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("meta must be an object"))?;
    meta.insert("resourceType".into(), json!(rt.name));
    meta.insert(
        "location".into(),
        json!(format!("{base}{}/{rid}", rt.endpoint)),
    );
    for k in ["created", "lastModified"] {
        if !meta.get(k).is_some_and(Value::is_string) {
            meta.insert(k.into(), json!(now));
        }
    }
    Ok(r)
}

#[allow(clippy::too_many_arguments)]
async fn single(
    shared: &Shared,
    id: ConnectionId,
    rt: &ResourceType,
    operation: &str,
    rid: Option<&str>,
    extra: Value,
    base: &str,
    projection: (Option<String>, Option<String>),
) -> Reply {
    let ctx = &shared.ctx;
    let (attributes, excluded) = match (
        query::parse_list(projection.0.as_deref()),
        query::parse_list(projection.1.as_deref()),
    ) {
        (Ok(a), Ok(e)) => (a, e),
        (Err(e), _) | (_, Err(e)) => return error(400, Some("invalidValue"), &e.to_string()),
    };
    let mut data = json!({"operation": operation, "resource_type": rt.name, "id": rid});
    if let Value::Object(m) = extra {
        data.as_object_mut().expect("object").extend(m);
    }
    let answer = match ask(shared, id, operation, data).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let built: anyhow::Result<Reply> =
        (|| match (answer["type"].as_str().unwrap_or_default(), operation) {
            ("scim_no_content", "delete" | "patch") => Ok(reply(204, None, None)),
            ("scim_resource", "get" | "replace" | "patch" | "create") => {
                let r = finish(rt, answer["resource"].clone(), rid, base)?;
                let location = r["meta"]["location"].as_str().map(str::to_owned);
                let body = query::project(&r, rt, &attributes, &excluded);
                Ok(if operation == "create" {
                    reply(201, Some(&body), location)
                } else {
                    reply(200, Some(&body), None)
                })
            }
            (t, o) => anyhow::bail!("{t} does not answer {o}"),
        })();
    match built {
        Ok(r) => {
            outcome(ctx, id, operation, "model_answer");
            r
        }
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Log::new(Some(&ctx.status_tx)).warn(format!("SCIM {operation}: {e}"));
            error(
                500,
                None,
                crate::utils::wire_failure::WireFailure::Unavailable.text(),
            )
        }
    }
}

async fn list(
    shared: &Shared,
    id: ConnectionId,
    types: Vec<&ResourceType>,
    q: ListQuery,
    base: &str,
) -> Reply {
    let ctx = &shared.ctx;
    let mut all = Vec::new();
    for rt in &types {
        let data = json!({
            "operation": "list",
            "resource_type": rt.name,
            "filter": q.filter.as_ref().map(|f| &f.0),
            "filter_parsed": q.filter.as_ref().map(|f| f.1.to_json()),
            "sort_by": q.sort_by.as_ref().map(|s| s.to_json()),
        });
        let answer = match ask(shared, id, "list", data).await {
            Ok(a) => a,
            Err(r) => return r,
        };
        if answer["type"] != "scim_resources" {
            outcome(ctx, id, "list", "fail_closed_invalid_reply");
            return error(
                500,
                None,
                crate::utils::wire_failure::WireFailure::Unavailable.text(),
            );
        }
        for r in answer["resources"].as_array().cloned().unwrap_or_default() {
            match finish(rt, r, None, base) {
                Ok(r)
                    if q.filter
                        .as_ref()
                        .is_none_or(|f| query::matches(&f.1, rt, &r)) =>
                {
                    all.push((*rt, r))
                }
                Ok(_) => {}
                Err(e) => {
                    outcome(ctx, id, "list", "fail_closed_invalid_reply");
                    Log::new(Some(&ctx.status_tx)).warn(format!("SCIM list: {e}"));
                    return error(
                        500,
                        None,
                        crate::utils::wire_failure::WireFailure::Unavailable.text(),
                    );
                }
            }
        }
    }
    if let (Some(path), [rt]) = (&q.sort_by, types.as_slice()) {
        let mut resources: Vec<Value> = all.drain(..).map(|(_, r)| r).collect();
        query::sort(&mut resources, rt, path, q.descending);
        all = resources.into_iter().map(|r| (*rt, r)).collect();
    }
    let total = all.len();
    let page: Vec<Value> = all
        .into_iter()
        .skip(q.start - 1)
        .take(q.count)
        .map(|(rt, r)| query::project(&r, rt, &q.attributes, &q.excluded))
        .collect();
    outcome(ctx, id, "list", "model_answer");
    reply(200, Some(&list_response(page, total, q.start)), None)
}
