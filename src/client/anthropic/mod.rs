//! Anthropic Messages API client: messages (plain, or server-sent events reassembled into one
//! message), count_tokens and models, one HTTP/1.1 connection per request.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::anthropic::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::AnthropicClientProtocol;
use actions::Defaults;
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    client::conn::http1,
    header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST},
    Request, Uri,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

/// One request, connect to last byte: generation can be slow.
pub const IO_TIMEOUT: Duration = Duration::from_secs(120);
/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// Most server-sent events read from one stream, and content blocks assembled from it.
pub const MAX_STREAM_EVENTS: usize = 100_000;

#[derive(Clone)]
struct Origin {
    authority: String,
    connect_addr: String,
}

fn origin(value: &str) -> Result<Origin> {
    ensure!(
        !value.starts_with("https://"),
        "plain HTTP only: point the client at a local or proxied server"
    );
    let value = if value.starts_with("http://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let uri: Uri = value.parse().context("invalid API address")?;
    ensure!(
        matches!(uri.path(), "" | "/") && uri.query().is_none(),
        "give the API as host:port or http://host:port, without a path"
    );
    let authority = uri.authority().context("API host required")?;
    Ok(Origin {
        authority: authority.as_str().to_string(),
        connect_addr: format!(
            "{}:{}",
            authority.host(),
            authority.port_u16().unwrap_or(80)
        ),
    })
}

struct Answer {
    status: u16,
    event_stream: bool,
    body: Bytes,
}

async fn exchange(
    origin: &Origin,
    d: &Defaults,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Answer> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let socket = tokio::net::TcpStream::connect(&origin.connect_addr).await?;
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(100)
            .max_buf_size(64 * 1024)
            .handshake(TokioIo::new(socket))
            .await?;
        let payload = body
            .map(|b| serde_json::to_vec(&b))
            .transpose()?
            .unwrap_or_default();
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, &origin.authority)
            .header(CONNECTION, "close")
            .header("anthropic-version", &d.version)
            .header(CONTENT_LENGTH, payload.len());
        if method == "POST" {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        if let Some(k) = &d.api_key {
            request = request.header("x-api-key", k);
        }
        let request = request.body(Full::new(Bytes::from(payload)))?;
        let exchange = async {
            let response = sender.send_request(request).await?;
            let (parts, body) = response.into_parts();
            let event_stream = parts
                .headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"));
            let body = Limited::new(body, wire::MAX_BODY)
                .collect()
                .await
                .map_err(|_| anyhow::anyhow!("response body exceeds 8 MiB or is incomplete"))?
                .to_bytes();
            Ok::<_, anyhow::Error>(Answer {
                status: parts.status.as_u16(),
                event_stream,
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
    .context("Anthropic request deadline exceeded")?
}

/// Reassemble a server-sent event stream into the message it describes, counting each event
/// type. An `error` event mid-stream is returned as the error.
pub fn assemble_stream(body: &str) -> Result<(Value, BTreeMap<String, u64>)> {
    let mut counts = BTreeMap::new();
    let mut message: Option<Value> = None;
    let mut blocks: Vec<Value> = Vec::new();
    let mut partial_json: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for frame in body.replace("\r\n", "\n").split("\n\n") {
        let data: String = frame
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        seen += 1;
        ensure!(
            seen <= MAX_STREAM_EVENTS,
            "more than {MAX_STREAM_EVENTS} events in one stream"
        );
        let ev: Value = serde_json::from_str(&data).context("an event whose data is not JSON")?;
        let kind = ev["type"].as_str().unwrap_or("unknown").to_string();
        *counts.entry(kind.clone()).or_insert(0) += 1;
        let index = ev["index"].as_u64().unwrap_or(0) as usize;
        match kind.as_str() {
            "message_start" => message = Some(ev["message"].clone()),
            "content_block_start" => {
                ensure!(
                    index == blocks.len() && index < wire::MAX_REPLY_BLOCKS,
                    "content block {index} out of order"
                );
                let mut block = ev["content_block"].clone();
                if block["type"] == "tool_use" {
                    block["input"] = json!({});
                }
                blocks.push(block);
                partial_json.push(String::new());
            }
            "content_block_delta" => {
                let block = blocks
                    .get_mut(index)
                    .context("delta for a block that never started")?;
                let delta = &ev["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let t = format!(
                            "{}{}",
                            block["text"].as_str().unwrap_or_default(),
                            delta["text"].as_str().unwrap_or_default()
                        );
                        ensure!(
                            t.len() <= wire::MAX_REPLY_TEXT,
                            "streamed text exceeds 1 MiB"
                        );
                        block["text"] = json!(t);
                    }
                    Some("input_json_delta") => partial_json[index]
                        .push_str(delta["partial_json"].as_str().unwrap_or_default()),
                    Some("thinking_delta") => {
                        let t = format!(
                            "{}{}",
                            block["thinking"].as_str().unwrap_or_default(),
                            delta["thinking"].as_str().unwrap_or_default()
                        );
                        block["thinking"] = json!(t);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let (Some(block), Some(raw)) = (blocks.get_mut(index), partial_json.get(index)) {
                    if block["type"] == "tool_use" && !raw.is_empty() {
                        block["input"] =
                            serde_json::from_str(raw).context("tool input that is not JSON")?;
                    }
                }
            }
            "message_delta" => {
                let m = message
                    .as_mut()
                    .context("message_delta before message_start")?;
                m["stop_reason"] = ev["delta"]["stop_reason"].clone();
                m["stop_sequence"] = ev["delta"]["stop_sequence"].clone();
                if let Some(n) = ev["usage"]["output_tokens"].as_u64() {
                    m["usage"]["output_tokens"] = json!(n);
                }
            }
            "error" => bail!(
                "stream error {}: {}",
                ev["error"]["type"].as_str().unwrap_or("unknown"),
                ev["error"]["message"].as_str().unwrap_or_default()
            ),
            _ => {}
        }
    }
    let mut m = message.context("a stream without message_start")?;
    ensure!(
        counts.contains_key("message_stop"),
        "a stream that ended before message_stop"
    );
    m["content"] = Value::Array(blocks);
    Ok((m, counts))
}

/// A message as the handler sees it: its text joined, its blocks, why it stopped, its usage.
fn shown_message(m: &Value) -> Value {
    let content = m["content"].as_array().cloned().unwrap_or_default();
    let text: String = content
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    json!({"id": m["id"], "model": m["model"], "text": text, "content": content,
           "stop_reason": m["stop_reason"], "stop_sequence": m["stop_sequence"], "usage": m["usage"]})
}

/// Perform one validated action and describe the outcome as the event it raises.
async fn perform(origin: &Origin, d: &Defaults, action: &Value) -> Result<Event> {
    let (method, path, body) = actions::request(action, d)?;
    let answer = exchange(origin, d, method, &path, body).await?;
    let operation = action["type"].as_str().unwrap_or_default();
    let mut data = Map::new();
    data.insert("operation".into(), json!(operation));
    data.insert("status".into(), json!(answer.status));
    if answer.status != 200 {
        let json: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        let error = if json["error"].is_object() {
            json!({"type": json["error"]["type"], "message": json["error"]["message"]})
        } else {
            let text = String::from_utf8_lossy(&answer.body);
            json!({"type": Value::Null, "message": crate::utils::truncate::truncate_for_log(text.trim(), 1024)})
        };
        data.insert("error".into(), error);
        return Ok(Event::new(&actions::RESPONSE_EVENT, Value::Object(data)));
    }
    match operation {
        "anthropic_create_message" if answer.event_stream => {
            let (m, counts) = assemble_stream(&String::from_utf8_lossy(&answer.body))?;
            data.insert("message".into(), shown_message(&m));
            data.insert("streamed".into(), json!(counts));
        }
        "anthropic_create_message" => {
            let m: Value =
                serde_json::from_slice(&answer.body).context("a message that is not JSON")?;
            ensure!(m["type"] == "message", "the answer is not a message object");
            data.insert("message".into(), shown_message(&m));
        }
        "anthropic_count_tokens" => {
            let v: Value =
                serde_json::from_slice(&answer.body).context("a count that is not JSON")?;
            data.insert("input_tokens".into(), v["input_tokens"].clone());
        }
        "anthropic_list_models" => {
            let v: Value =
                serde_json::from_slice(&answer.body).context("a model list that is not JSON")?;
            let ids: Vec<Value> = v["data"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| m["id"].clone())
                .collect();
            data.insert("models".into(), Value::Array(ids));
        }
        _ => {
            let v: Value =
                serde_json::from_slice(&answer.body).context("a model that is not JSON")?;
            data.insert("models".into(), json!([v["id"]]));
        }
    }
    Ok(Event::new(&actions::RESPONSE_EVENT, Value::Object(data)))
}

pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let origin = origin(&ctx.remote_addr)?;
    let params = ctx.startup_params.as_ref();
    let get = |k: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten())
    };
    let defaults = Defaults {
        api_key: get("api_key")?.filter(|k| !k.is_empty()),
        version: get("anthropic_version")?.unwrap_or_else(|| actions::API_VERSION.to_string()),
        model: get("model")?,
    };
    ensure!(
        hyper::header::HeaderValue::from_str(&defaults.version).is_ok()
            && defaults
                .api_key
                .as_deref()
                .is_none_or(|k| hyper::header::HeaderValue::from_str(k).is_ok()),
        "anthropic_version and api_key must be valid header values"
    );
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
                &AnthropicClientProtocol,
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
                    .warn(format!("Anthropic client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            &origin,
            &defaults,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Anthropic client ended: {e}"));
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
    defaults: &Defaults,
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
        let checked = AnthropicClientProtocol
            .execute_action(action.clone())
            .and_then(|r| match r {
                ClientActionResult::Disconnect => Ok(r),
                _ => actions::request(&action, defaults).map(|_| r),
            });
        match checked {
            Ok(ClientActionResult::Disconnect) => {
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("Anthropic client action refused: {e:#}"));
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
                "Anthropic client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Anthropic",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        match perform(origin, defaults, &action).await {
            // The dispatcher's call_llm_for_client records the event with the handler's answer.
            Ok(event) => {
                reply(
                    &mut injected,
                    ClientSendOutcome::Executed {
                        detail: event.data.to_string(),
                    },
                );
                events
                    .try_send((event, depth))
                    .context("Anthropic event queue full; consumer stalled")?;
            }
            Err(e) => {
                log.warn(format!("Anthropic request failed: {e:#}"));
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
