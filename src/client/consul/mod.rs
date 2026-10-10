//! Consul HTTP API client: KV, catalog, health and service registration, one HTTP/1.1
//! connection per request; KV values come back decoded from base64.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::consul::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ConsulClientProtocol;
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    client::conn::http1,
    header::{CONNECTION, CONTENT_LENGTH, HOST},
    Request, Uri,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// Largest answer read.
pub const MAX_ANSWER_BYTES: usize = 1024 * 1024;
/// One request, connect to last byte.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;

#[derive(Clone)]
struct Origin {
    authority: String,
    connect_addr: String,
}

fn origin(value: &str) -> Result<Origin> {
    let value = if value.starts_with("http://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let uri: Uri = value.parse().context("invalid Consul agent address")?;
    ensure!(
        matches!(uri.path(), "" | "/") && uri.query().is_none(),
        "give the agent as host:port or http://host:port, without a path"
    );
    let authority = uri.authority().context("agent host required")?;
    Ok(Origin {
        authority: authority.as_str().to_string(),
        connect_addr: format!(
            "{}:{}",
            authority.host(),
            authority.port_u16().unwrap_or(8500)
        ),
    })
}

struct Answer {
    status: u16,
    index: u64,
    body: Bytes,
}

async fn exchange(
    origin: &Origin,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<Answer> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let socket = tokio::net::TcpStream::connect(&origin.connect_addr).await?;
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(64)
            .max_buf_size(32 * 1024)
            .handshake(TokioIo::new(socket))
            .await?;
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, &origin.authority)
            .header(CONNECTION, "close");
        let payload = body.unwrap_or_default();
        request = request.header(CONTENT_LENGTH, payload.len());
        let request = request.body(Full::new(Bytes::from(payload)))?;
        let exchange = async {
            let response = sender.send_request(request).await?;
            let (parts, body) = response.into_parts();
            let body = Limited::new(body, MAX_ANSWER_BYTES)
                .collect()
                .await
                .map_err(|_| anyhow::anyhow!("response body exceeds 1 MiB or is incomplete"))?
                .to_bytes();
            Ok::<_, anyhow::Error>(Answer {
                status: parts.status.as_u16(),
                index: parts
                    .headers
                    .get("X-Consul-Index")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                body,
            })
        };
        tokio::pin!(connection);
        tokio::pin!(exchange);
        tokio::select! {
            r = &mut exchange => r,
            r = &mut connection => {
                r.context("HTTP connection failed")?;
                exchange.await
            }
        }
    })
    .await
    .context("Consul request deadline exceeded")?
}

fn message(answer: &Answer) -> String {
    let text = String::from_utf8_lossy(&answer.body);
    crate::utils::truncate::truncate_for_log(text.trim(), 1024).to_string()
}

/// The result an answer carries, in the handler's terms.
fn result(operation: &str, keys_only: bool, body: &[u8]) -> Value {
    let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match operation {
        "consul_kv_get" if !keys_only => Value::Array(
            json.as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    let bytes = e["Value"].as_str().and_then(|v| wire::unb64(v).ok()).unwrap_or_default();
                    let (value, encoding) = wire::shown(&bytes);
                    json!({"key": e["Key"], "value": value, "encoding": encoding, "flags": e["Flags"],
                           "modify_index": e["ModifyIndex"]})
                })
                .collect(),
        ),
        "consul_catalog" => match &json {
            Value::Array(items) if items.first().is_some_and(|i| i.get("ServiceName").is_some() || i.get("Service").is_some()) => Value::Array(
                items
                    .iter()
                    .map(|i| {
                        let s = if i.get("Service").is_some_and(Value::is_object) { &i["Service"] } else { i };
                        let pick = |a: &str, b: &str| s.get(a).or_else(|| s.get(b)).cloned().unwrap_or(Value::Null);
                        json!({"id": pick("ServiceID", "ID"), "name": pick("ServiceName", "Service"),
                               "address": pick("ServiceAddress", "Address"), "port": pick("ServicePort", "Port"),
                               "tags": pick("ServiceTags", "Tags"),
                               "status": i["Checks"].as_array().map(|c| if c.iter().all(|c| c["Status"] == "passing") { "passing" } else { "failing" })})
                    })
                    .collect(),
            ),
            other => other.clone(),
        },
        _ => json,
    }
}

/// Perform one validated action and describe the outcome as the event it raises.
async fn perform(origin: &Origin, action: &Value) -> Result<Event> {
    let (method, path, body) = actions::request(action)?;
    let answer = exchange(origin, method, &path, body).await?;
    let operation = action["type"].as_str().unwrap_or_default();
    let ok = answer.status == 200;
    let message = if ok { String::new() } else { message(&answer) };
    let result = if ok {
        result(operation, action["keys_only"] == true, &answer.body)
    } else {
        Value::Null
    };
    Ok(Event::new(
        &actions::RESPONSE_EVENT,
        json!({"operation": operation, "status": answer.status, "result": result, "index": answer.index, "message": message}),
    ))
}

pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let origin = origin(&ctx.remote_addr)?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(32);
    event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({"remote_addr": format!("http://{}", origin.authority)}),
        ),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            let instruction = events_ctx
                .state
                .get_instruction_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = events_ctx
                .state
                .get_memory_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &ConsulClientProtocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("Consul client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &origin, external, internal_rx, event_tx).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Consul client ended: {e}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, status)
            .await;
        session_ctx
            .state
            .remove_client_handle(session_ctx.client_id)
            .await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok("0.0.0.0:0".parse()?)
}

async fn session(
    ctx: &ConnectContext,
    origin: &Origin,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) -> Result<()> {
    let log = Log::new(Some(&ctx.status_tx));
    loop {
        let (action, depth, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some((a, depth)) => (a, depth, None),
                None => return Ok(()),
            },
        };
        let reply = |injected: &mut Option<ClientCommand>, outcome: ClientSendOutcome| {
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(command, Ok(outcome));
            }
        };
        match ConsulClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("Consul client action refused: {e:#}"));
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                continue;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "Consul client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Consul",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        match perform(origin, &action).await {
            Ok(event) => {
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "Consul",
                        None,
                        event.id(),
                        event.data.clone(),
                        vec![],
                    )
                    .await;
                reply(
                    &mut injected,
                    ClientSendOutcome::Executed {
                        detail: event.data.to_string(),
                    },
                );
                events
                    .try_send((event, depth))
                    .context("Consul event queue full; consumer stalled")?;
            }
            Err(e) => {
                log.warn(format!("Consul request failed: {e:#}"));
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: format!("{e:#}"),
                    },
                );
            }
        }
    }
}
