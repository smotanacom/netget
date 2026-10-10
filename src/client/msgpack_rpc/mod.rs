//! MessagePack-RPC client: one connection, pipelined calls matched by msgid, notifications in
//! both directions.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::msgpack_rpc::wire::{self, Rpc};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::MsgpackRpcClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::{
    io::{AsyncWriteExt, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};

/// Calls awaiting a response at once.
pub const MAX_PENDING: usize = 64;
/// The connection may stay quiet indefinitely between messages.
const READ_IDLE: Duration = Duration::from_secs(365 * 24 * 3600);

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("MessagePack-RPC connect deadline")??;
    let local = stream.local_addr()?;
    let (reader, writer) = tokio::io::split(stream);
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (msg_tx, msg_rx) = mpsc::channel::<Result<Value>>(64);
    let reader_task = tokio::spawn(async move {
        let mut stream = wire::Stream::new(reader);
        loop {
            let item = stream.next(READ_IDLE).await;
            let end = !matches!(item, Ok(Some(_)));
            let sent = match item {
                Ok(Some(v)) => msg_tx.send(Ok(v)).await,
                Ok(None) => {
                    msg_tx
                        .send(Err(anyhow::anyhow!("server closed the connection")))
                        .await
                }
                Err(e) => msg_tx.send(Err(e)).await,
            };
            if end || sent.is_err() {
                return;
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(64);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"remote_addr": ctx.remote_addr}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = MsgpackRpcClientProtocol;
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
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("MessagePack-RPC client handler: {e}")),
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
            writer,
            msg_rx,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx))
                    .warn(format!("MessagePack-RPC client ended: {e}"));
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

async fn session(
    ctx: &ConnectContext,
    mut writer: WriteHalf<TcpStream>,
    mut messages: mpsc::Receiver<Result<Value>>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut next_id: u64 = 0;
    let mut pending: HashMap<u64, (String, Option<ClientCommand>)> = HashMap::new();
    loop {
        let (action, mut injected) = tokio::select! {
            item = messages.recv() => {
                let Some(item) = item else { return Ok(()) };
                match wire::parse(item?)? {
                    Rpc::Response { msgid, error, result } => {
                        // A response to no call of ours is dropped.
                        if let Some((method, caller)) = pending.remove(&msgid) {
                            let event = json!({"method": method, "msgid": msgid, "result": result, "error": error});
                            if let Some(command) = caller {
                                crate::client::command_support::reply(command, Ok(ClientSendOutcome::Executed { detail: event.to_string() }));
                            }
                            events.try_send(Event::new(&actions::RESPONSE_EVENT, event)).context("MessagePack-RPC event queue full; consumer stalled")?;
                        }
                    }
                    Rpc::Notification { method, params } => {
                        events.try_send(Event::new(&actions::NOTIFICATION_EVENT, json!({"method": method, "params": params})))
                            .context("MessagePack-RPC event queue full; consumer stalled")?;
                    }
                    Rpc::Request { msgid, method, .. } => {
                        Log::new(Some(&ctx.status_tx)).warn(format!("MessagePack-RPC server called {method}; this client serves no methods"));
                        writer.write_all(&wire::response(msgid, &json!("NetGet's client serves no methods"), &Value::Null)?).await?;
                    }
                }
                continue;
            }
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        let refuse = |injected: &mut Option<ClientCommand>, error: String| {
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(
                    command,
                    Ok(ClientSendOutcome::Rejected { error }),
                );
            }
        };
        match MsgpackRpcClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                refuse(&mut injected, e.to_string());
                continue;
            }
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "MessagePack-RPC",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        let method = action["method"].as_str().unwrap_or_default().to_string();
        let bytes = if action["type"] == "msgpack_call" {
            if pending.len() >= MAX_PENDING {
                refuse(&mut injected, "too many calls awaiting responses".into());
                continue;
            }
            next_id = (next_id + 1) % (u64::from(u32::MAX) + 1);
            wire::request(next_id, &method, &action["params"])?
        } else {
            wire::notification(&method, &action["params"])?
        };
        tokio::time::timeout(wire::IO_TIMEOUT, writer.write_all(&bytes))
            .await
            .context("MessagePack-RPC write deadline")??;
        if action["type"] == "msgpack_call" {
            pending.insert(next_id, (method, injected.take()));
        } else if let Some(command) = injected.take() {
            crate::client::command_support::reply(
                command,
                Ok(ClientSendOutcome::Sent {
                    bytes_sent: bytes.len(),
                }),
            );
        }
    }
}
