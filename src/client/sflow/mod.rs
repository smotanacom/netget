pub mod actions;
pub mod transport;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::sflow::codec::Batch;
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
use crate::{console_error, console_info};
pub use actions::SflowClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, future::Future, pin::Pin, sync::Arc};

pub const MAX_QUEUED_EVENTS: usize = 32;
pub const MAX_HANDLER_ACTIONS: usize = 32;
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
type Handler = Pin<Box<dyn Future<Output = Result<crate::llm::ClientLlmResult>> + Send>>;
type SendFuture = Pin<Box<dyn Future<Output = Result<usize>> + Send>>;
struct Inflight {
    send: SendFuture,
    prepared: transport::Prepared,
    command: Option<ClientCommand>,
    depth: usize,
}
fn handler(ctx: ConnectContext, event: Event) -> Handler {
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
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "sFlow",
                    None,
                    event.id(),
                    event.data.clone(),
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
        crate::client::llm_budget::call_llm_for_client(
            &ctx.llm_client,
            &ctx.state,
            ctx.client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &SflowClientProtocol::new(),
            &ctx.status_tx,
        )
        .await
    })
}
pub struct SflowClient;
impl SflowClient {
    pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        anyhow::ensure!(
            ctx.remote_addr.len() <= 1024,
            "sFlow destination length bound"
        );
        let peer = tokio::time::timeout(
            transport::IO_TIMEOUT,
            tokio::net::lookup_host(&ctx.remote_addr),
        )
        .await
        .context("sFlow resolve deadline")??
        .next()
        .context("collector resolved to no address")?;
        let socket = Arc::new(
            tokio::net::UdpSocket::bind(if peer.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            })
            .await?,
        );
        socket.connect(peer).await?;
        let local = socket.local_addr()?;
        let mut commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        let owner = ctx.state.clone();
        let id = ctx.client_id;
        console_info!(
            ctx.status_tx,
            "sFlow UDP exporter {} ready for {}",
            id,
            peer
        );
        owner.spawn_client_task(id, async move {
            let protocol = SflowClientProtocol::new();
            let started = tokio::time::Instant::now();
            let mut sequences = transport::Sequences::default();
            let mut current = Some((
                handler(ctx.clone(), Event::new(
                    &actions::SFLOW_CONNECTED_EVENT,
                    json!({"remote_addr":peer.to_string(),"local_addr":local.to_string()})
                )), 0
            ));
            let mut events = VecDeque::new();
            let mut actions = VecDeque::new();
            let mut pending: Option<Inflight> = None;
            let mut incoming = [0u8; 1];
            'session: loop {
                if current.is_none() {
                    if let Some((event, depth)) = events.pop_front() {
                        current = Some((handler(ctx.clone(), event), depth));
                    }
                }
                if pending.is_none() {
                    if let Some((action, depth)) = actions.pop_front() {
                        match prepare(
                            &protocol, action, depth, &sequences, started.elapsed().as_millis() as u32
                        ) {
                            Ok(Prepared::Write(p)) => pending = Some(inflight(socket.clone(), p, None, depth)),
                            Ok(Prepared::Disconnect) => break,
                            Err(e) => console_error!(ctx.status_tx, "sFlow decision=fail_closed_action_error error={}", e),
                        }
                        continue;
                    }
                }
                tokio::select! {
                    result = socket.recv(&mut incoming) => {
                        match result {
                            Ok(_) => console_error!(ctx.status_tx, "sFlow decision=fail_closed_unexpected_reply closing UDP exporter"),
                            Err(e) => console_error!(ctx.status_tx, "sFlow decision=fail_closed_receive_error error={}", e),
                        }
                        break;
                    },
                    result = async {
                        pending.as_mut().unwrap().send.as_mut().await
                    }, if pending.is_some() => {
                        let p = pending.take().unwrap();
                        match result {
                            Ok(byte_count) => {
                                let info = json!({"agent_address":p.prepared.agent,"sub_agent_id":p.prepared.sub_agent,
                                    "sequence_number":p.prepared.sequence,"uptime_ms":p.prepared.uptime,"sample_count":p.prepared.sample_count,
                                    "record_count":p.prepared.records,"byte_count":byte_count,"local_transport_only":true});
                                sequences.commit(p.prepared);
                                if let Some(command) = p.command {
                                    finish(&ctx, command, Ok(ClientSendOutcome::Executed {
                                        detail: "sFlow datagram accepted by local UDP transport".into()
                                    })).await;
                                }
                                console_info!(ctx.status_tx, "sFlow decision=local_export local_transport_only=true byte_count={}", byte_count);
                                if events.len() == MAX_QUEUED_EVENTS {
                                    console_error!(ctx.status_tx, "sFlow decision=fail_closed_event_capacity event_cap={}", MAX_QUEUED_EVENTS);
                                    break;
                                }
                                events.push_back((Event::new(&actions::SFLOW_EXPORTED_EVENT, info), p.depth));
                            },
                            Err(e) => {
                                if let Some(command) = p.command {
                                    finish(&ctx, command, Err(anyhow::anyhow!("sFlow UDP send failed"))).await;
                                }
                                console_error!(ctx.status_tx, "sFlow decision=fail_closed_send_error error={}", e);
                                break;
                            },
                        }
                    },
                    result = async {
                        current.as_mut().unwrap().0.as_mut().await
                    }, if current.is_some() => {
                        let (_, depth) = current.take().unwrap();
                        match result {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates {
                                    ctx.state.set_memory_for_client(id, memory).await;
                                }
                                if result.actions.len() > MAX_HANDLER_ACTIONS
                                    || actions.len() + result.actions.len() > MAX_HANDLER_ACTIONS
                                {
                                    console_error!(ctx.status_tx, "sFlow decision=fail_closed_action_capacity action_cap={}", MAX_HANDLER_ACTIONS);
                                    break;
                                }
                                actions.extend(result.actions.into_iter().map(|a| (a, depth + 1)));
                            },
                            Err(e) => console_error!(ctx.status_tx, "sFlow decision=fail_closed_dispatch_error error={}", e),
                        }
                    },
                    command = commands.recv() => {
                        let Some(command) = command else { break; };
                        match prepare(
                            &protocol, command.action.clone(), 0, &sequences, started.elapsed().as_millis() as u32
                        ) {
                            Ok(Prepared::Disconnect) => {
                                finish(&ctx, command, Ok(ClientSendOutcome::Disconnected)).await;
                                break 'session;
                            },
                            Ok(Prepared::Write(p)) if pending.is_none() => pending = Some(inflight(socket.clone(), p, Some(command), 0)),
                            Ok(Prepared::Write(_)) => finish(&ctx, command, Ok(ClientSendOutcome::Rejected {
                                error: "One UDP send is already in flight".into()
                            })).await,
                            Err(e) => finish(&ctx, command, Ok(ClientSendOutcome::Rejected { error: e.to_string() })).await,
                        }
                    },
                }
                let _ = ctx.status_tx.send("__UPDATE_UI__".into());
            }
            if let Some(p) = pending.take() {
                if let Some(command) = p.command {
                    finish(&ctx, command, Err(anyhow::anyhow!("sFlow UDP send cancelled"))).await;
                }
            }
            current.take();
            events.clear();
            actions.clear();
            ctx.state.remove_client_handle(id).await;
            ctx.state.update_client_status(id, ClientStatus::Disconnected).await;
            let _ = ctx.status_tx.send("__UPDATE_UI__".into());
        }).await;
        Ok(local)
    }
}
enum Prepared {
    Disconnect,
    Write(transport::Prepared),
}
fn prepare(
    protocol: &SflowClientProtocol,
    action: Value,
    depth: usize,
    sequences: &transport::Sequences,
    uptime: u32,
) -> Result<Prepared> {
    match protocol.execute_action(action)? {
        ClientActionResult::Disconnect => Ok(Prepared::Disconnect),
        ClientActionResult::Custom { name, data } if name == "export_sflow_samples" => {
            anyhow::ensure!(depth <= MAX_FOLLOWUP_DEPTH, "sFlow followup depth bound8");
            let batch: Batch = serde_json::from_value(data)?;
            Ok(Prepared::Write(sequences.prepare(&batch, uptime)?))
        }
        _ => anyhow::bail!("unsupported sFlow action result"),
    }
}
fn inflight(
    socket: Arc<tokio::net::UdpSocket>,
    prepared: transport::Prepared,
    command: Option<ClientCommand>,
    depth: usize,
) -> Inflight {
    let bytes = prepared.bytes.clone();
    Inflight {
        send: Box::pin(async move {
            let count = tokio::time::timeout(transport::IO_TIMEOUT, socket.send(&bytes))
                .await
                .context("sFlow UDP write deadline")??;
            anyhow::ensure!(count == bytes.len(), "incomplete UDP datagram send");
            Ok(count)
        }),
        prepared,
        command,
        depth,
    }
}
async fn finish(ctx: &ConnectContext, command: ClientCommand, result: Result<ClientSendOutcome>) {
    let outcome = match &result {
        Ok(outcome) => serde_json::to_value(outcome).unwrap_or(Value::Null),
        Err(e) => json!({"error":e.to_string()}),
    };
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.client_id.as_u32()),
            "sFlow",
            None,
            "injected_action",
            command.action.clone(),
            vec![outcome],
        )
        .await;
    crate::client::command_support::reply(command, result);
}
