//! Steam/Source server query (A2S) over UDP. Rust owns the challenge exchange, encoding and
//! splitting; handlers decide what the server reports.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use anyhow::Result;
use serde_json::{json, Value};
use std::{hash::BuildHasher, net::SocketAddr, sync::Arc};
use tokio::net::UdpSocket;
use wire::Kind;

pub const DEFAULT_INFO_CHALLENGE: bool = false;
/// Queries handled at once; datagrams beyond this are dropped.
pub const MAX_IN_FLIGHT: usize = 64;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let info_challenge = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_bool("info_challenge"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_INFO_CHALLENGE);
    let socket = Arc::new(UdpSocket::bind(ctx.legacy_listen_addr()).await?);
    let addr = socket.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("A2S listening on udp {addr}"));
    let child = ctx.clone();
    let task = tokio::spawn(serve(child, socket, info_challenge));
    ctx.state.register_server_task(ctx.server_id, task).await;
    Ok(addr)
}

/// A per-server keyed hash of the client's address: no state per client, unforgeable
/// without receiving at that address. Never 0xFFFFFFFF, which asks for a challenge.
fn challenge_for(key: &std::collections::hash_map::RandomState, peer: SocketAddr) -> u32 {
    let value = key.hash_one(peer) as u32;
    if value == wire::NO_CHALLENGE {
        0x5A5A_5A5A
    } else {
        value
    }
}

fn outcome(ctx: &SpawnContext, peer: SocketAddr, query: &str, decision: &str) {
    let summary = format!("A2S datagram from {peer} operation={query} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The handler's encoded answer; `None` when it refused, failed or answered the wrong query.
async fn answer(ctx: &SpawnContext, kind: Kind, peer: SocketAddr) -> Option<Vec<u8>> {
    let event = Event::new(
        &actions::QUERY_EVENT,
        json!({"query": kind.name(), "remote_addr": peer.to_string()}),
    );
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        None,
        &event,
        &actions::A2sProtocol,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            outcome(ctx, peer, kind.name(), "fail_closed_llm_error");
            return None;
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, peer, kind.name(), "fail_closed_invalid_reply");
        return None;
    }
    let mut found: Option<(String, Value)> = None;
    let mut pending = result.protocol_results;
    while let Some(item) = pending.pop() {
        match item {
            ActionResult::Custom { name, data } if name.starts_with("a2s_") => {
                if found.is_some() {
                    outcome(ctx, peer, kind.name(), "fail_closed_invalid_reply");
                    return None;
                }
                found = Some((name, data));
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    let Some((name, data)) = found else {
        outcome(ctx, peer, kind.name(), "model_silent");
        return None;
    };
    if name == "a2s_refuse" {
        outcome(ctx, peer, kind.name(), "model_reject");
        return None;
    }
    if name != actions::answer_name(kind) {
        outcome(ctx, peer, kind.name(), "fail_closed_invalid_reply");
        return None;
    }
    let encoded = match kind {
        Kind::Info => wire::encode_info(&data),
        Kind::Players => wire::encode_players(&data),
        Kind::Rules => wire::encode_rules(&data),
    };
    match encoded {
        Ok(payload) => {
            outcome(ctx, peer, kind.name(), "model_answer");
            Some(payload)
        }
        Err(_) => {
            outcome(ctx, peer, kind.name(), "fail_closed_invalid_reply");
            None
        }
    }
}

async fn serve(ctx: SpawnContext, socket: Arc<UdpSocket>, info_challenge: bool) {
    let key = std::collections::hash_map::RandomState::new();
    let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let mut buf = vec![0u8; wire::MAX_REQUEST + 1];
    let mut split_id: u32 = 1;
    loop {
        let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let (kind, challenge) = match wire::parse_request(&buf[..n]) {
            Ok(request) => request,
            Err(e) => {
                Log::new(Some(&ctx.status_tx))
                    .debug(format!("A2S dropped a datagram from {peer}: {e}"));
                continue;
            }
        };
        let expected = challenge_for(&key, peer);
        let needs_challenge = kind != Kind::Info || info_challenge;
        if needs_challenge && challenge != Some(expected) {
            let _ = socket
                .send_to(&wire::encode_challenge(expected), peer)
                .await;
            continue;
        }
        let Ok(permit) = in_flight.clone().try_acquire_owned() else {
            Log::new(Some(&ctx.status_tx)).warn(format!(
                "A2S dropped a query from {peer}: too many in flight"
            ));
            continue;
        };
        let id = split_id;
        split_id = split_id.wrapping_add(1) & 0x7FFF_FFFF;
        let child = ctx.clone();
        let reply = socket.clone();
        ctx.state
            .spawn_server_task(ctx.server_id, async move {
                let _permit = permit;
                let Some(payload) = answer(&child, kind, peer).await else {
                    return;
                };
                match wire::packetize(&payload, id) {
                    Ok(packets) => {
                        for packet in packets {
                            let _ = reply.send_to(&packet, peer).await;
                        }
                    }
                    Err(e) => Log::new(Some(&child.status_tx))
                        .warn(format!("A2S answer to {peer} not sent: {e}")),
                }
            })
            .await;
    }
}
