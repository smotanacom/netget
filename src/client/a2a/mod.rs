//! A2A 1.0 client over the JSON-RPC binding.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::a2a::model;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::A2aClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_ALLOW_REDIRECT: bool = false;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STREAM_EVENTS: usize = 256;

/// The card's endpoint is the agent we were pointed at: same host and port, treating the
/// loopback names as one host.
fn same_target(card_host: &str, remote: &str) -> bool {
    let split = |s: &str| {
        s.rsplit_once(':').map(|(h, p)| {
            (
                h.trim_matches(['[', ']']).to_ascii_lowercase(),
                p.to_owned(),
            )
        })
    };
    let loopback = |h: &str| matches!(h, "localhost" | "127.0.0.1" | "::1");
    match (split(card_host), split(remote)) {
        (Some((h1, p1)), Some((h2, p2))) => {
            p1 == p2 && (h1 == h2 || (loopback(&h1) && loopback(&h2)))
        }
        _ => card_host == remote,
    }
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("")
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let allow_redirect = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_bool("allow_card_redirect"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_ALLOW_REDIRECT);
    let base = format!("http://{}", ctx.remote_addr);
    crate::client::http_fetch::check_url(&base)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none()),
            &base,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = FetchClient::transport(TIMEOUT);
    let fetch = fetch
        .with_max_body(model::MAX_BODY_BYTES)
        .with_user_agent("netget-a2a");
    let response = fetch
        .get(&format!("{base}{}", model::CARD_PATH))
        .send()
        .await
        .context("fetching the agent card")?;
    ensure!(
        response.status().as_u16() == 200,
        "agent card request answered {}",
        response.status()
    );
    let card: Value =
        serde_json::from_slice(&response.bytes().await?).context("agent card is not JSON")?;
    ensure!(model::budget_ok(&card), "agent card exceeds the bounds");
    let rpc_url = model::card_rpc_url(&card)?;
    if !allow_redirect {
        let card_host = host_of(&rpc_url);
        ensure!(same_target(card_host, &ctx.remote_addr), "agent card sends JSON-RPC to {card_host}, not {}; set allow_card_redirect to follow it", ctx.remote_addr);
    }
    let streaming = card["capabilities"]["streaming"].as_bool().unwrap_or(false);
    let local: SocketAddr = "0.0.0.0:0".parse()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"name": card["name"], "description": card["description"], "skills": card["skills"], "streaming": streaming, "rpc_url": rpc_url}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = A2aClientProtocol;
        while let Some(event) = event_rx.recv().await {
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
                &protocol,
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
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("A2A client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            &fetch,
            &rpc_url,
            streaming,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("A2A client ended: {e}"));
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
    Ok(local)
}

fn request_for(action: &Value, streaming_offered: bool) -> Result<(String, Value)> {
    Ok(match action["type"].as_str().unwrap_or_default() {
        "a2a_send_message" => {
            let mut parts = vec![json!({"text": action["text"]})];
            if action.get("data").is_some_and(|d| !d.is_null()) {
                parts.push(json!({"data": action["data"]}));
            }
            let mut message =
                json!({"messageId": model::new_id(), "role": "ROLE_USER", "parts": parts});
            for (k, w) in [("context_id", "contextId"), ("task_id", "taskId")] {
                if let Some(v) = action.get(k).and_then(Value::as_str) {
                    message[w] = json!(v);
                }
            }
            let stream = action["stream"].as_bool().unwrap_or(false);
            ensure!(
                !stream || streaming_offered,
                "the agent card does not advertise streaming"
            );
            (
                if stream {
                    "SendStreamingMessage"
                } else {
                    "SendMessage"
                }
                .into(),
                json!({"message": message}),
            )
        }
        "a2a_get_task" => {
            let mut p = json!({"id": action["task_id"]});
            if let Some(h) = action.get("history_length").and_then(Value::as_u64) {
                p["historyLength"] = json!(h);
            }
            ("GetTask".into(), p)
        }
        "a2a_cancel_task" => ("CancelTask".into(), json!({"id": action["task_id"]})),
        "a2a_list_tasks" => {
            let mut p = json!({});
            if let Some(c) = action.get("context_id").and_then(Value::as_str) {
                p["contextId"] = json!(c);
            }
            if let Some(n) = action.get("page_size").and_then(Value::as_u64) {
                p["pageSize"] = json!(n);
            }
            ("ListTasks".into(), p)
        }
        other => bail!("unsupported action {other}"),
    })
}

async fn session(
    ctx: &ConnectContext,
    fetch: &FetchClient,
    rpc_url: &str,
    streaming_offered: bool,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut next_id = 1u64;
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let prepared = match A2aClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => request_for(&action, streaming_offered),
            Err(e) => Err(e),
        };
        let (method, params) = match prepared {
            Ok(p) => p,
            Err(e) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        };
        let body = json!({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params});
        next_id += 1;
        let outcome = exchange(fetch, rpc_url, &method, &body).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "A2A",
                    None,
                    "injected_action",
                    json!({"method": method}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Sent {
                            bytes_sent: body.to_string().len(),
                        }),
                    );
                }
                events
                    .try_send(Event::new(&actions::RESPONSE_EVENT, data))
                    .context("A2A event queue full; consumer stalled")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("A2A {method} failed: {e}"));
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Err(e));
                }
            }
        }
    }
}

fn summarize(data: &mut Value, result: &Value) {
    let task = result.get("task").unwrap_or(result);
    if let Some(id) = task
        .get("id")
        .or_else(|| result.get("statusUpdate").and_then(|u| u.get("taskId")))
    {
        data["task_id"] = id.clone();
    }
    let status = task
        .get("status")
        .or_else(|| result.get("statusUpdate").and_then(|u| u.get("status")));
    if let Some(s) = status
        .and_then(|s| s["state"].as_str())
        .and_then(model::state_short)
    {
        data["state"] = json!(s);
    }
}

async fn exchange(fetch: &FetchClient, url: &str, method: &str, body: &Value) -> Result<Value> {
    let response = fetch
        .post(url)
        .header("Content-Type", "application/json")
        .header(model::VERSION_HEADER, model::PROTOCOL_VERSION)
        .body(body.to_string())
        .send()
        .await?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes = response.bytes().await?;
    ensure!(status == 200, "JSON-RPC endpoint answered HTTP {status}");
    let mut data = json!({"method": method});
    let responses: Vec<Value> = if content_type.starts_with("text/event-stream") {
        let text = std::str::from_utf8(&bytes).context("SSE stream is not UTF-8")?;
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                ensure!(
                    out.len() < MAX_STREAM_EVENTS,
                    "stream exceeds {MAX_STREAM_EVENTS} events"
                );
                out.push(
                    serde_json::from_str::<Value>(payload.trim())
                        .context("SSE event is not JSON")?,
                );
            }
        }
        ensure!(!out.is_empty(), "empty SSE stream");
        out
    } else {
        vec![serde_json::from_slice(&bytes).context("JSON-RPC response is not JSON")?]
    };
    let mut stream_events = Vec::new();
    for r in &responses {
        ensure!(
            r["jsonrpc"] == "2.0" && r["id"] == body["id"],
            "JSON-RPC response id does not match the request"
        );
        if let Some(e) = r.get("error").filter(|e| !e.is_null()) {
            data["error"] = json!({"code": e["code"], "message": e["message"]});
            return Ok(data);
        }
        let result = r
            .get("result")
            .context("JSON-RPC response has neither result nor error")?;
        model::check_result(method, result)?;
        summarize(&mut data, result);
        stream_events.push(result.clone());
    }
    if method == "SendStreamingMessage" {
        data["stream_events"] = Value::Array(stream_events);
    } else {
        data["result"] = stream_events.pop().unwrap_or(Value::Null);
    }
    Ok(data)
}
