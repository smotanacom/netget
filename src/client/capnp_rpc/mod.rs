//! Cap'n Proto RPC client: bootstraps the server's capability, then calls its methods
//! (pipelined, matched by question id) with parameters typed by the startup schema.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::capnp_rpc::{
    layout::{self, Message, Target},
    rpc::{self, Incoming, Returned},
    schema::Schema,
    send,
};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::CapnpRpcClientProtocol;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};

/// Calls awaiting a Return at once.
pub const MAX_PENDING: usize = 64;
/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// Connecting and bootstrapping must finish within this long.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The connection may stay quiet indefinitely between messages.
const READ_IDLE: Duration = Duration::from_secs(365 * 24 * 3600);

struct Session {
    schema: Schema,
    interface: u64,
    import: u32,
}

async fn bootstrap(
    reader: &mut ReadHalf<TcpStream>,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<u32> {
    send(writer, &rpc::bootstrap(0)?).await?;
    loop {
        let segments = layout::read_message(reader, CONNECT_TIMEOUT)
            .await?
            .context("the server closed the connection during bootstrap")?;
        let msg = Message::new(segments);
        if let Incoming::Return {
            answer: 0,
            returned,
        } = rpc::decode(&msg)?
        {
            let import = match returned {
                Returned::Results {
                    content: Target::Capability(i),
                    caps,
                } => caps
                    .get(i as usize)
                    .copied()
                    .flatten()
                    .context("bootstrap capability missing from the cap table")?,
                Returned::Exception { reason, .. } => bail!("bootstrap refused: {reason}"),
                _ => bail!("bootstrap answered without a capability"),
            };
            send(writer, &rpc::finish(0, false)?).await?;
            return Ok(import);
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx
        .startup_params
        .as_ref()
        .context("schema and interface are required")?;
    let schema = crate::server::capnp_rpc::load_schema(&params.get_string("schema")?).await?;
    let iface = schema.interface(&params.get_string("interface")?)?;
    let (interface, interface_name) = (iface.id, iface.name.clone());
    let methods: Vec<Value> = schema
        .methods_of(interface)
        .iter()
        .map(|(_, _, m)| {
            json!({"name": m.name, "params": schema.describe(m.params), "results": schema.describe(m.results)})
        })
        .collect();
    let (mut reader, writer, local) = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let stream = TcpStream::connect(&ctx.remote_addr).await?;
        let local = stream.local_addr()?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        let import = bootstrap(&mut reader, &mut writer).await?;
        Ok::<_, anyhow::Error>((reader, writer, (local, import)))
    })
    .await
    .context("Cap'n Proto connect and bootstrap deadline")??;
    let (local, import) = local;
    let session = Arc::new(Session {
        schema,
        interface,
        import,
    });
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (msg_tx, msg_rx) = mpsc::channel::<Result<Vec<Vec<u64>>>>(64);
    let reader_task = tokio::spawn(async move {
        loop {
            let item = layout::read_message(&mut reader, READ_IDLE).await;
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
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({"remote_addr": ctx.remote_addr, "interface": interface_name, "methods": methods}),
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
                &CapnpRpcClientProtocol,
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
                    .warn(format!("Cap'n Proto client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = run(
            &session_ctx,
            &session,
            writer,
            msg_rx,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx))
                    .warn(format!("Cap'n Proto client ended: {e:#}"));
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

struct Pending {
    method: String,
    results: u64,
    caller: Option<ClientCommand>,
    depth: usize,
}

/// Encode one call action; returns (bytes to send, method name, results struct id).
fn encode_call(
    session: &Session,
    question: u32,
    action: &Value,
) -> Result<(Vec<u64>, String, u64)> {
    let name = action["method"].as_str().unwrap_or_default();
    let methods = session.schema.methods_of(session.interface);
    let (iface, _, m) = methods
        .iter()
        .find(|(_, _, m)| m.name == name)
        .with_context(|| format!("the interface has no method {name}"))?;
    let (dw, pc) = session.schema.struct_size(m.params)?;
    let words = rpc::call(question, session.import, *iface, m.id, |b, payload| {
        let s = b.init_struct(payload, 0, dw, pc)?;
        session.schema.from_json(b, m.params, s, &action["params"])
    })?;
    Ok((words, m.name.clone(), m.results))
}

async fn run(
    ctx: &ConnectContext,
    session: &Session,
    mut writer: WriteHalf<TcpStream>,
    mut messages: mpsc::Receiver<Result<Vec<Vec<u64>>>>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) -> Result<()> {
    let log = Log::new(Some(&ctx.status_tx));
    let mut next_question: u32 = 0;
    let mut pending: HashMap<u32, Pending> = HashMap::new();
    loop {
        let (action, depth, mut injected) = tokio::select! {
            item = messages.recv() => {
                let Some(item) = item else { return Ok(()) };
                let msg = Message::new(item?);
                let reply = match rpc::decode(&msg)? {
                    Incoming::Return { answer, returned } => {
                        if let Some(p) = pending.remove(&answer) {
                            let (results, exception) = match returned {
                                Returned::Results { content: Target::Struct(s), .. } => {
                                    (session.schema.to_json(p.results, s)?, Value::Null)
                                }
                                Returned::Results { .. } => (json!({}), Value::Null),
                                Returned::Exception { reason, kind } => (
                                    Value::Null,
                                    json!({"reason": reason, "kind": rpc::exception_name(kind)}),
                                ),
                                Returned::Other => (
                                    Value::Null,
                                    json!({"reason": "the call was not answered", "kind": "failed"}),
                                ),
                            };
                            let data = json!({"method": p.method, "results": results, "exception": exception});
                            if let Some(command) = p.caller {
                                crate::client::command_support::reply(command, Ok(ClientSendOutcome::Executed { detail: data.to_string() }));
                            }
                            ctx.state.record_access_log(AccessLogOwner::Client(ctx.client_id.as_u32()), "Cap'n Proto RPC", None, "capnp_result", data.clone(), vec![]).await;
                            events.try_send((Event::new(&actions::RESULT_EVENT, data), p.depth))
                                .context("Cap'n Proto event queue full; consumer stalled")?;
                            Some(rpc::finish(answer, true)?)
                        } else {
                            None
                        }
                    }
                    Incoming::Call { question, .. } => Some(rpc::return_exception(question, "this client exports no capabilities", rpc::EXC_UNIMPLEMENTED)?),
                    Incoming::Bootstrap { question } => Some(rpc::return_exception(question, "this client exports no bootstrap capability", rpc::EXC_UNIMPLEMENTED)?),
                    Incoming::Abort { reason } => bail!("the server aborted: {reason}"),
                    Incoming::Other(_) => Some(rpc::unimplemented(&msg)?),
                    _ => None,
                };
                if let Some(words) = reply {
                    send(&mut writer, &words).await?;
                }
                continue;
            }
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some((a, d)) => (a, d, None),
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
        match CapnpRpcClientProtocol.execute_action(action.clone()) {
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
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "Cap'n Proto client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if pending.len() >= MAX_PENDING {
            refuse(&mut injected, "too many calls awaiting answers".into());
            continue;
        }
        next_question = next_question.wrapping_add(1).max(1);
        let (words, method, results) = match encode_call(session, next_question, &action) {
            Ok(v) => v,
            Err(e) => {
                log.warn(format!("Cap'n Proto call refused: {e:#}"));
                refuse(&mut injected, format!("{e:#}"));
                continue;
            }
        };
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Cap'n Proto RPC",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        send(&mut writer, &words).await?;
        pending.insert(
            next_question,
            Pending {
                method,
                results,
                caller: injected.take(),
                depth,
            },
        );
    }
}
