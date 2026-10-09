//! LPD (RFC 1179) client. LPD carries one command per connection, so `connect` only checks
//! that the server is reachable and every action dials a fresh connection.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::lpd::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use actions::Command;
pub use actions::LpdClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

#[derive(Clone)]
struct Identity {
    host: String,
    user: String,
}

fn identity(ctx: &ConnectContext) -> Result<Identity> {
    let params = ctx.startup_params.as_ref();
    let host = params
        .map(|p| p.get_optional_string("host"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_HOST.to_string());
    let user = params
        .map(|p| p.get_optional_string("user"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_USER.to_string());
    anyhow::ensure!(
        wire::valid_token(&host) && host.len() <= 31,
        "host must be at most 31 printable characters without spaces"
    );
    anyhow::ensure!(
        wire::valid_token(&user) && user.len() <= 31,
        "user must be at most 31 printable characters without spaces"
    );
    Ok(Identity { host, user })
}

async fn dial(remote: &str) -> Result<TcpStream> {
    tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(remote))
        .await
        .context("LPD connect deadline")?
        .context("LPD connect")
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let who = identity(&ctx)?;
    let probe = dial(&ctx.remote_addr).await?;
    let local = probe.local_addr()?;
    drop(probe);
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
        let protocol = LpdClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("LPD client handler: {e}"))
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
        let result = session(&session_ctx, &who, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("LPD client ended: {e}"));
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

async fn send(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    tokio::time::timeout(wire::IO_TIMEOUT, stream.write_all(bytes))
        .await
        .context("LPD write deadline")??;
    Ok(())
}

/// One acknowledgement byte; EOF counts as a refusal.
async fn ack(stream: &mut TcpStream) -> Result<u8> {
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(wire::IO_TIMEOUT, stream.read(&mut byte))
        .await
        .context("LPD acknowledgement deadline")??;
    Ok(if read == 0 { 0xff } else { byte[0] })
}

/// Read a listing or removal reply to EOF, bounded.
async fn read_reply(stream: &mut TcpStream) -> Result<String> {
    let mut out = Vec::new();
    let mut limited = stream.take(wire::MAX_REPLY_BYTES as u64 + 1);
    tokio::time::timeout(wire::IO_TIMEOUT, limited.read_to_end(&mut out))
        .await
        .context("LPD reply deadline")??;
    anyhow::ensure!(
        out.len() <= wire::MAX_REPLY_BYTES,
        "LPD reply exceeds {} bytes",
        wire::MAX_REPLY_BYTES
    );
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn job_number() -> u16 {
    (uuid::Uuid::new_v4().as_u128() % 1000) as u16
}

fn answer(injected: &mut Option<ClientCommand>, sent: usize) {
    if let Some(command) = injected.take() {
        crate::client::command_support::reply(
            command,
            Ok(ClientSendOutcome::Sent { bytes_sent: sent }),
        );
    }
}

/// Run one command on its own connection; `injected` is answered once its bytes are sent.
async fn run(
    remote: &str,
    who: &Identity,
    command: Command,
    injected: &mut Option<ClientCommand>,
) -> Result<Event> {
    let mut stream = dial(remote).await?;
    let mut sent = 0usize;
    match command {
        Command::Print {
            queue,
            text,
            job_name,
            format,
            data_first,
        } => {
            let number = format!("{:03}", job_number());
            let data_name = format!("dfA{number}{}", who.host);
            let control_name = format!("cfA{number}{}", who.host);
            let mut control = format!("H{}\nP{}\n", who.host, who.user);
            if let Some(name) = &job_name {
                control.push_str(&format!("J{name}\n"));
            }
            control.push_str(&format!(
                "L{}\nN{}\n{format}{data_name}\nU{data_name}\n",
                who.user,
                job_name.as_deref().unwrap_or("(stdin)")
            ));
            let line = format!("\x02{queue}\n");
            sent += line.len();
            send(&mut stream, line.as_bytes()).await?;
            let mut result =
                json!({"queue": queue, "job_id": number, "accepted": false, "refused_at": "queue"});
            if ack(&mut stream).await? != 0 {
                answer(injected, sent);
                return Ok(Event::new(&actions::PRINT_RESULT_EVENT, result));
            }
            let files: [(u8, &str, &[u8]); 2] = [
                (wire::SUB_CONTROL_FILE, &control_name, control.as_bytes()),
                (wire::SUB_DATA_FILE, &data_name, text.as_bytes()),
            ];
            let order: Vec<usize> = if data_first { vec![1, 0] } else { vec![0, 1] };
            for (step, index) in order.into_iter().enumerate() {
                let (code, name, body) = files[index];
                let stage = if code == wire::SUB_CONTROL_FILE {
                    "control"
                } else {
                    "data"
                };
                let header = format!("{}{} {name}\n", code as char, body.len());
                sent += header.len();
                send(&mut stream, header.as_bytes()).await?;
                if ack(&mut stream).await? != 0 {
                    result["refused_at"] = json!(stage);
                    answer(injected, sent);
                    return Ok(Event::new(&actions::PRINT_RESULT_EVENT, result));
                }
                let mut payload = body.to_vec();
                payload.push(0);
                sent += payload.len();
                send(&mut stream, &payload).await?;
                if step == 1 {
                    answer(injected, sent);
                }
                if ack(&mut stream).await? != 0 {
                    result["refused_at"] = json!(if step == 1 { "final" } else { stage });
                    answer(injected, sent);
                    return Ok(Event::new(&actions::PRINT_RESULT_EVENT, result));
                }
            }
            result["accepted"] = json!(true);
            result["refused_at"] = Value::Null;
            let _ = stream.shutdown().await;
            Ok(Event::new(&actions::PRINT_RESULT_EVENT, result))
        }
        Command::Queue { queue, long, list } => {
            let code = if long {
                wire::CMD_QUEUE_LONG
            } else {
                wire::CMD_QUEUE_SHORT
            };
            let mut line = format!("{}{queue}", code as char);
            for item in &list {
                line.push(' ');
                line.push_str(item);
            }
            line.push('\n');
            sent += line.len();
            send(&mut stream, line.as_bytes()).await?;
            answer(injected, sent);
            let text = read_reply(&mut stream).await?;
            Ok(Event::new(
                &actions::REPLY_EVENT,
                json!({"command": "queue", "queue": queue, "text": text}),
            ))
        }
        Command::Remove { queue, agent, jobs } => {
            let mut line = format!("{}{queue} {agent}", wire::CMD_REMOVE_JOBS as char);
            for job in &jobs {
                line.push(' ');
                line.push_str(job);
            }
            line.push('\n');
            sent += line.len();
            send(&mut stream, line.as_bytes()).await?;
            answer(injected, sent);
            let text = read_reply(&mut stream).await?;
            Ok(Event::new(
                &actions::REPLY_EVENT,
                json!({"command": "remove", "queue": queue, "text": text}),
            ))
        }
        Command::Start { queue } => {
            let line = format!("{}{queue}\n", wire::CMD_PRINT_WAITING as char);
            sent += line.len();
            send(&mut stream, line.as_bytes()).await?;
            answer(injected, sent);
            let _ = stream.shutdown().await;
            Ok(Event::new(
                &actions::REPLY_EVENT,
                json!({"command": "start_queue", "queue": queue, "text": ""}),
            ))
        }
    }
}

async fn session(
    ctx: &ConnectContext,
    who: &Identity,
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
        let command = match LpdClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => Command::from_action(&action),
            Err(e) => Err(e),
        };
        let command = match command {
            Ok(command) => command,
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
                    "LPD",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "queue": action["queue"]}),
                    vec![],
                )
                .await;
        }
        // A server that refuses or drops one connection does not end the client: the failure
        // is logged and the next command dials again.
        match run(&ctx.remote_addr, who, command, &mut injected).await {
            Ok(event) => events
                .try_send(event)
                .context("LPD event queue full; consumer stalled")?,
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Err(anyhow::anyhow!(e.to_string())),
                    );
                }
                Log::new(Some(&ctx.status_tx)).warn(format!("LPD command failed: {e:#}"));
            }
        }
    }
}
