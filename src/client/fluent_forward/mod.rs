//! One TCP owner; commands stay available while any event handler is parked.
pub mod actions;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::fluent_forward::codec;
use crate::state::{client_handles::ClientSendOutcome, AccessLogOwner, ClientStatus};
use crate::{console_error, console_info};
pub use actions::FluentForwardClientProtocol;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_PENDING_ACKS: usize = 32;
pub const MAX_QUEUED_EVENTS: usize = 32;
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
pub const MAX_HANDLER_ACTIONS: usize = 32;
type Handler = Pin<Box<dyn Future<Output = Result<crate::llm::ClientLlmResult>> + Send>>;
struct Pending {
    tag: String,
    count: usize,
    deadline: tokio::time::Instant,
    depth: usize,
}
pub struct FluentForwardClient;
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
                    "FluentForward",
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
            &FluentForwardClientProtocol::new(),
            &ctx.status_tx,
        )
        .await
    })
}
impl FluentForwardClient {
    pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        let socket = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
            .await
            .context("Forward connect deadline exceeded")??;
        let peer = socket.peer_addr()?;
        let local = socket.local_addr()?;
        let (mut reader, mut writer) = socket.into_split();
        let mut commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        let registrar = ctx.state.clone();
        let client_id = ctx.client_id;
        console_info!(ctx.status_tx, "Forward emitter connected to {}", peer);
        let task = tokio::spawn(async move {
            let protocol = FluentForwardClientProtocol::new();
            let mut decoder = codec::Decoder::default();
            let mut buffer = [0; 8192];
            let mut pending = HashMap::<String, Pending>::new();
            let mut events = VecDeque::new();
            let connected = Event::new(
                &actions::FORWARD_CONNECTED_EVENT,
                json!({"remote_addr":peer.to_string(),"local_addr":local.to_string()}),
            );
            let mut current = Some((handler(ctx.clone(), connected), 0));
            let mut expiry = tokio::time::interval(Duration::from_millis(100));
            'session: loop {
                if current.is_none() {
                    if let Some((event, depth)) = events.pop_front() {
                        current = Some((handler(ctx.clone(), event), depth));
                    }
                }
                tokio::select! {
                    read = reader.read(&mut buffer) => {
                        let n = match read {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(error) => {
                                console_error!(ctx.status_tx, "Forward read failed: {}", error);
                                break;
                            }
                        };
                        if let Err(error) = decoder.feed(&buffer[..n]) {
                            console_error!(ctx.status_tx, "Forward ACK framing failed: {}", error);
                            break;
                        }
                        loop {
                            let node = match decoder.next_node() {
                                Ok(Some(node)) => node,
                                Ok(None) => break,
                                Err(error) => {
                                    console_error!(ctx.status_tx, "Forward ACK invalid: {}", error);
                                    break 'session;
                                }
                            };
                            let token = match codec::parse_ack(node) {
                                Ok(token) => token,
                                Err(error) => {
                                    console_error!(ctx.status_tx, "Forward ACK invalid or unsupported authentication greeting: {}", error);
                                    break 'session;
                                }
                            };
                            let Some(p) = pending.remove(&token) else {
                                console_error!(ctx.status_tx, "Forward unmatched/repeated ACK; closing");
                                break 'session;
                            };
                            if p.deadline <= tokio::time::Instant::now() {
                                Self::record_ack_timeout(&ctx, pending.len() + 1).await;
                                break 'session;
                            }
                            if events.len() == MAX_QUEUED_EVENTS {
                                console_error!(ctx.status_tx, "Forward response-event queue limit; closing");
                                break 'session;
                            }
                            events.push_back((Event::new(&actions::FORWARD_ACK_EVENT,
                                json!({"tag":p.tag,"record_count":p.count})), p.depth));
                        }
                    }
                    result = async { current.as_mut().unwrap().0.as_mut().await }, if current.is_some() => {
                        let (_, depth) = current.take().unwrap();
                        match result {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates {
                                    ctx.state.set_memory_for_client(client_id, memory).await;
                                }
                                if result.actions.len() > MAX_HANDLER_ACTIONS {
                                    console_error!(ctx.status_tx, "Forward handler action count limit");
                                    break;
                                }
                                for action in result.actions {
                                    match Self::apply(&mut writer, &protocol, &mut pending, depth + 1, action).await {
                                        Ok(ClientSendOutcome::Disconnected) => break 'session,
                                        Ok(ClientSendOutcome::Rejected { error }) => {
                                            console_error!(ctx.status_tx, "Forward action rejected: {}", error);
                                        }
                                        Ok(_) => {}
                                        Err(error) => {
                                            console_error!(ctx.status_tx, "Forward send failed: {}", error);
                                            break 'session;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                console_error!(ctx.status_tx, "Forward event handler failed: {}", error);
                            }
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break; };
                        let action = command.action.clone();
                        let result = Self::apply(&mut writer, &protocol, &mut pending, 0, action.clone()).await;
                        let stop = result.is_err() || matches!(result, Ok(ClientSendOutcome::Disconnected));
                        let response = match &result {
                            Ok(outcome) => serde_json::to_value(outcome).unwrap_or(Value::Null),
                            Err(error) => json!({"error":error.to_string()}),
                        };
                        ctx.state.record_access_log(AccessLogOwner::Client(client_id.as_u32()),
                            "FluentForward", None, "injected_action", action, vec![response]).await;
                        crate::client::command_support::reply(command, result);
                        if stop { break; }
                    }
                    _ = expiry.tick() => {
                        if pending.values().any(|p| p.deadline <= tokio::time::Instant::now()) {
                            Self::record_ack_timeout(&ctx, pending.len()).await;
                            break;
                        }
                    }
                }
                let _ = ctx.status_tx.send("__UPDATE_UI__".into());
            }
            ctx.state.remove_client_handle(client_id).await;
            ctx.state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
            let _ = ctx.status_tx.send("__UPDATE_UI__".into());
        });
        registrar.register_client_task(client_id, task).await;
        Ok(local)
    }
    async fn record_ack_timeout(ctx: &ConnectContext, pending_count: usize) {
        ctx.state
            .record_access_log(
                AccessLogOwner::Client(ctx.client_id.as_u32()),
                "FluentForward",
                None,
                "forward_ack_timeout",
                json!({"pending_count":pending_count}),
                vec![],
            )
            .await;
        console_error!(
            ctx.status_tx,
            "Forward ACK deadline exceeded; closing without retry"
        );
    }
    async fn apply(
        writer: &mut tokio::net::tcp::OwnedWriteHalf,
        protocol: &FluentForwardClientProtocol,
        pending: &mut HashMap<String, Pending>,
        depth: usize,
        action: Value,
    ) -> Result<ClientSendOutcome> {
        let result = match protocol.execute_action(action) {
            Ok(r) => r,
            Err(error) => {
                return Ok(ClientSendOutcome::Rejected {
                    error: error.to_string(),
                })
            }
        };
        match result {
            ClientActionResult::Disconnect => Ok(ClientSendOutcome::Disconnected),
            ClientActionResult::Custom { name, data } if name == "send_forward_batch" => {
                let batch: codec::Batch = serde_json::from_value(data)?;
                if batch.require_ack
                    && (pending.len() == MAX_PENDING_ACKS || depth > MAX_FOLLOWUP_DEPTH)
                {
                    return Ok(ClientSendOutcome::Rejected {
                        error: "Forward pending ACK/follow-up depth limit".into(),
                    });
                }
                let chunk = batch
                    .require_ack
                    .then(|| STANDARD.encode(rand::random::<[u8; 16]>()));
                let bytes = match codec::encode_batch(&batch, chunk.as_deref()) {
                    Ok(b) => b,
                    Err(error) => {
                        return Ok(ClientSendOutcome::Rejected {
                            error: error.to_string(),
                        })
                    }
                };
                tokio::time::timeout(IO_TIMEOUT,writer.write_all(&bytes)).await.context("Forward write deadline exceeded; stream closed after possible partial write")??;
                if let Some(chunk) = chunk {
                    pending.insert(
                        chunk,
                        Pending {
                            tag: batch.tag,
                            count: batch.entries.len(),
                            deadline: tokio::time::Instant::now() + IO_TIMEOUT,
                            depth,
                        },
                    );
                }
                Ok(ClientSendOutcome::Sent {
                    bytes_sent: bytes.len(),
                })
            }
            _ => anyhow::bail!("unsupported Forward action result"),
        }
    }
}
