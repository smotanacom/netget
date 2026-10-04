//! A2A 1.0 agent over HTTP/1.1: agent card, JSON-RPC dispatch, SSE streaming. Rust owns the
//! envelope, ids and version rules; the handler owns every reply.
pub mod actions;
pub mod model;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const DEFAULT_NAME: &str = "NetGet Agent";
pub const DEFAULT_DESCRIPTION: &str = "An A2A agent served by NetGet";
pub const DEFAULT_STREAMING: bool = true;
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

struct Shared {
    ctx: SpawnContext,
    name: String,
    description: String,
    skills: Vec<Value>,
    streaming: bool,
    local: SocketAddr,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str, d: &str| -> Result<String> {
        Ok(p.map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| d.to_owned()))
    };
    let name = s("agent_name", DEFAULT_NAME)?;
    let description = s("agent_description", DEFAULT_DESCRIPTION)?;
    ensure!(
        !name.is_empty() && name.len() <= 256 && description.len() <= 4096,
        "agent_name/agent_description too long"
    );
    let skills = match p
        .map(|p| p.get_optional_array("skills"))
        .transpose()?
        .flatten()
    {
        None => vec![
            json!({"id": "chat", "name": "Chat", "description": "Converse with the agent", "tags": ["chat"]}),
        ],
        Some(list) => {
            ensure!(list.len() <= 64, "at most 64 skills");
            list.iter()
                .map(|sk| {
                    let id = sk["id"].as_str().context("each skill needs an id")?;
                    let name = sk["name"].as_str().context("each skill needs a name")?;
                    Ok(json!({"id": id, "name": name, "description": sk["description"].as_str().unwrap_or(""), "tags": sk.get("tags").cloned().unwrap_or(json!([])), "examples": sk.get("examples").cloned().unwrap_or(json!([]))}))
                })
                .collect::<Result<_>>()?
        }
    };
    let streaming = p
        .map(|p| p.get_optional_bool("streaming"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_STREAMING);
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "A2A agent '{name}' at http://{local}/ (card {})",
        model::CARD_PATH
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        name,
        description,
        skills,
        streaming,
        local,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "A2A", Some(&shared.ctx.status_tx)).await {
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
                            .debug(format!("A2A connection {id}: {e}"));
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

fn respond(status: u16, content_type: &'static str, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    r
}
fn json_response(value: &Value) -> Response<Full<Bytes>> {
    respond(
        200,
        "application/json",
        serde_json::to_vec(value).unwrap_or_default(),
    )
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("A2A connection {id} method={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn decide(shared: &Shared, id: ConnectionId, event: Event) -> Result<Value> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::A2aProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        bail!("A2A handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == "a2a_reply" => {
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("A2A handler supplied more than one reply");
                }
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    found
        .context("A2A handler did not answer")
        .inspect_err(|_| outcome(ctx, id, event.id(), "model_silent"))
}

async fn handle(
    shared: &Shared,
    id: ConnectionId,
    req: Request<Incoming>,
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
    let response = route(shared, id, req).await;
    let sent = hyper::body::Body::size_hint(response.body())
        .exact()
        .unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    response
}

async fn route(shared: &Shared, id: ConnectionId, req: Request<Incoming>) -> Response<Full<Bytes>> {
    if req.method() == Method::GET && req.uri().path() == model::CARD_PATH {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .filter(|h| !h.is_empty() && h.len() <= 255 && !h.contains(['/', ' ']))
            .map(str::to_owned)
            .unwrap_or_else(|| shared.local.to_string());
        let card = model::card(
            &shared.name,
            &shared.description,
            "1.0.0",
            &format!("http://{host}/"),
            shared.streaming,
            &shared.skills,
        );
        return json_response(&card);
    }
    if req.method() != Method::POST || req.uri().path() != "/" {
        return respond(404, "text/plain", b"not found".to_vec());
    }
    let version = req
        .headers()
        .get(model::VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(req.into_body(), model::MAX_BODY_BYTES).collect(),
    )
    .await
    {
        Ok(Ok(b)) => b.to_bytes(),
        _ => {
            return json_response(&model::rpc_error(
                &Value::Null,
                -32600,
                "request body unreadable or over 1 MiB",
            ))
        }
    };
    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) if model::budget_ok(&v) => v,
        _ => return json_response(&model::rpc_error(&Value::Null, -32700, "parse error")),
    };
    let rpc_id = request.get("id").cloned().unwrap_or(Value::Null);
    if request["jsonrpc"] != "2.0"
        || !matches!(rpc_id, Value::String(_) | Value::Number(_) | Value::Null)
    {
        return json_response(&model::rpc_error(
            &rpc_id,
            -32600,
            "invalid JSON-RPC 2.0 request",
        ));
    }
    let Some(method) = request["method"].as_str() else {
        return json_response(&model::rpc_error(&rpc_id, -32600, "method is required"));
    };
    if !model::version_ok(version.as_deref()) {
        return json_response(&model::rpc_error(
            &rpc_id,
            model::VERSION_NOT_SUPPORTED,
            "A2A-Version 1.0 is required",
        ));
    }
    let params = request.get("params").cloned().unwrap_or(json!({}));
    if !params.is_object() {
        return json_response(&model::rpc_error(
            &rpc_id,
            -32602,
            "params must be an object",
        ));
    }
    match method {
        "SendMessage" | "SendStreamingMessage" => send(shared, id, method, &rpc_id, &params).await,
        "GetTask" | "CancelTask" | "ListTasks" => {
            task_request(shared, id, method, &rpc_id, &params).await
        }
        "SubscribeToTask"
        | "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => json_response(&model::rpc_error(
            &rpc_id,
            if method == "SubscribeToTask" {
                model::UNSUPPORTED_OPERATION
            } else {
                model::PUSH_NOT_SUPPORTED
            },
            "not supported by this agent",
        )),
        "GetExtendedAgentCard" => json_response(&model::rpc_error(
            &rpc_id,
            model::EXTENDED_CARD_NOT_CONFIGURED,
            "no extended agent card",
        )),
        _ => json_response(&model::rpc_error(&rpc_id, -32601, "method not found")),
    }
}

fn internal(rpc_id: &Value, streaming: bool) -> Response<Full<Bytes>> {
    let err = model::rpc_error(rpc_id, -32603, "the agent cannot answer right now");
    if streaming {
        respond(200, "text/event-stream", model::sse(&err).into_bytes())
    } else {
        json_response(&err)
    }
}

fn mapped_error(rpc_id: &Value, v: &Value) -> Value {
    let code = v["error"]["code"].as_str().unwrap_or("");
    let num = actions::ERROR_NAMES
        .iter()
        .find(|(n, _)| *n == code)
        .map(|(_, c)| *c)
        .unwrap_or(-32603);
    model::rpc_error(rpc_id, num, v["error"]["message"].as_str().unwrap_or(code))
}

async fn send(
    shared: &Shared,
    id: ConnectionId,
    method: &str,
    rpc_id: &Value,
    params: &Value,
) -> Response<Full<Bytes>> {
    let ctx = &shared.ctx;
    let streaming = method == "SendStreamingMessage";
    if streaming && !shared.streaming {
        return json_response(&model::rpc_error(
            rpc_id,
            model::UNSUPPORTED_OPERATION,
            "streaming is not offered",
        ));
    }
    let incoming = match model::incoming_message(params) {
        Ok(m) => m,
        Err(e) => {
            outcome(ctx, id, method, "protocol_refusal");
            return json_response(&model::rpc_error(rpc_id, -32602, &e.to_string()));
        }
    };
    let context_id = incoming.context_id.clone().unwrap_or_else(model::new_id);
    let event = Event::new(
        &actions::MESSAGE_EVENT,
        json!({"method": method, "text": incoming.text, "parts": incoming.parts, "message_id": incoming.message_id, "context_id": context_id, "task_id": incoming.task_id}),
    );
    let reply = match decide(shared, id, event).await {
        Ok(v) => v,
        Err(_) => return internal(rpc_id, streaming),
    };
    let built = (|| -> Result<Value> {
        actions::validate_reply(&reply)?;
        if reply.get("error").is_some_and(|e| !e.is_null()) {
            return Ok(json!({"error": mapped_error(rpc_id, &reply)}));
        }
        if reply.get("message").is_some_and(|m| !m.is_null()) {
            return Ok(
                json!({"message": model::agent_message(reply["message"]["text"].as_str(), reply["message"].get("parts"), &context_id, incoming.task_id.as_deref())?}),
            );
        }
        let task = reply
            .get("task")
            .filter(|t| !t.is_null())
            .context("SendMessage is answered with message, task or error")?;
        let default_id = incoming.task_id.clone().unwrap_or_else(model::new_id);
        Ok(json!({"task": model::task(task, &default_id, &context_id)?}))
    })();
    let built = match built {
        Ok(b) => b,
        Err(_) => {
            outcome(ctx, id, method, "fail_closed_invalid_reply");
            return internal(rpc_id, streaming);
        }
    };
    if let Some(err) = built.get("error") {
        outcome(ctx, id, method, "model_reject");
        return if streaming {
            respond(200, "text/event-stream", model::sse(err).into_bytes())
        } else {
            json_response(err)
        };
    }
    outcome(ctx, id, method, "model_answer");
    if !streaming {
        return json_response(&model::rpc_result(rpc_id, built));
    }
    let events = match built.get("task") {
        Some(task) => model::stream_events(rpc_id, task),
        None => vec![model::rpc_result(rpc_id, built)],
    };
    respond(
        200,
        "text/event-stream",
        events
            .iter()
            .map(model::sse)
            .collect::<String>()
            .into_bytes(),
    )
}

async fn task_request(
    shared: &Shared,
    id: ConnectionId,
    method: &str,
    rpc_id: &Value,
    params: &Value,
) -> Response<Full<Bytes>> {
    let ctx = &shared.ctx;
    let task_id = params.get("id").and_then(Value::as_str).map(str::to_owned);
    if method != "ListTasks"
        && task_id
            .as_deref()
            .is_none_or(|t| t.is_empty() || t.len() > 256)
    {
        return json_response(&model::rpc_error(rpc_id, -32602, "params.id is required"));
    }
    let event = Event::new(
        &actions::TASK_EVENT,
        json!({"method": method, "task_id": task_id, "params": params}),
    );
    let reply = match decide(shared, id, event).await {
        Ok(v) => v,
        Err(_) => return internal(rpc_id, false),
    };
    let built = (|| -> Result<Value> {
        actions::validate_reply(&reply)?;
        if reply.get("error").is_some_and(|e| !e.is_null()) {
            return Ok(mapped_error(rpc_id, &reply));
        }
        match method {
            "ListTasks" => {
                let list = reply
                    .get("tasks")
                    .and_then(Value::as_array)
                    .context("ListTasks is answered with tasks or error")?;
                let tasks: Vec<Value> = list
                    .iter()
                    .map(|t| {
                        model::task(t, "unused", t["context_id"].as_str().unwrap_or("default"))
                    })
                    .collect::<Result<_>>()?;
                Ok(model::rpc_result(
                    rpc_id,
                    json!({"tasks": tasks, "pageSize": tasks.len(), "totalSize": tasks.len(), "nextPageToken": ""}),
                ))
            }
            _ => {
                let spec = reply
                    .get("task")
                    .filter(|t| !t.is_null())
                    .context("GetTask/CancelTask are answered with task or error")?;
                let tid = task_id.clone().unwrap_or_default();
                let task =
                    model::task(spec, &tid, spec["context_id"].as_str().unwrap_or("default"))?;
                ensure!(task["id"] == tid, "the answer describes a different task");
                if method == "CancelTask" {
                    ensure!(task["status"]["state"] == "TASK_STATE_CANCELED", "a successful CancelTask returns the task canceled; use task_not_cancelable otherwise");
                }
                Ok(model::rpc_result(rpc_id, task))
            }
        }
    })();
    match built {
        Ok(v) => {
            outcome(
                ctx,
                id,
                method,
                if v.get("error").is_some() {
                    "model_reject"
                } else {
                    "model_answer"
                },
            );
            json_response(&v)
        }
        Err(_) => {
            outcome(ctx, id, method, "fail_closed_invalid_reply");
            internal(rpc_id, false)
        }
    }
}
