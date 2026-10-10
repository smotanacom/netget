//! ONC RPC portmapper / rpcbind server on TCP and UDP (the same port). Rust decodes every
//! call and answers the RPC-level errors, NULL and GETTIME; the model answers lookups and
//! dumps with mappings and decides registrations.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::Result;
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use wire::{Accepted, Mapping, Reader, Writer};

/// A TCP connection that sends no complete call for this long is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// UDP calls answered at once; past it, datagrams wait in the socket buffer.
pub const MAX_UDP_IN_FLIGHT: usize = 64;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    // UDP on the same port as TCP, as every portmapper does.
    let udp = Arc::new(UdpSocket::bind(local).await?);
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Portmapper (PMAP 2, RPCBIND 3-4) listening on {local} tcp and udp"
    ));
    let server_id = ctx.server_id;
    let state = ctx.state.clone();

    let udp_ctx = ctx.clone();
    let udp_task = tokio::spawn(async move {
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_UDP_IN_FLIGHT));
        let mut buf = vec![0u8; 65_536];
        loop {
            let Ok((n, peer)) = udp.recv_from(&mut buf).await else {
                break;
            };
            let Ok(permit) = permits.clone().acquire_owned().await else {
                break;
            };
            let msg = buf[..n].to_vec();
            let ctx = udp_ctx.clone();
            let udp = udp.clone();
            udp_ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Some(reply) = handle(&ctx, None, &msg, "udp", local.ip()).await {
                        let _ = udp.send_to(&reply, peer).await;
                    }
                })
                .await;
        }
    });
    state.register_server_task(server_id, udp_task).await;

    let accept =
        tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                let (stream, peer, permit) =
                    match accept_bounded(&listener, &limiter, b"", "SunRPC", Some(&ctx.status_tx))
                        .await
                    {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                let child = ctx.clone();
                ctx.state
                    .spawn_server_task(server_id, async move {
                        let _permit = permit;
                        connection(child, stream, peer, local).await;
                    })
                    .await;
            }
        });
    state.register_server_task(server_id, accept).await;
    Ok(local)
}

async fn connection(ctx: SpawnContext, stream: TcpStream, remote: SocketAddr, local: SocketAddr) {
    let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
    let now = crate::utils::clock::Instant::now();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id,
                remote_addr: remote,
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
    let local_ip = stream.local_addr().map(|a| a.ip()).unwrap_or(local.ip());
    let (mut r, mut w) = stream.into_split();
    let reason = loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, wire::read_record(&mut r)).await {
            Err(_) => break "idle".to_string(),
            Ok(Ok(None)) => break "closed by the peer".to_string(),
            Ok(Err(e)) => break format!("refused: {e:#}"),
            Ok(Ok(Some(m))) => m,
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(msg.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        if let Some(reply) = handle(&ctx, Some(id), &msg, "tcp", local_ip).await {
            if w.write_all(&wire::record(&reply)).await.is_err() {
                break "write failed".into();
            }
            ctx.state
                .update_connection_stats(
                    ctx.server_id,
                    id,
                    None,
                    Some(reply.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
    };
    let _ = w.shutdown().await;
    ctx.state
        .update_connection_status(ctx.server_id, id, ConnectionStatus::Closed)
        .await;
    Log::new(Some(&ctx.status_tx)).debug(format!("SunRPC connection {id} ended: {reason}"));
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}

fn outcome(ctx: &SpawnContext, op: &str, decision: &str) {
    let line = format!("SunRPC operation={op} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed") || decision == "model_silent" {
        log.error(line)
    } else {
        log.info(line)
    }
}

/// Ask the model; its one portmapper answer, or None when the call must fail (SYSTEM_ERR).
async fn ask(
    ctx: &SpawnContext,
    conn: Option<ConnectionId>,
    event_type: &'static EventType,
    data: Value,
    op: &str,
) -> Option<(String, Value)> {
    let event = Event::new(event_type, data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        conn,
        &event,
        &actions::SunRpcProtocol,
    )
    .await
    {
        Ok(r) if r.failures.is_empty() => r,
        Ok(_) => {
            outcome(ctx, op, "fail_closed_invalid_reply");
            return None;
        }
        Err(_) => {
            outcome(ctx, op, "fail_closed_llm_error");
            return None;
        }
    };
    let mut answers = Vec::new();
    let mut stack = result.protocol_results;
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("sunrpc_") => {
                answers.push((name, data))
            }
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    match answers.len() {
        1 => {
            outcome(ctx, op, "model_answer");
            answers.pop()
        }
        0 => {
            outcome(ctx, op, "model_silent");
            None
        }
        _ => {
            outcome(ctx, op, "fail_closed_invalid_reply");
            None
        }
    }
}

/// The model's mappings for a query; None on failure.
async fn mappings(
    ctx: &SpawnContext,
    conn: Option<ConnectionId>,
    data: Value,
    op: &str,
) -> Option<Vec<Mapping>> {
    let (name, answer) = ask(ctx, conn, &actions::QUERY_EVENT, data, op).await?;
    if name != actions::MAPPINGS {
        outcome(ctx, op, "fail_closed_invalid_reply");
        return None;
    }
    answer["mappings"]
        .as_array()?
        .iter()
        .map(wire::mapping_from_json)
        .collect::<Result<Vec<_>>>()
        .ok()
}

/// The mappings a lookup returns: exact version first; unless `exact`, any version of the
/// program when that version is not registered (what rpcbind does for GETPORT/GETADDR).
fn pick(ms: &[Mapping], program: u32, version: u32, netid: &str, exact: bool) -> Vec<Mapping> {
    let of = |v: Option<u32>| -> Vec<Mapping> {
        ms.iter()
            .filter(|m| {
                m.program == program
                    && v.is_none_or(|v| m.version == v)
                    && (netid.is_empty() || m.netid == netid)
            })
            .cloned()
            .collect()
    };
    let hit = of(Some(version));
    if hit.is_empty() && !exact {
        of(None)
    } else {
        hit
    }
}

async fn register(
    ctx: &SpawnContext,
    conn: Option<ConnectionId>,
    data: Value,
    op: &str,
) -> Accepted {
    match ask(ctx, conn, &actions::REGISTER_EVENT, data, op).await {
        Some((name, _)) if name == actions::ACCEPT => {
            Accepted::Success(Writer::default().u32(1).0.clone())
        }
        Some((name, _)) if name == actions::REJECT => {
            Accepted::Success(Writer::default().u32(0).0.clone())
        }
        Some(_) => {
            outcome(ctx, op, "fail_closed_invalid_reply");
            Accepted::SystemErr
        }
        None => Accepted::SystemErr,
    }
}

/// Answer one RPC message; None when there is nothing to answer (a reply, or no xid).
pub async fn handle(
    ctx: &SpawnContext,
    conn: Option<ConnectionId>,
    msg: &[u8],
    transport: &str,
    local: IpAddr,
) -> Option<Vec<u8>> {
    let call = match wire::parse_call(msg) {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(_) => {
            return wire::xid_of(msg).map(|x| wire::accepted_reply(x, &Accepted::GarbageArgs))
        }
    };
    if call.rpc_version != 2 {
        return Some(wire::rpc_mismatch_reply(call.xid));
    }
    let result = if call.program != wire::PROGRAM {
        Accepted::ProgUnavail
    } else if !(wire::LOW_VERSION..=wire::HIGH_VERSION).contains(&call.version) {
        Accepted::ProgMismatch(wire::LOW_VERSION, wire::HIGH_VERSION)
    } else {
        let credentials = wire::credential_json(call.cred_flavor, call.cred);
        match procedure(ctx, conn, &call, transport, local, credentials).await {
            Ok(a) => a,
            Err(_) => Accepted::GarbageArgs,
        }
    };
    Some(wire::accepted_reply(call.xid, &result))
}

async fn procedure(
    ctx: &SpawnContext,
    conn: Option<ConnectionId>,
    call: &wire::Call<'_>,
    transport: &str,
    local: IpAddr,
    credentials: Value,
) -> Result<Accepted> {
    let v = call.version;
    let mut r = Reader::new(call.args);
    let base =
        |procedure: &str| json!({"transport": transport, "rpc_version": v, "procedure": procedure});
    Ok(match (v, call.procedure) {
        (_, 0) => Accepted::Success(Vec::new()),
        // PMAP v2
        (2, 1 | 2) => {
            let (program, version, prot, port) = wire::read_pmap(&mut r)?;
            let set = call.procedure == 1;
            let Some(netid) = wire::netid_of(prot) else {
                // Not TCP or UDP: nothing to register.
                return Ok(Accepted::Success(Writer::default().u32(0).0.clone()));
            };
            let mut data = json!({"transport": transport, "rpc_version": v,
                "operation": if set { "set" } else { "unset" }, "program": program,
                "program_name": wire::program_name(program), "program_version": version,
                "protocol": netid, "credentials": credentials});
            if set {
                data["port"] = json!(port);
            }
            register(ctx, conn, data, if set { "set" } else { "unset" }).await
        }
        (2, 3) => {
            let (program, version, prot, _) = wire::read_pmap(&mut r)?;
            let netid = wire::netid_of(prot).unwrap_or("");
            let mut data = base("getport");
            data["program"] = json!(program);
            data["program_name"] = json!(wire::program_name(program));
            data["program_version"] = json!(version);
            data["protocol"] = json!(netid);
            match mappings(ctx, conn, data, "getport").await {
                None => Accepted::SystemErr,
                Some(ms) => {
                    let port = if netid.is_empty() {
                        0
                    } else {
                        pick(&ms, program, version, netid, false)
                            .first()
                            .map(|m| u32::from(m.port))
                            .unwrap_or(0)
                    };
                    Accepted::Success(Writer::default().u32(port).0.clone())
                }
            }
        }
        (2, 4) => match mappings(ctx, conn, base("dump"), "dump").await {
            None => Accepted::SystemErr,
            Some(ms) => Accepted::Success(wire::pmaplist(&ms)),
        },
        // RPCBIND v3 and v4
        (3 | 4, 1 | 2) => {
            let b = wire::read_rpcb(&mut r)?;
            let set = call.procedure == 1;
            let mut data = json!({"transport": transport, "rpc_version": v,
                "operation": if set { "set" } else { "unset" }, "program": b.program,
                "program_name": wire::program_name(b.program), "program_version": b.version,
                "protocol": b.netid, "owner": b.owner, "credentials": credentials});
            if set {
                if wire::protocol_of(&b.netid).is_none() {
                    return Ok(Accepted::Success(Writer::default().u32(0).0.clone()));
                }
                let addr = wire::parse_uaddr(&b.addr)?;
                data["port"] = json!(addr.port());
                data["address"] = json!(b.addr);
            }
            register(ctx, conn, data, if set { "set" } else { "unset" }).await
        }
        (3 | 4, 3) | (4, 9) => {
            let b = wire::read_rpcb(&mut r)?;
            let exact = call.procedure == 9;
            let procedure = if exact { "getversaddr" } else { "getaddr" };
            let mut data = base(procedure);
            data["program"] = json!(b.program);
            data["program_name"] = json!(wire::program_name(b.program));
            data["program_version"] = json!(b.version);
            data["protocol"] = json!(b.netid);
            match mappings(ctx, conn, data, procedure).await {
                None => Accepted::SystemErr,
                Some(ms) => {
                    let netid = if b.netid.is_empty() {
                        transport
                    } else {
                        b.netid.as_str()
                    };
                    let addr = pick(&ms, b.program, b.version, netid, exact)
                        .first()
                        .map(|m| wire::uaddr(wire::host_for(&m.netid, local), m.port))
                        .unwrap_or_default();
                    Accepted::Success(Writer::default().string(&addr).0.clone())
                }
            }
        }
        (3 | 4, 4) => match mappings(ctx, conn, base("dump"), "dump").await {
            None => Accepted::SystemErr,
            Some(ms) => Accepted::Success(wire::rpcblist(&ms, local)),
        },
        (3 | 4, 6) => {
            let now = crate::utils::clock::SystemTime::now()
                .duration_since(crate::utils::clock::UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            Accepted::Success(Writer::default().u32(now).0.clone())
        }
        (4, 11) => {
            let b = wire::read_rpcb(&mut r)?;
            let mut data = base("getaddrlist");
            data["program"] = json!(b.program);
            data["program_name"] = json!(wire::program_name(b.program));
            data["program_version"] = json!(b.version);
            data["protocol"] = json!(b.netid);
            match mappings(ctx, conn, data, "getaddrlist").await {
                None => Accepted::SystemErr,
                Some(ms) => Accepted::Success(wire::entry_list(
                    &pick(&ms, b.program, b.version, "", true),
                    local,
                )),
            }
        }
        _ => Accepted::ProcUnavail,
    })
}
