//! Steam/Source server query (A2S) client over UDP.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::a2s::wire::{self, Kind, Reassembly};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::A2sClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{net::UdpSocket, sync::mpsc};

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let remote: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await
        .context("resolve the server address")?
        .next()
        .context("the server address resolved to nothing")?;
    let socket = UdpSocket::bind(if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    socket.connect(remote).await?;
    let local = socket.local_addr()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::READY_EVENT,
        json!({"remote_addr": ctx.remote_addr}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = A2sClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("A2S client handler: {e}"))
                }
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &socket, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("A2S client ended: {e}"));
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

/// Read one logical response: a challenge, or a (possibly split) answer.
async fn receive(socket: &UdpSocket) -> Result<(Vec<u8>, usize)> {
    let mut reassembly = Reassembly::default();
    let mut buf = vec![0u8; 4096];
    let mut packets = 0;
    loop {
        let n = tokio::time::timeout(wire::REPLY_TIMEOUT, socket.recv(&mut buf))
            .await
            .context("A2S reply deadline")??;
        packets += 1;
        anyhow::ensure!(
            packets <= wire::MAX_SPLIT_PACKETS,
            "A2S response spans too many packets"
        );
        if let Some(payload) = reassembly.feed(&buf[..n])? {
            return Ok((payload, packets));
        }
    }
}

/// One query, retrying once with the challenge the server hands out.
async fn query(
    socket: &UdpSocket,
    kind: Kind,
    injected: &mut Option<ClientCommand>,
) -> Result<Value> {
    let mut challenge = if kind == Kind::Info {
        None
    } else {
        Some(wire::NO_CHALLENGE)
    };
    for attempt in 0..2 {
        let request = wire::encode_request(kind, challenge);
        socket.send(&request).await?;
        if let Some(command) = injected.take() {
            crate::client::command_support::reply(
                command,
                Ok(ClientSendOutcome::Sent {
                    bytes_sent: request.len(),
                }),
            );
        }
        let (payload, packets) = receive(socket).await?;
        if payload.len() == 9 && payload[4] == wire::RESPONSE_CHALLENGE && attempt == 0 {
            challenge = Some(u32::from_le_bytes(payload[5..9].try_into()?));
            continue;
        }
        let decoded = wire::decode_response(kind, &payload)?;
        let mut event = json!({"query": kind.name(), "packets": packets});
        event[kind.name()] = decoded;
        return Ok(event);
    }
    anyhow::bail!("A2S server asked for a challenge twice")
}

async fn session(
    ctx: &ConnectContext,
    socket: &UdpSocket,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        let kind = match A2sClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => actions::kind(&action),
            Err(e) => Err(e),
        };
        let kind = match kind {
            Ok(kind) => kind,
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        };
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "A2S",
                    None,
                    "injected_action",
                    json!({"query": kind.name()}),
                    vec![],
                )
                .await;
        }
        // A lost datagram or a server that does not answer is logged; the client stays up.
        match query(socket, kind, &mut injected).await {
            Ok(event) => events
                .try_send(Event::new(&actions::RESPONSE_EVENT, event))
                .context("A2S event queue full; consumer stalled")?,
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Err(anyhow::anyhow!(e.to_string())),
                    );
                }
                Log::new(Some(&ctx.status_tx))
                    .warn(format!("A2S {} query failed: {e:#}", kind.name()));
            }
        }
    }
}
