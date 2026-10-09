pub mod actions;
pub mod transport;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::tacacs::codec;
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
use crate::{console_error, console_info};
pub use actions::TacacsClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, future::Future, pin::Pin, sync::Arc, time::Duration};
pub const MAX_QUEUED_EVENTS: usize = 32;
pub const MAX_HANDLER_ACTIONS: usize = 32;
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
type Handler = Pin<Box<dyn Future<Output = Result<crate::llm::ClientLlmResult>> + Send>>;
type Exchange = Pin<Box<dyn Future<Output = Result<transport::Response>> + Send>>;
struct Inflight {
    exchange: Exchange,
    command: Option<ClientCommand>,
    depth: usize,
}
fn response_event(name: &str) -> &'static crate::protocol::EventType {
    match name {
        "tacacs_authentication_result" => &actions::AUTH_EVENT,
        "tacacs_authorization_result" => &actions::AUTHOR_EVENT,
        "tacacs_accounting_result" => &actions::ACCOUNT_EVENT,
        _ => &actions::ERROR_EVENT,
    }
}
fn handler(ctx: ConnectContext, event: Event, timeout: Duration) -> Handler {
    Box::pin(async move {
        let instruction = ctx
            .state
            .get_instruction_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        let configured = ctx
            .state
            .get_client_event_handler_config(ctx.client_id)
            .await
            .is_some_and(|c| c.find_handler(event.id()).is_some());
        if instruction.is_empty() && !configured {
            let event_id = event.id().to_owned();
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "TACACS",
                    None,
                    &event_id,
                    event.data,
                    vec![],
                )
                .await;
            return Ok(crate::llm::ClientLlmResult {
                actions: vec![],
                memory_updates: None,
            });
        }
        let memory = ctx
            .state
            .get_memory_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        tokio::time::timeout(
            timeout,
            crate::client::llm_budget::call_llm_for_client(
                &ctx.llm_client,
                &ctx.state,
                ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &TacacsClientProtocol,
                &ctx.status_tx,
            ),
        )
        .await
        .context("TACACS handler deadline")?
    })
}
pub struct TacacsClient;
impl TacacsClient {
    pub async fn connect(mut ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        ensure!(
            ctx.remote_addr.len() <= 1024 && !ctx.remote_addr.contains(['\r', '\n', '@', '/']),
            "TACACS endpoint host:port only"
        );
        let params = ctx
            .startup_params
            .as_ref()
            .context("TACACS shared_secret required")?;
        let secret = params.get_string("shared_secret")?;
        codec::validate_secret(&secret)?;
        let io_timeout = crate::server::tacacs::timeout(
            params.get_optional_u64("io_timeout_seconds")?,
            codec::DEFAULT_IO_SECONDS,
        )?;
        let handler_timeout = crate::server::tacacs::timeout(
            params.get_optional_u64("handler_timeout_seconds")?,
            codec::DEFAULT_HANDLER_SECONDS,
        )?;
        ensure!(
            tokio::time::timeout(io_timeout, tokio::net::lookup_host(&ctx.remote_addr))
                .await
                .context("TACACS DNS deadline")??
                .next()
                .is_some(),
            "TACACS endpoint resolution empty"
        );
        let settings = Arc::new(transport::Settings {
            remote_addr: ctx.remote_addr.clone(),
            shared_secret: secret.into_bytes(),
            io_timeout,
        });
        ctx.startup_params = None;
        let commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        let registrar = ctx.state.clone();
        let id = ctx.client_id;
        console_info!(
            ctx.status_tx,
            "TACACS legacy connector ready for {}",
            ctx.remote_addr
        );
        registrar
            .spawn_client_task(id, run(ctx, settings, handler_timeout, commands))
            .await;
        Ok("0.0.0.0:0".parse().unwrap())
    }
}
async fn run(
    ctx: ConnectContext,
    settings: Arc<transport::Settings>,
    handler_timeout: Duration,
    mut commands: tokio::sync::mpsc::Receiver<ClientCommand>,
) {
    let id = ctx.client_id;
    let connected = Event::new(
        &actions::CONNECTED_EVENT,
        json!({"remote_addr":ctx.remote_addr}),
    );
    let mut current = Some((handler(ctx.clone(), connected, handler_timeout), 0));
    let mut events = VecDeque::new();
    let mut queued = VecDeque::new();
    let mut pending: Option<Inflight> = None;
    'session: loop {
        if current.is_none() {
            if let Some((event, depth)) = events.pop_front() {
                current = Some((handler(ctx.clone(), event, handler_timeout), depth));
            }
        }
        if pending.is_none() {
            if let Some((action, depth)) = queued.pop_front() {
                match prepare(action, depth, settings.clone(), ctx.clone()) {
                    Ok(Prepared::Exchange(exchange)) => {
                        pending = Some(Inflight {
                            exchange,
                            command: None,
                            depth,
                        })
                    }
                    Ok(Prepared::Disconnect) => break,
                    Err(_) => console_error!(
                        ctx.status_tx,
                        "TACACS handler action refused decision=fail_closed_action"
                    ),
                };
                continue;
            }
        }
        tokio::select! {
            result = async { pending.as_mut().unwrap().exchange.as_mut().await }, if pending.is_some() => {
                let work = pending.take().unwrap();
                let event = match result {
                    Ok(response) => {
                        if let Some(command) = work.command {
                            finish(&ctx, command, Ok(ClientSendOutcome::Executed {detail:response.event.into()})).await;
                        }
                        Event::new(response_event(response.event), response.data)
                    }
                    Err(_) => {
                        if let Some(command) = work.command {
                            finish(&ctx, command, Err(anyhow::anyhow!("TACACS session failed; no automatic replay"))).await;
                        }
                        Event::new(&actions::ERROR_EVENT, json!({"error":"TACACS transport, framing, correlation or flow failure; no replay"}))
                    }
                };
                if events.len() >= MAX_QUEUED_EVENTS {
                    console_error!(ctx.status_tx, "TACACS event capacity decision=fail_closed_event_capacity");
                    break;
                }
                events.push_back((event, work.depth));
            }
            result = async { current.as_mut().unwrap().0.as_mut().await }, if current.is_some() => {
                let (_, depth) = current.take().unwrap();
                match result {
                    Ok(result) => {
                        if result.actions.len() > MAX_HANDLER_ACTIONS
                            || queued.len() + result.actions.len() > MAX_HANDLER_ACTIONS
                            || result.actions.iter().any(|v| !codec::within_json_budget(v))
                        {
                            for value in result.actions { crate::utils::json_budget::drop_iteratively(value); }
                            console_error!(ctx.status_tx, "TACACS handler JSON/action capacity decision=fail_closed_handler_capacity");
                            break;
                        }
                        if result.actions.iter().any(|v| v["type"] == "disconnect") {
                            for value in result.actions { crate::utils::json_budget::drop_iteratively(value); }
                            break;
                        }
                        if let Some(memory) = result.memory_updates { ctx.state.set_memory_for_client(id, memory).await; }
                        queued.extend(result.actions.into_iter().map(|v| (v, depth + 1)));
                    }
                    Err(_) => console_error!(ctx.status_tx, "TACACS handler failed decision=fail_closed_handler"),
                }
            }
            command = commands.recv() => {
                let Some(mut command) = command else { break; };
                if !codec::within_json_budget(&command.action) {
                    crate::utils::json_budget::drop_iteratively(std::mem::take(&mut command.action));
                    finish(&ctx, command, Ok(ClientSendOutcome::Rejected {error:"TACACS JSON depth/node/retained-content budget".into()})).await;
                    continue;
                }
                match prepare(command.action.clone(), 0, settings.clone(), ctx.clone()) {
                    Ok(Prepared::Disconnect) => {
                        finish(&ctx, command, Ok(ClientSendOutcome::Disconnected)).await;
                        break 'session;
                    }
                    Ok(Prepared::Exchange(exchange)) if pending.is_none() => {
                        pending = Some(Inflight {exchange, command:Some(command), depth:0});
                    }
                    Ok(Prepared::Exchange(_)) => finish(&ctx, command, Ok(ClientSendOutcome::Rejected {error:"One TACACS operation is already in flight".into()})).await,
                    Err(_) => finish(&ctx, command, Ok(ClientSendOutcome::Rejected {error:"TACACS action invalid or over limit".into()})).await,
                }
            }
        }
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
    if let Some(work) = pending.take() {
        if let Some(command) = work.command {
            finish(
                &ctx,
                command,
                Err(anyhow::anyhow!("TACACS session cancelled")),
            )
            .await;
        }
    }
    drop(current.take());
    events.clear();
    for (value, _) in queued {
        crate::utils::json_budget::drop_iteratively(value);
    }
    ctx.state.remove_client_handle(id).await;
    ctx.state
        .update_client_status(id, ClientStatus::Disconnected)
        .await;
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}
enum Prepared {
    Disconnect,
    Exchange(Exchange),
}
fn prepare(
    action: Value,
    depth: usize,
    settings: Arc<transport::Settings>,
    ctx: ConnectContext,
) -> Result<Prepared> {
    ensure!(depth <= MAX_FOLLOWUP_DEPTH, "TACACS follow-up depth");
    match TacacsClientProtocol.execute_action(action)? {
        ClientActionResult::Disconnect => Ok(Prepared::Disconnect),
        ClientActionResult::Custom { name, data } if name == "tacacs_command" => {
            Ok(Prepared::Exchange(Box::pin(transport::exchange(
                settings,
                transport::parse(data)?,
                ctx,
            ))))
        }
        _ => anyhow::bail!("Unsupported TACACS action result"),
    }
}
async fn finish(ctx: &ConnectContext, command: ClientCommand, result: Result<ClientSendOutcome>) {
    let outcome = match &result {
        Ok(v) => serde_json::to_value(v).unwrap_or(Value::Null),
        Err(_) => json!({"error":"TACACS session failed or cancelled"}),
    };
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.client_id.as_u32()),
            "TACACS",
            None,
            "injected_action",
            crate::utils::redact::redact_sensitive(&command.action),
            vec![outcome],
        )
        .await;
    crate::client::command_support::reply(command, result);
}
