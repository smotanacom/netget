pub mod actions;
pub mod codec;
use crate::llm::actions::{executor::ExecutionResult, protocol_trait::ActionResult};
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::{
    server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
    AccessLogOwner,
};
use anyhow::{bail, ensure, Context, Result};
use codec::*;
use serde_json::{json, Value};
use std::{future::Future, net::SocketAddr, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    time::Instant,
};

pub struct Config {
    pub identity: Identity,
    pub io_timeout: Duration,
    pub handler_timeout: Duration,
    pub watchdog_interval: Duration,
    pub llm_fallback: bool,
}
impl Config {
    pub fn from_params(p: &crate::protocol::StartupParams) -> Result<Self> {
        Ok(Self {
            identity: actions::identity(p)?,
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
            llm_fallback: p
                .get_optional_bool("llm_fallback")?
                .unwrap_or(DEFAULT_LLM_FALLBACK),
        })
    }
}
pub type Reader = Pin<Box<dyn Future<Output = (Result<Packet>, OwnedReadHalf)> + Send>>;
pub fn reader(mut half: OwnedReadHalf, deadline: Duration) -> Reader {
    Box::pin(async move {
        let packet = read_packet(&mut half, deadline).await;
        (packet, half)
    })
}
pub struct DiameterServer;
impl DiameterServer {
    pub async fn spawn(mut ctx: SpawnContext) -> Result<SocketAddr> {
        let cfg = Arc::new(Config::from_params(
            ctx.startup_params
                .as_ref()
                .context("Diameter identity parameters required")?,
        )?);
        ctx.startup_params = None;
        let listener =
            TcpListener::bind(ctx.socket_addr().context("Diameter TCP bind address")?).await?;
        let local = listener.local_addr()?;
        let sid = ctx.server_id;
        Log::new(Some(&ctx.status_tx))
            .info(format!("Diameter experimental NASREQ listening on {local}"));
        ctx.state
            .clone()
            .spawn_server_task(sid, async move {
                let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
                loop {
                    let (socket, peer, permit) = match accept_bounded(
                        &listener,
                        &limiter,
                        b"",
                        "DIAMETER",
                        Some(&ctx.status_tx),
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let cid = ConnectionId::new(ctx.state.get_next_unified_id().await);
                    let now = crate::utils::clock::Instant::now();
                    ctx.state
                        .add_connection_to_server(
                            sid,
                            ConnectionState {
                                id: cid,
                                remote_addr: peer,
                                local_addr: local,
                                bytes_sent: 0,
                                bytes_received: 0,
                                packets_sent: 0,
                                packets_received: 0,
                                last_activity: now,
                                status: ConnectionStatus::Active,
                                status_changed_at: now,
                                protocol_info: ProtocolConnectionInfo::empty(),
                            },
                        )
                        .await;
                    let child = ctx.clone();
                    let config = cfg.clone();
                    ctx.state
                        .spawn_server_task(sid, async move {
                            let _permit = permit;
                            if session(&child, cid, socket, peer, &config).await.is_err() {
                                decision(&child, cid, "fail_closed_peer_session");
                            }
                            child
                                .state
                                .update_connection_status(sid, cid, ConnectionStatus::Closed)
                                .await;
                            child.state.remove_connection_from_server(sid, cid).await;
                            let _ = child.status_tx.send("__UPDATE_UI__".into());
                        })
                        .await;
                }
            })
            .await;
        Ok(local)
    }
}
fn decision(ctx: &SpawnContext, id: ConnectionId, tag: &str) {
    Log::new(Some(&ctx.status_tx)).info(format!("Diameter connection={id} decision={tag}"));
}
async fn send(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut OwnedWriteHalf,
    p: &Packet,
) -> Result<()> {
    let n = write_packet(w, p).await?;
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(n as u64), None, Some(1))
        .await;
    Ok(())
}
async fn received(ctx: &SpawnContext, id: ConnectionId, p: &Packet) {
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(p.encode().map(|b| b.len()).unwrap_or(0) as u64),
            None,
            Some(1),
            None,
        )
        .await;
}
pub fn control(p: &Packet, peer: &Identity) -> Result<()> {
    ensure!(
        p.application == 0 && p.flags & 0x40 == 0 && p.flags & 0x20 == 0,
        "Diameter peer-control header"
    );
    peer.same_peer(p)?;
    let allowed = if p.command == DPR {
        &[HOST, REALM, ORIGIN_STATE, DISCONNECT_CAUSE, RESULT][..]
    } else {
        &[HOST, REALM, ORIGIN_STATE, RESULT][..]
    };
    ensure!(
        p.unsupported_mandatory(allowed).is_none(),
        "Diameter unsupported mandatory peer-control AVP"
    );
    if let Some(a) = p.optional(ORIGIN_STATE)? {
        a.integer()?;
    }
    if p.is_request() && p.command == DPR {
        ensure!(p.num(DISCONNECT_CAUSE)? <= 2, "Diameter Disconnect-Cause");
    }
    if !p.is_request() {
        ensure!(p.num(RESULT)? == 2001, "Diameter peer-control rejected");
    }
    Ok(())
}
/// All execution failures precede any positive wire verdict. Constructed result
/// trees and raw actions are consumed iteratively before JSON copies or decoding.
pub fn chosen_reply(result: ExecutionResult) -> Result<Option<Reply>> {
    let mut invalid = !result.failures.is_empty() || result.raw_actions.len() > 32;
    for v in result.raw_actions {
        invalid |= !within_json_budget(&v);
        crate::utils::json_budget::drop_iteratively(v);
    }
    let mut pending = result.protocol_results;
    let mut found: Option<Value> = None;
    let mut seen = 0usize;
    while let Some(result) = pending.pop() {
        seen = seen.saturating_add(1);
        invalid |= seen > 64;
        match result {
            ActionResult::Multiple(v) => pending.extend(v),
            ActionResult::Custom { name, data } => {
                if name == "respond_diameter_aa" && found.is_none() && within_json_budget(&data) {
                    found = Some(data);
                } else {
                    invalid = true;
                    crate::utils::json_budget::drop_iteratively(data);
                }
            }
            ActionResult::NoAction => {}
            _ => invalid = true,
        }
    }
    if invalid {
        if let Some(v) = found {
            crate::utils::json_budget::drop_iteratively(v);
        }
        bail!("Diameter handler failure/type/count/budget");
    }
    found
        .map(|v| {
            let reply: Reply = serde_json::from_value(v)?;
            reply.validate()?;
            Ok(reply)
        })
        .transpose()
}
type Handler = Pin<Box<dyn Future<Output = Result<Option<Reply>>> + Send>>;
fn handler(ctx: SpawnContext, cid: ConnectionId, event: Event, cfg: Arc<Config>) -> Handler {
    Box::pin(async move {
        let configured = ctx
            .state
            .get_event_handler_config(ctx.server_id)
            .await
            .is_some_and(|c| c.find_handler(event.id()).is_some());
        if !configured && !cfg.llm_fallback {
            let event_id = event.id().to_owned();
            ctx.state
                .record_access_log(
                    AccessLogOwner::Server(ctx.server_id.as_u32()),
                    "DIAMETER",
                    Some(cid.as_u32()),
                    &event_id,
                    event.data,
                    vec![],
                )
                .await;
            return Ok(None);
        }
        let result = tokio::time::timeout(
            cfg.handler_timeout,
            crate::llm::action_helper::call_llm(
                &ctx.llm_client,
                &ctx.state,
                ctx.server_id,
                Some(cid),
                &event,
                &actions::DiameterProtocol,
            ),
        )
        .await
        .context("Diameter AAA handler deadline")??;
        chosen_reply(result)
    })
}
async fn refuse(
    ctx: &SpawnContext,
    cid: ConnectionId,
    w: &mut OwnedWriteHalf,
    p: &Packet,
    id: &Identity,
    code: u32,
    failed: Option<&Avp>,
    tag: &str,
) -> Result<()> {
    decision(ctx, cid, tag);
    send(ctx, cid, w, &error_answer(p, id, code, failed)?).await
}
async fn session(
    ctx: &SpawnContext,
    cid: ConnectionId,
    socket: TcpStream,
    source: SocketAddr,
    cfg: &Arc<Config>,
) -> Result<()> {
    let local_ip = socket.local_addr()?.ip();
    let (mut half, mut w) = socket.into_split();
    let first = read_packet(&mut half, cfg.io_timeout).await?;
    received(ctx, cid, &first).await;
    ensure!(
        first.is_request() && first.command == CER,
        "Diameter CER required first"
    );
    let peer = match capabilities(&first) {
        Ok(peer) => peer,
        Err(_) => {
            let failed = first.unsupported_mandatory(BASE_ALLOWED);
            let security = first.avps.iter().any(|a| {
                a.code == SECURITY && a.vendor.is_none() && a.integer().ok().is_some_and(|v| v != 0)
            });
            let code =
                if failed.is_some() {
                    5001
                } else if security {
                    5017
                } else if !first.avps.iter().any(|a| {
                    a.code == AUTH_APP && a.vendor.is_none() && a.integer().ok() == Some(1)
                }) {
                    5010
                } else {
                    5012
                };
            decision(ctx, cid, "fail_closed_capabilities");
            let mut answer = error_answer(&first, &cfg.identity, code, failed)?;
            answer.avps.retain(|a| ![HOST, REALM].contains(&a.code));
            capability_fields(&mut answer, &cfg.identity, local_ip);
            send(ctx, cid, &mut w, &answer).await?;
            return Ok(());
        }
    };
    let mut cea = first.answer(2001);
    capability_fields(&mut cea, &cfg.identity, local_ip);
    send(ctx, cid, &mut w, &cea).await?;
    decision(ctx, cid, "peer_capabilities_accepted");
    let mut reading = reader(half, cfg.io_timeout);
    let mut current: Option<(Packet, Handler)> = None;
    let mut watchdog: Option<(Packet, Instant)> = None;
    let mut next_watchdog = Instant::now() + cfg.watchdog_interval;
    loop {
        tokio::select! {
         (result,half)=&mut reading=>{
             let p = result?;
             received(ctx, cid, &p).await;
             reading = reader(half, cfg.io_timeout);
             next_watchdog = Instant::now() + cfg.watchdog_interval;
             if !p.is_request() {
                 let Some((request, _)) = watchdog.take() else {
                     bail!("Diameter unexpected answer");
                 };
                 ensure!(p.matches(&request), "Diameter watchdog correlation");
                 control(&p, &peer)?;
                 continue;
             }
             match p.command {
                 DWR | DPR => {
                     control(&p, &peer)?;
                     let mut answer = p.answer(2001);
                     answer.origin(&cfg.identity);
                     send(ctx, cid, &mut w, &answer).await?;
                     if p.command == DPR {
                         decision(ctx, cid, "peer_disconnect_accepted");
                         break;
                     }
                 }
                 CER => {
                     refuse(
                         ctx,
                         cid,
                         &mut w,
                         &p,
                         &cfg.identity,
                         5012,
                         None,
                         "fail_closed_repeated_capabilities",
                     )
                     .await?;
                     break;
                 }
                 AA if p.application == 1 => {
                     if let Some(failed) = p.unsupported_mandatory(REQUEST_ALLOWED) {
                         refuse(
                             ctx,
                             cid,
                             &mut w,
                             &p,
                             &cfg.identity,
                             5001,
                             Some(failed),
                             "fail_closed_mandatory_avp",
                         )
                         .await?;
                         continue;
                     }
                     let (request, session) = match Request::from_packet(&p, &cfg.identity, &peer) {
                         Ok(v) => v,
                         Err(_) => {
                             refuse(
                                 ctx,
                                 cid,
                                 &mut w,
                                 &p,
                                 &cfg.identity,
                                 5012,
                                 None,
                                 "fail_closed_request_semantics",
                             )
                             .await?;
                             continue;
                         }
                     };
                     if current.is_some() {
                         refuse(
                             ctx,
                             cid,
                             &mut w,
                             &p,
                             &cfg.identity,
                             3004,
                             None,
                             "fail_closed_handler_capacity",
                         )
                         .await?;
                         continue;
                     }
                     let mut data = serde_json::to_value(request)?;
                     data["session_id"] = json!(session);
                     data["origin_host"] = json!(peer.host);
                     data["origin_realm"] = json!(peer.realm);
                     data["source_addr"] = json!(source.to_string());
                     let event = Event::new(&actions::AA_EVENT, json!({"request":data}));
                     current = Some((p, handler(ctx.clone(), cid, event, cfg.clone())));
                 }
                 _ => {
                     let code = if p.application != 0 && p.application != 1 {
                         3007
                     } else {
                         3001
                     };
                     refuse(
                         ctx,
                         cid,
                         &mut w,
                         &p,
                         &cfg.identity,
                         code,
                         None,
                         "fail_closed_unsupported_command_application",
                     )
                     .await?;
                 }
             }
         }
         result=async{current.as_mut().unwrap().1.as_mut().await},if current.is_some()=>{
             let (request, _) = current.take().unwrap();
             match result {
                 Ok(reply) => {
                     let reply = reply.unwrap_or_default();
                     decision(
                         ctx,
                         cid,
                         match reply.verdict {
                             Verdict::Accept => "handler_accept",
                             Verdict::Reject => "handler_reject_or_no_action",
                             Verdict::Error => "handler_error",
                         },
                     );
                     let answer = reply.packet(&request, &cfg.identity)?;
                     send(ctx, cid, &mut w, &answer).await?;
                 }
                 Err(error) => {
                     let code = match crate::utils::WireFailure::classify(&error) {
                         crate::utils::WireFailure::Overloaded => 3004,
                         crate::utils::WireFailure::Unavailable => 5012,
                     };
                     refuse(
                         ctx,
                         cid,
                         &mut w,
                         &request,
                         &cfg.identity,
                         code,
                         None,
                         "fail_closed_handler_failure",
                     )
                     .await?;
                 }
             }
             next_watchdog = Instant::now() + cfg.watchdog_interval;
         }
         _=tokio::time::sleep_until(next_watchdog),if watchdog.is_none()=>{
             let mut request = Packet::request(DWR, 0)?;
             request.origin(&cfg.identity);
             send(ctx, cid, &mut w, &request).await?;
             watchdog = Some((request, Instant::now() + cfg.io_timeout));
         }
         _=tokio::time::sleep_until(watchdog.as_ref().map(|v|v.1).unwrap_or_else(Instant::now)),if watchdog.is_some()=>{
             decision(ctx, cid, "fail_closed_watchdog_deadline");
             bail!("Diameter watchdog answer deadline");
         }
        }
    }
    Ok(())
}
