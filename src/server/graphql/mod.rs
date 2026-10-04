//! GraphQL over HTTP (GraphQL-over-HTTP draft) on hyper. Rust owns request parsing, media-type
//! negotiation, validation against the schema, introspection and execution; the handler owns
//! the data behind every query and mutation.
pub mod actions;
pub mod engine;
pub mod ws;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use apollo_compiler::{executable::OperationType, validation::Valid, Schema};
use bytes::Bytes;
use engine::RequestErrors;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Map, Value};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const DEFAULT_SCHEMA: &str = "type Query { hello(name: String): String! }";
pub const DEFAULT_ENDPOINT: &str = "/graphql";
pub const DEFAULT_INTROSPECTION: bool = true;
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub const GRAPHQL_RESPONSE: &str = "application/graphql-response+json";

type UpgradeSlot = Arc<std::sync::Mutex<Option<hyper::upgrade::OnUpgrade>>>;

struct Shared {
    ctx: SpawnContext,
    schema: Valid<Schema>,
    endpoint: String,
    introspection: bool,
    init_timeout: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let sdl = p
        .map(|p| p.get_optional_string("schema"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());
    let schema = engine::load_schema(&sdl)?;
    let endpoint = p
        .map(|p| p.get_optional_string("endpoint"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
    anyhow::ensure!(
        endpoint.starts_with('/')
            && endpoint.len() <= 256
            && !endpoint.contains(['?', '#', ' '])
            && !endpoint.chars().any(char::is_control),
        "endpoint must be an absolute path without query or fragment"
    );
    let introspection = p
        .map(|p| p.get_optional_bool("introspection"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_INTROSPECTION);
    let init_timeout = p
        .map(|p| p.get_optional_u64("connection_init_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(ws::CONNECTION_INIT_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=300).contains(&init_timeout),
        "connection_init_timeout_secs must be 1..=300"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "GraphQL server at http://{local}{endpoint} (introspection {})",
        if introspection { "on" } else { "off" }
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        schema,
        endpoint,
        introspection,
        init_timeout: Duration::from_secs(init_timeout),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "GraphQL", Some(&shared.ctx.status_tx)).await {
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
                    let upgrade: UpgradeSlot = Arc::default();
                    let svc_upgrade = upgrade.clone();
                    let service = service_fn(move |req| {
                        let shared = svc_shared.clone();
                        let upgrade = svc_upgrade.clone();
                        async move { Ok::<_, Infallible>(handle(&shared, id, req, &upgrade).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(64)
                        .max_buf_size(32 * 1024);
                    if let Err(e) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await
                    {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("GraphQL connection {id}: {e}"));
                    }
                    // An accepted WebSocket upgrade continues in this task, so it keeps the
                    // connection's permit and its place in the server's task registry.
                    let pending = upgrade.lock().ok().and_then(|mut u| u.take());
                    if let Some(on_upgrade) = pending {
                        if let Ok(upgraded) = on_upgrade.await {
                            let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                                TokioIo::new(upgraded),
                                tokio_tungstenite::tungstenite::protocol::Role::Server,
                                Some(ws::ws_config()),
                            )
                            .await;
                            ws::session(child.clone(), id, socket).await;
                        }
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

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Media {
    GraphqlResponse,
    Json,
}

impl Media {
    fn content_type(self) -> &'static str {
        match self {
            Media::GraphqlResponse => "application/graphql-response+json; charset=utf-8",
            Media::Json => "application/json; charset=utf-8",
        }
    }
}

/// Pick the response media type from `Accept`: no header means `application/json` (the spec's
/// legacy rule); otherwise the acceptable type with the highest q, an exact type beating a
/// wildcard, and `None` (406) when neither type is acceptable.
pub fn negotiate(accept: Option<&str>) -> Option<Media> {
    let Some(accept) = accept.filter(|a| !a.trim().is_empty()) else {
        return Some(Media::Json);
    };
    let mut best: Option<(f32, u8, Media)> = None;
    for item in accept.split(',').take(32) {
        let mut parts = item.split(';');
        let ty = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .next()
            .and_then(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        if q <= 0.0 {
            continue;
        }
        let (media, specificity) = match ty.as_str() {
            GRAPHQL_RESPONSE => (Media::GraphqlResponse, 2),
            "application/json" => (Media::Json, 2),
            "application/*" | "*/*" => (Media::GraphqlResponse, 1),
            _ => continue,
        };
        let better = match best {
            None => true,
            Some((bq, bs, bm)) => {
                q > bq
                    || (q == bq && specificity > bs)
                    || (q == bq
                        && specificity == bs
                        && media == Media::GraphqlResponse
                        && bm == Media::Json)
            }
        };
        if better {
            best = Some((q, specificity, media));
        }
    }
    best.map(|(_, _, m)| m)
}

fn respond(status: u16, content_type: &'static str, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    r
}

fn graphql(status: u16, media: Media, body: &Value) -> Response<Full<Bytes>> {
    respond(
        status,
        media.content_type(),
        serde_json::to_vec(body).unwrap_or_default(),
    )
}

/// A request error: 400 under `application/graphql-response+json`, 200 under legacy
/// `application/json` for a well-formed request (the spec's compatibility rule).
fn request_error(media: Media, well_formed: bool, errors: &RequestErrors) -> Response<Full<Bytes>> {
    let status = if media == Media::Json && well_formed {
        200
    } else {
        400
    };
    graphql(status, media, &errors.body())
}

fn not_allowed(allow: &'static str) -> Response<Full<Bytes>> {
    let mut r = respond(405, "text/plain", b"method not allowed".to_vec());
    r.headers_mut()
        .insert(header::ALLOW, header::HeaderValue::from_static(allow));
    r
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("GraphQL connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn handle(
    shared: &Shared,
    id: ConnectionId,
    req: Request<Incoming>,
    upgrade: &UpgradeSlot,
) -> Response<Full<Bytes>> {
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
    let response = route(shared, id, req, upgrade).await;
    let sent = hyper::body::Body::size_hint(response.body())
        .exact()
        .unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    response
}

struct GraphqlRequest {
    query: String,
    operation_name: Option<String>,
    variables: Map<String, Value>,
}

/// Decode a query-string component (`+` is a space, `%XX` a byte).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn decode_params(
    query: Option<Value>,
    operation_name: Option<Value>,
    variables: Option<Value>,
) -> Result<GraphqlRequest, &'static str> {
    let query = match query {
        Some(Value::String(q)) if !q.trim().is_empty() => q,
        _ => return Err("query is required and must be a non-empty string"),
    };
    let operation_name = match operation_name {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) if n.len() <= 256 => Some(n),
        _ => return Err("operationName must be a string"),
    };
    let variables = match variables {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m,
        _ => return Err("variables must be an object"),
    };
    Ok(GraphqlRequest {
        query,
        operation_name,
        variables,
    })
}

fn from_query_string(qs: &str) -> Result<GraphqlRequest, &'static str> {
    let (mut query, mut name, mut vars) = (None, None, None);
    for pair in qs.split('&').filter(|p| !p.is_empty()).take(16) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v).ok_or("query string is not valid UTF-8")?;
        match k {
            "query" => query = Some(Value::String(v)),
            "operationName" if !v.is_empty() => name = Some(Value::String(v)),
            "variables" if !v.is_empty() => {
                vars = Some(serde_json::from_str(&v).map_err(|_| "variables is not JSON")?)
            }
            _ => {}
        }
    }
    decode_params(query, name, vars)
}

async fn route(
    shared: &Shared,
    id: ConnectionId,
    mut req: Request<Incoming>,
    upgrade: &UpgradeSlot,
) -> Response<Full<Bytes>> {
    if req.uri().path() != shared.endpoint {
        return respond(404, "text/plain", b"not found".to_vec());
    }
    if ws::is_upgrade(&req) {
        return match ws::handshake(&req) {
            Ok(accepted) => {
                if let Ok(mut slot) = upgrade.lock() {
                    *slot = Some(hyper::upgrade::on(&mut req));
                }
                accepted.map(|()| Full::new(Bytes::new()))
            }
            Err((status, why)) => respond(status, "text/plain", why.as_bytes().to_vec()),
        };
    }
    let method = req.method().clone();
    if method != Method::GET && method != Method::POST {
        return not_allowed("GET, POST");
    }
    let accept = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let Some(media) = negotiate(accept.as_deref()) else {
        return respond(
            406,
            "text/plain",
            b"accept application/graphql-response+json or application/json".to_vec(),
        );
    };
    let parsed = if method == Method::GET {
        from_query_string(req.uri().query().unwrap_or(""))
    } else {
        let content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if content_type != "application/json" {
            return respond(
                415,
                "text/plain",
                b"POST bodies must be application/json".to_vec(),
            );
        }
        let body = match tokio::time::timeout(
            BODY_TIMEOUT,
            Limited::new(req.into_body(), engine::MAX_BODY_BYTES).collect(),
        )
        .await
        {
            Ok(Ok(b)) => b.to_bytes(),
            _ => {
                return request_error(
                    media,
                    false,
                    &RequestErrors::one("request body unreadable or over 1 MiB"),
                )
            }
        };
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(mut m)) if engine::budget_ok(&Value::Object(m.clone())) => {
                decode_params(
                    m.remove("query"),
                    m.remove("operationName"),
                    m.remove("variables"),
                )
            }
            _ => Err("body must be a JSON object"),
        }
    };
    let request = match parsed {
        Ok(r) => r,
        Err(e) => return request_error(media, false, &RequestErrors::one(e)),
    };
    let prepared = match engine::prepare(
        &shared.schema,
        &request.query,
        request.operation_name.as_deref(),
        &request.variables,
    ) {
        Ok(p) => p,
        Err(errors) => {
            outcome(&shared.ctx, id, "request", "protocol_refusal");
            return request_error(media, true, &errors);
        }
    };
    match prepared.operation_type {
        OperationType::Mutation if method == Method::GET => return not_allowed("POST"),
        OperationType::Subscription => {
            return request_error(
                media,
                true,
                &RequestErrors::one(
                    "subscriptions need a WebSocket using the graphql-transport-ws subprotocol",
                ),
            )
        }
        _ => {}
    }
    match answer_operation(shared, id, &prepared, &request.query, method.as_str()).await {
        Ok(body) => graphql(200, media, &body),
        Err(e) => unavailable(media, &e),
    }
}

/// Collect the handler's answers: custom results named in `allowed`, and a close request as
/// `{"type": "disconnect"}`, in the order the handler gave them.
fn collect_answers(results: Vec<ActionResult>, allowed: &[&str]) -> Vec<Value> {
    let mut out = Vec::new();
    for r in results {
        match r {
            ActionResult::Custom { name, data } if allowed.contains(&name.as_str()) => {
                out.push(data)
            }
            ActionResult::CloseConnection => out.push(json!({"type": "disconnect"})),
            ActionResult::Multiple(items) => out.extend(collect_answers(items, allowed)),
            _ => {}
        }
    }
    out
}

/// Ask the handler about `event`. Every failure is logged with its decision tag and returned
/// as `Err`, which the caller turns into a category error — never into data.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    op_type: &str,
    allowed: &[&str],
) -> anyhow::Result<Vec<Value>> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::GraphqlProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, op_type, "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, op_type, "fail_closed_invalid_reply");
        anyhow::bail!("invalid handler answer");
    }
    Ok(collect_answers(result.protocol_results, allowed))
}

/// Answer a query or mutation: introspection by Rust alone, everything else by executing the
/// handler's data. `Ok` is a complete GraphQL response body.
async fn answer_operation(
    shared: &Shared,
    id: ConnectionId,
    prepared: &engine::Prepared,
    query: &str,
    method: &str,
) -> anyhow::Result<Value> {
    if prepared.is_introspection() {
        let r = prepared.execute(&shared.schema, &json!({}), shared.introspection)?;
        return Ok(serde_json::to_value(r)?);
    }
    let op_type = engine::operation_type_name(prepared.operation_type);
    let event = Event::new(
        &actions::OPERATION_EVENT,
        json!({
            "operation_type": op_type,
            "operation_name": prepared.operation_name,
            "query": query,
            "variables": prepared.variables_json(),
            "root_fields": prepared.root_fields(),
            "shape": prepared.shape(&shared.schema),
            "method": method,
        }),
    );
    let ctx = &shared.ctx;
    let mut answers = ask(
        shared,
        id,
        event,
        op_type,
        &["graphql_result", "graphql_error"],
    )
    .await?;
    let answer = match answers.len() {
        1 => answers.remove(0),
        0 => {
            outcome(ctx, id, op_type, "model_silent");
            anyhow::bail!("no handler answer");
        }
        _ => {
            outcome(ctx, id, op_type, "fail_closed_invalid_reply");
            anyhow::bail!("more than one handler answer");
        }
    };
    if answer["type"] == "graphql_error" {
        outcome(ctx, id, op_type, "model_reject");
        return Ok(json!({"errors": [refusal(&answer)], "data": null}));
    }
    match execute_answer(shared, prepared, &answer) {
        Ok(body) => {
            outcome(ctx, id, op_type, "model_answer");
            Ok(body)
        }
        Err(e) => {
            outcome(ctx, id, op_type, "fail_closed_invalid_reply");
            Err(e)
        }
    }
}

/// A `graphql_error` answer as one GraphQL error.
fn refusal(answer: &Value) -> Value {
    let mut error = json!({"message": answer["message"]});
    if let Some(x) = answer.get("extensions").filter(|x| x.is_object()) {
        error["extensions"] = x.clone();
    }
    error
}

/// Execute a `graphql_result` or `graphql_event` answer over the operation, appending the
/// handler's own errors.
fn execute_answer(
    shared: &Shared,
    prepared: &engine::Prepared,
    answer: &Value,
) -> anyhow::Result<Value> {
    let mut r = prepared.execute(&shared.schema, &answer["data"], shared.introspection)?;
    r.errors
        .extend(engine::handler_errors(answer.get("errors"))?);
    Ok(serde_json::to_value(r)?)
}

/// The handler could not answer: 503 when overloaded, 500 otherwise, with a category message —
/// never the error itself, and never data the handler did not give.
fn unavailable(media: Media, e: &anyhow::Error) -> Response<Full<Bytes>> {
    let failure = crate::utils::wire_failure::WireFailure::classify(e);
    graphql(
        if failure.is_overloaded() { 503 } else { 500 },
        media,
        &json!({"errors": [{"message": failure.text()}]}),
    )
}
