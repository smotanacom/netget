pub mod actions;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::diameter::{codec::*, control, reader, Config};
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
use crate::{console_error, console_info};
pub use actions::DiameterClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    net::{tcp::OwnedWriteHalf, TcpStream},
    time::Instant,
};
pub const MAX_QUEUED_EVENTS: usize = 32;
pub const MAX_HANDLER_ACTIONS: usize = 32;
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
type Handler = Pin<Box<dyn Future<Output = Result<crate::llm::ClientLlmResult>> + Send>>;
fn handler(ctx: ConnectContext, event: Event, deadline: Duration) -> Handler {
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
                    "DIAMETER",
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
            deadline,
            crate::client::llm_budget::call_llm_for_client(
                &ctx.llm_client,
                &ctx.state,
                ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &DiameterClientProtocol,
                &ctx.status_tx,
            ),
        )
        .await
        .context("Diameter event handler deadline")?
    })
}
struct Pending {
    packet: Packet,
    request: Value,
    command: Option<ClientCommand>,
    depth: usize,
    deadline: Instant,
}
struct Closing {
    packet: Packet,
    command: Option<ClientCommand>,
    deadline: Instant,
}
pub struct DiameterClient;
impl DiameterClient {
    pub async fn connect(mut ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        ensure!(
            ctx.remote_addr.len() <= 1024 && !ctx.remote_addr.contains(['\r', '\n', '@', '/']),
            "Diameter endpoint host:port only"
        );
        let p = ctx
            .startup_params
            .as_ref()
            .context("Diameter identity parameters required")?;
        let cfg = Arc::new(Config {
            identity: crate::server::diameter::actions::identity(p)?,
            io_timeout: timeout(
                p.get_optional_u64("io_timeout_seconds")?,
                DEFAULT_IO_SECONDS,
            )?,
            handler_timeout: timeout(
                p.get_optional_u64("handler_timeout_seconds")?,
                DEFAULT_HANDLER_SECONDS,
            )?,
            watchdog_interval: timeout(
                p.get_optional_u64("watchdog_interval_seconds")?,
                DEFAULT_WATCHDOG_SECONDS,
            )?,
            llm_fallback: false,
        });
        ctx.startup_params = None;
        let socket = tokio::time::timeout(cfg.io_timeout, TcpStream::connect(&ctx.remote_addr))
            .await
            .context("Diameter TCP connect deadline")??;
        let local = socket.local_addr()?;
        let connected_addr = socket.peer_addr()?;
        let (mut half, mut write) = socket.into_split();
        let mut cer = Packet::request(CER, 0)?;
        capability_fields(&mut cer, &cfg.identity, local.ip());
        write_packet(&mut write, &cer).await?;
        let answer = read_packet(&mut half, cfg.io_timeout).await?;
        ensure!(answer.matches(&cer), "Diameter CEA correlation");
        let peer = capabilities(&answer)?;
        ctx.state
            .with_client_mut(ctx.client_id, |client| {
                let now = crate::utils::clock::Instant::now();
                client.connection = Some(crate::state::client::ClientConnectionState {
                    id: ctx.client_id,
                    remote_addr: ctx.remote_addr.clone(),
                    connected_addr: Some(connected_addr),
                    local_addr: Some(local),
                    bytes_sent: cer.encode().map(|v| v.len()).unwrap_or(0) as u64,
                    bytes_received: answer.encode().map(|v| v.len()).unwrap_or(0) as u64,
                    packets_sent: 1,
                    packets_received: 1,
                    last_activity: now,
                    status: ClientStatus::Connected,
                    status_changed_at: now,
                    protocol_info: crate::state::server::ProtocolConnectionInfo::empty(),
                });
            })
            .await;
        let commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        console_info!(
            ctx.status_tx,
            "Diameter NASREQ capabilities negotiated with {}",
            ctx.remote_addr
        );
        ctx.state
            .clone()
            .spawn_client_task(ctx.client_id, run(ctx, cfg, peer, half, write, commands))
            .await;
        Ok(local)
    }
}
async fn stats(ctx: &ConnectContext, bytes: u64, received: bool) {
    ctx.state
        .with_client_mut(ctx.client_id, |client| {
            if let Some(connection) = client.connection.as_mut() {
                if received {
                    connection.bytes_received += bytes;
                    connection.packets_received += 1;
                } else {
                    connection.bytes_sent += bytes;
                    connection.packets_sent += 1;
                }
                connection.last_activity = crate::utils::clock::Instant::now();
            }
        })
        .await;
}
async fn send(ctx: &ConnectContext, write: &mut OwnedWriteHalf, packet: &Packet) -> Result<()> {
    let bytes = write_packet(write, packet).await?;
    stats(ctx, bytes as u64, false).await;
    Ok(())
}
fn prepared(value: Value, depth: usize) -> Result<Option<Request>> {
    ensure!(depth <= MAX_FOLLOWUP_DEPTH, "Diameter follow-up depth");
    match DiameterClientProtocol.execute_action(value)? {
        ClientActionResult::Disconnect => Ok(None),
        ClientActionResult::Custom { name, data } if name == "diameter_command" => {
            Ok(Some(actions::parse_action(data)?))
        }
        _ => bail!("Diameter action result"),
    }
}
fn request(
    request: Request,
    cfg: &Config,
    peer: &Identity,
    command: Option<ClientCommand>,
    depth: usize,
) -> Result<Pending> {
    let session = format!("{};{}", cfg.identity.host, uuid::Uuid::new_v4());
    let packet = request.packet(&cfg.identity, peer, &session)?;
    let mut safe = request.credential_free();
    safe["session_id"] = json!(session);
    Ok(Pending {
        packet,
        request: safe,
        command,
        depth,
        deadline: Instant::now() + cfg.io_timeout,
    })
}
async fn finish(ctx: &ConnectContext, command: ClientCommand, result: Result<ClientSendOutcome>) {
    let outcome = match &result {
        Ok(v) => serde_json::to_value(v).unwrap_or(Value::Null),
        Err(_) => json!({"error":"Diameter operation failed or cancelled; no replay"}),
    };
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.client_id.as_u32()),
            "DIAMETER",
            None,
            "injected_action",
            crate::utils::redact::redact_sensitive(&command.action),
            vec![outcome],
        )
        .await;
    crate::client::command_support::reply(command, result);
}
async fn begin_close(
    ctx: &ConnectContext,
    cfg: &Config,
    write: &mut OwnedWriteHalf,
    pending: &mut Option<Pending>,
    command: Option<ClientCommand>,
) -> Result<Closing> {
    if let Some(p) = pending.as_mut() {
        if let Some(command) = p.command.take() {
            finish(
                ctx,
                command,
                Err(anyhow::anyhow!("Diameter request cancelled")),
            )
            .await;
        }
    }
    let mut packet = Packet::request(DPR, 0)?;
    packet.origin(&cfg.identity);
    packet.avps.push(Avp::number(DISCONNECT_CAUSE, 0));
    send(ctx, write, &packet).await?;
    Ok(Closing {
        packet,
        command,
        deadline: Instant::now() + cfg.io_timeout,
    })
}
async fn run(
    ctx: ConnectContext,
    cfg: Arc<Config>,
    peer: Identity,
    half: tokio::net::tcp::OwnedReadHalf,
    mut write: OwnedWriteHalf,
    mut commands: tokio::sync::mpsc::Receiver<ClientCommand>,
) {
    let connected = Event::new(
        &actions::CONNECTED_EVENT,
        json!({"peer":{"origin_host":peer.host,"origin_realm":peer.realm,"remote_addr":ctx.remote_addr}}),
    );
    let mut current = Some((handler(ctx.clone(), connected, cfg.handler_timeout), 0usize));
    let mut events = VecDeque::new();
    let mut queued: VecDeque<(Value, usize)> = VecDeque::new();
    let mut pending: Option<Pending> = None;
    let mut closing: Option<Closing> = None;
    let mut watchdog: Option<(Packet, Instant)> = None;
    let mut next_watchdog = Instant::now() + cfg.watchdog_interval;
    let mut reading = reader(half, cfg.io_timeout);
    let mut failed = false;
    'peer: loop {
        if closing.is_none() {
            if current.is_none() {
                if let Some((event, depth)) = events.pop_front() {
                    current = Some((handler(ctx.clone(), event, cfg.handler_timeout), depth));
                }
            }
            if pending.is_none() {
                if let Some((action, depth)) = queued.pop_front() {
                    match prepared(action, depth) {
                        Ok(Some(r)) => match request(r, &cfg, &peer, None, depth) {
                            Ok(work) => {
                                if send(&ctx, &mut write, &work.packet).await.is_err() {
                                    failed = true;
                                    break;
                                }
                                pending = Some(work);
                            }
                            Err(_) => {
                                failed = true;
                                break;
                            }
                        },
                        Ok(None) => {
                            match begin_close(&ctx, &cfg, &mut write, &mut pending, None).await {
                                Ok(v) => {
                                    closing = Some(v);
                                    drop(current.take());
                                }
                                Err(_) => {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                        Err(_) => console_error!(
                            ctx.status_tx,
                            "Diameter handler action refused decision=fail_closed_action"
                        ),
                    }
                    continue;
                }
            }
        }
        tokio::select! {
         (result,half)=&mut reading=>{
             let packet = match result {
                 Ok(p) => p,
                 Err(_) => {
                     failed = true;
                     break;
                 }
             };
             stats(
                 &ctx,
                 packet.encode().map(|v| v.len()).unwrap_or(0) as u64,
                 true,
             )
             .await;
             reading = reader(half, cfg.io_timeout);
             next_watchdog = Instant::now() + cfg.watchdog_interval;
             if packet.is_request() {
                 if !matches!(packet.command, DWR | DPR) || control(&packet, &peer).is_err() {
                     failed = true;
                     break;
                 }
                 let mut answer = packet.answer(2001);
                 answer.origin(&cfg.identity);
                 if send(&ctx, &mut write, &answer).await.is_err() {
                     failed = true;
                     break;
                 }
                 if packet.command == DPR {
                     if let Some(v) = closing.take() {
                         if let Some(command) = v.command {
                             finish(&ctx, command, Ok(ClientSendOutcome::Disconnected)).await;
                         }
                     }
                     break;
                 }
                 continue;
             }
             if let Some(work) = closing.as_ref() {
                 if packet.matches(&work.packet) && control(&packet, &peer).is_ok() {
                     if let Some(command) = closing.take().unwrap().command {
                         finish(&ctx, command, Ok(ClientSendOutcome::Disconnected)).await;
                     }
                     break;
                 }
                 if pending.as_ref().is_some_and(|p| packet.matches(&p.packet)) {
                     pending = None;
                     continue;
                 }
             }
             if watchdog.as_ref().is_some_and(|w| packet.matches(&w.0)) {
                 if control(&packet, &peer).is_err() {
                     failed = true;
                     break;
                 }
                 watchdog = None;
                 continue;
             }
             let Some(work) = pending.take() else {
                 failed = true;
                 break;
             };
             let response = match Response::from_packet(&packet, &work.packet, &peer) {
                 Ok(v) => v,
                 Err(_) => {
                     pending = Some(work);
                     failed = true;
                     break;
                 }
             };
             if let Some(command) = work.command {
                 finish(
                     &ctx,
                     command,
                     Ok(ClientSendOutcome::Executed {
                         detail: format!("Diameter NASREQ result {}", response.result_code),
                     }),
                 )
                 .await;
             }
             if events.len() >= MAX_QUEUED_EVENTS {
                 failed = true;
                 break;
             }
             events.push_back((
                 Event::new(
                     &actions::AA_EVENT,
                     json!({"request":work.request,"reply":response}),
                 ),
                 work.depth,
             ));
         }
         result=async{current.as_mut().unwrap().0.as_mut().await},if current.is_some()&&closing.is_none()=>{
             let (_, depth) = current.take().unwrap();
             match result {
                 Ok(result) => {
                     if result.actions.len() > MAX_HANDLER_ACTIONS
                         || queued.len() + result.actions.len() > MAX_HANDLER_ACTIONS
                         || result.actions.iter().any(|v| !within_json_budget(v))
                     {
                         for v in result.actions {
                             crate::utils::json_budget::drop_iteratively(v);
                         }
                         failed = true;
                         break;
                     }
                     if result
                         .actions
                         .iter()
                         .any(|v| DiameterClientProtocol.execute_action(v.clone()).is_err())
                     {
                         for v in result.actions {
                             crate::utils::json_budget::drop_iteratively(v);
                         }
                         console_error!(
                             ctx.status_tx,
                             "Diameter handler action invalid decision=fail_closed_action"
                         );
                         continue;
                     }
                     if result.actions.iter().any(|v| v["type"] == "disconnect") {
                         for v in result.actions {
                             crate::utils::json_budget::drop_iteratively(v);
                         }
                         match begin_close(&ctx, &cfg, &mut write, &mut pending, None).await {
                             Ok(v) => closing = Some(v),
                             Err(_) => {
                                 failed = true;
                                 break;
                             }
                         }
                         continue;
                     }
                     if let Some(memory) = result.memory_updates {
                         ctx.state.set_memory_for_client(ctx.client_id, memory).await;
                     }
                     queued.extend(result.actions.into_iter().map(|v| (v, depth + 1)));
                 }
                 Err(_) => console_error!(
                     ctx.status_tx,
                     "Diameter handler failed decision=fail_closed_handler"
                 ),
             }
         }
         command=commands.recv(),if closing.is_none()=>{
             let Some(mut command) = command else {
                 break;
             };
             if !within_json_budget(&command.action) {
                 crate::utils::json_budget::drop_iteratively(std::mem::take(&mut command.action));
                 finish(
                     &ctx,
                     command,
                     Ok(ClientSendOutcome::Rejected {
                         error: "Diameter JSON size/node/depth budget".into(),
                     }),
                 )
                 .await;
                 continue;
             }
             match prepared(command.action.clone(), 0) {
                 Ok(None) => {
                     drop(current.take());
                     match begin_close(&ctx, &cfg, &mut write, &mut pending, Some(command)).await {
                         Ok(v) => closing = Some(v),
                         Err(_) => {
                             failed = true;
                             break 'peer;
                         }
                     }
                 }
                 Ok(Some(r)) if pending.is_none() => match request(r, &cfg, &peer, Some(command), 0) {
                     Ok(work) => {
                         if send(&ctx, &mut write, &work.packet).await.is_err() {
                             pending = Some(work);
                             failed = true;
                             break;
                         }
                         pending = Some(work);
                     }
                     Err(_) => {
                         failed = true;
                         break;
                     }
                 },
                 Ok(Some(_)) =>
                     finish(
                         &ctx,
                         command,
                         Ok(ClientSendOutcome::Rejected {
                             error: "One Diameter AAA request already in flight".into(),
                         }),
                     )
                     .await,
                 Err(_) =>
                     finish(
                         &ctx,
                         command,
                         Ok(ClientSendOutcome::Rejected {
                             error: "Diameter action invalid or over bound".into(),
                         }),
                     )
                     .await,
             }
         }
         _=tokio::time::sleep_until(pending.as_ref().map(|v|v.deadline).unwrap_or_else(Instant::now)),if pending.is_some()&&closing.is_none()=>{
             failed = true;
             break;
         }
         _=tokio::time::sleep_until(closing.as_ref().map(|v|v.deadline).unwrap_or_else(Instant::now)),if closing.is_some()=>{
             failed = true;
             break;
         }
         _=tokio::time::sleep_until(next_watchdog),if watchdog.is_none()&&closing.is_none()=>{
             let mut p = match Packet::request(DWR, 0) {
                 Ok(v) => v,
                 Err(_) => {
                     failed = true;
                     break;
                 }
             };
             p.origin(&cfg.identity);
             if send(&ctx, &mut write, &p).await.is_err() {
                 failed = true;
                 break;
             }
             watchdog = Some((p, Instant::now() + cfg.io_timeout));
         }
         _=tokio::time::sleep_until(watchdog.as_ref().map(|v|v.1).unwrap_or_else(Instant::now)),if watchdog.is_some()&&closing.is_none()=>{
             failed = true;
             break;
         }
        }
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
    // Dropping this retained read future closes the owned TCP half even when a
    // partially read frame was interrupted. No detached reader or retry remains.
    drop(reading);
    drop(write);
    drop(current.take());
    for (v, _) in queued {
        crate::utils::json_budget::drop_iteratively(v);
    }
    events.clear();
    if let Some(p) = pending {
        if let Some(command) = p.command {
            finish(
                &ctx,
                command,
                Err(anyhow::anyhow!("Diameter operation failed or cancelled")),
            )
            .await;
        }
    }
    if let Some(v) = closing {
        if let Some(command) = v.command {
            finish(
                &ctx,
                command,
                Err(anyhow::anyhow!("Diameter disconnect failed")),
            )
            .await;
        }
    }
    ctx.state.remove_client_handle(ctx.client_id).await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Disconnected)
        .await;
    if failed {
        console_error!(
            ctx.status_tx,
            "Diameter peer failed decision=fail_closed_peer_session"
        );
        if let Ok(result) = handler(
            ctx.clone(),
            Event::new(
                &actions::ERROR_EVENT,
                json!({"error":"Diameter transport, framing, correlation or deadline failure; no replay"}),
            ),
            cfg.handler_timeout,
        )
        .await
        {
            for v in result.actions {
                crate::utils::json_budget::drop_iteratively(v);
            }
        }
    }
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}
