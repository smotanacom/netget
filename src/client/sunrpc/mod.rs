//! ONC RPC portmapper/rpcbind client: one TCP connection, one call at a time, each reply
//! matched by xid and raised to the handler as `sunrpc_reply`.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::sunrpc::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::SunRpcClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

/// Connect, and each call to its reply.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;

pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let stream = tokio::time::timeout(IO_TIMEOUT, tokio::net::TcpStream::connect(&ctx.remote_addr))
        .await
        .context("connect timed out")??;
    let local = stream.local_addr()?;
    let remote = stream.peer_addr()?;
    let (r, w) = stream.into_split();
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
            json!({"remote_addr": remote.to_string()}),
        ),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "SunRPC",
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
                &SunRpcClientProtocol,
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
                    .warn(format!("SunRPC client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, r, w, external, internal_rx, event_tx).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("SunRPC client ended: {e}"));
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

/// One call to its reply: the result body, or the reason there is none.
async fn exchange(
    r: &mut OwnedReadHalf,
    w: &mut OwnedWriteHalf,
    xid: u32,
    version: u32,
    procedure: u32,
    args: &[u8],
) -> Result<std::result::Result<Vec<u8>, String>> {
    tokio::time::timeout(IO_TIMEOUT, async {
        w.write_all(&wire::record(&wire::call_message(
            xid, version, procedure, args,
        )))
        .await?;
        loop {
            let msg = wire::read_record(r)
                .await?
                .context("the portmapper closed the connection")?;
            let (got, body) = wire::parse_reply(&msg)?;
            // A stale reply to an earlier, timed-out call is skipped.
            if got == xid {
                return Ok(body);
            }
        }
    })
    .await
    .context("the portmapper did not answer within 10 s")?
}

async fn session(
    ctx: &ConnectContext,
    mut r: OwnedReadHalf,
    mut w: OwnedWriteHalf,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) -> Result<()> {
    let log = Log::new(Some(&ctx.status_tx));
    let mut xid: u32 = rand::random();
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
        match SunRpcClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("SunRPC client action refused: {e:#}"));
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
                "SunRPC client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "SunRPC",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        let (version, procedure, args) = actions::call(&action)?;
        xid = xid.wrapping_add(1);
        let operation = action["type"].as_str().unwrap_or_default();
        let answer = exchange(&mut r, &mut w, xid, version, procedure, &args).await;
        let data = match answer {
            Err(e) => {
                // The transport failed: tell the injector, end the session.
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: format!("{e:#}"),
                    },
                );
                return Err(e);
            }
            Ok(Err(why)) => {
                json!({"operation": operation, "ok": false, "result": null, "error": why})
            }
            Ok(Ok(body)) => match actions::result(&action, version, &body) {
                Ok(result) => {
                    let ok = !matches!(operation, "sunrpc_set" | "sunrpc_unset") || result == true;
                    json!({"operation": operation, "ok": ok, "result": result})
                }
                Err(e) => json!({"operation": operation, "ok": false, "result": null,
                                 "error": format!("unreadable result: {e:#}")}),
            },
        };
        reply(
            &mut injected,
            ClientSendOutcome::Executed {
                detail: data.to_string(),
            },
        );
        ensure!(
            events
                .try_send((Event::new(&actions::REPLY_EVENT, data), depth))
                .is_ok(),
            "SunRPC event queue full; consumer stalled"
        );
    }
}
