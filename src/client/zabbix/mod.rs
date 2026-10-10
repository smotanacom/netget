//! Zabbix client: passive checks against agents and sender data to trappers, one ZBXD request
//! per connection, framed with the trapper server's own `wire` module.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::zabbix::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ZabbixClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// One request, connect to answer, at most this long unless `timeout_secs` says otherwise.
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_TIMEOUT_SECS: u64 = 300;
/// The text an agent puts before the reason when it cannot answer a key.
pub const NOT_SUPPORTED: &str = "ZBX_NOTSUPPORTED";

pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let secs = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_TIMEOUT_SECS).contains(&secs),
        "timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}"
    );
    let timeout = Duration::from_secs(secs);
    let target = ctx.remote_addr.clone();
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(32);
    event_tx.try_send((
        Event::new(&actions::READY_EVENT, json!({"remote_addr": target})),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "Zabbix",
                    None,
                    event.id(),
                    event.data.clone(),
                    vec![],
                )
                .await;
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
                &ZabbixClientProtocol,
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
                    .warn(format!("Zabbix client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        session(
            &session_ctx,
            &target,
            timeout,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        dispatcher_abort.abort();
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
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

/// One ZBXD request and its answer's data.
async fn exchange(target: &str, request: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    tokio::time::timeout(timeout, async {
        let mut s = TcpStream::connect(target)
            .await
            .with_context(|| format!("connect {target}"))?;
        s.write_all(&wire::encode(request)).await?;
        let mut head = [0u8; wire::LARGE_HEADER_LEN];
        s.read_exact(&mut head[..5])
            .await
            .context("the peer closed before answering")?;
        let len = wire::header_len(head[4]);
        s.read_exact(&mut head[5..len]).await?;
        let h = wire::parse_header(&head[..len])
            .map_err(|e| anyhow::anyhow!("not a Zabbix answer: {e:?}"))?;
        let mut data = vec![0u8; h.data_len as usize];
        s.read_exact(&mut data).await?;
        Ok(data)
    })
    .await
    .context("the request timed out")?
}

async fn get(target: &str, key: &str, timeout: Duration) -> Value {
    match exchange(target, key.as_bytes(), timeout).await {
        Ok(data) => {
            let text = String::from_utf8_lossy(&data).to_string();
            match text.strip_prefix(NOT_SUPPORTED) {
                Some(rest) => {
                    json!({"key": key, "supported": false, "error": rest.trim_start_matches('\0').trim()})
                }
                None => json!({"key": key, "supported": true, "value": text}),
            }
        }
        Err(e) => json!({"key": key, "supported": false, "error": format!("{e:#}")}),
    }
}

fn info_counts(info: &str) -> Option<(u64, u64, u64)> {
    let mut f = info.split("; ");
    let mut next = |name: &str| -> Option<u64> {
        f.next()?
            .strip_prefix(name)?
            .strip_prefix(": ")?
            .parse()
            .ok()
    };
    Some((next("processed")?, next("failed")?, next("total")?))
}

async fn send(target: &str, values: &Value, timeout: Duration) -> Value {
    let data: Vec<Value> = values
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| {
                    let value = match &v["value"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let mut item = json!({"host": v["host"], "key": v["key"], "value": value});
                    if v["clock"].is_u64() {
                        item["clock"] = v["clock"].clone();
                    }
                    item
                })
                .collect()
        })
        .unwrap_or_default();
    let request = json!({"request": "sender data", "data": data}).to_string();
    let answer: Result<Value> = async {
        let body = exchange(target, request.as_bytes(), timeout).await?;
        serde_json::from_slice(&body).context("the trapper's answer is not JSON")
    }
    .await;
    match answer {
        Ok(a) => {
            let info = a["info"].as_str().unwrap_or_default().to_string();
            let mut out = json!({"response": a["response"], "info": info});
            if let Some((p, f, t)) = info_counts(&info) {
                out["processed"] = json!(p);
                out["failed"] = json!(f);
                out["total"] = json!(t);
            }
            out
        }
        Err(e) => json!({"response": "error", "error": format!("{e:#}")}),
    }
}

async fn session(
    ctx: &ConnectContext,
    target: &str,
    timeout: Duration,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) {
    let log = Log::new(Some(&ctx.status_tx));
    loop {
        let (action, depth, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return,
            },
            action = internal.recv() => match action {
                Some((a, depth)) => (a, depth, None),
                None => return,
            },
        };
        let reply = |injected: &mut Option<ClientCommand>, outcome: ClientSendOutcome| {
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(command, Ok(outcome));
            }
        };
        match ZabbixClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return;
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("Zabbix client action refused: {e}"));
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
                "Zabbix client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Zabbix",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        let (event, data) = match action["type"].as_str().unwrap_or_default() {
            actions::GET => (
                &*actions::VALUE_EVENT,
                get(target, action["key"].as_str().unwrap_or_default(), timeout).await,
            ),
            _ => (
                &*actions::SENT_EVENT,
                send(target, &action["values"], timeout).await,
            ),
        };
        reply(
            &mut injected,
            ClientSendOutcome::Executed {
                detail: data.to_string(),
            },
        );
        if events.try_send((Event::new(event, data), depth)).is_err() {
            log.warn("Zabbix client: event queue full; consumer stalled".to_string());
            return;
        }
    }
}
