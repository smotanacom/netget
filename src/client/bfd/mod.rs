//! BFD client: one session with one router, in the active role, run by the server's engine
//! (`src/server/bfd/runner.rs`). The model is asked when the session starts and whenever its
//! state changes; its answers (timers, AdminDown, back up, disconnect) go to the session, as
//! do actions injected through the client's command channel.
pub mod actions;

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::bfd::packet;
use crate::server::bfd::runner::{self, Input, Link, Note};
use crate::server::bfd::session::Session;
use crate::server::bfd::ttl;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::BfdClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::mpsc;

/// How many model turns may follow from one another before the chain stops.
pub const MAX_FOLLOWUP_DEPTH: u32 = 8;
const TURNS: usize = 64;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let multihop_param = params
        .map(|p| p.get_optional_bool("multihop"))
        .transpose()?
        .flatten();
    let remote = parse_remote(&ctx.remote_addr, multihop_param).await?;
    let multihop = multihop_param.unwrap_or(remote.port() == packet::MULTIHOP_PORT);
    let local_ip = match params
        .map(|p| p.get_optional_string("local_address"))
        .transpose()?
        .flatten()
    {
        Some(raw) => raw
            .parse::<IpAddr>()
            .with_context(|| format!("local_address {raw:?} is not an IP address"))?,
        None => route_source(remote).await?,
    };
    let timers = crate::server::bfd::actions::timers_from(params)?;
    let auth = crate::server::bfd::actions::auth_from(params)?;

    // The router sends to its neighbour's address at the BFD port, so that is where to listen;
    // SO_REUSEADDR lets a routing daemon's wildcard socket on the same port coexist.
    let rx = crate::server::socket_helpers::create_reusable_udp_socket(SocketAddr::new(
        local_ip,
        remote.port(),
    ))
    .await
    .with_context(|| format!("cannot listen on {local_ip}:{}", remote.port()))?;
    ttl::enable(&rx).context("asking for the TTL of received packets")?;
    let tx = Arc::new(runner::tx_socket(local_ip).await?);
    let local = tx.local_addr()?;
    let discr = loop {
        let d: u32 = rand::random();
        if d != 0 {
            break d;
        }
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "BFD client {} session {discr} with {remote} from {local} ({})",
        ctx.client_id,
        if multihop { "multihop" } else { "single-hop" }
    ));
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;

    let (inputs, inputs_rx) = mpsc::channel(64);
    let (notes_tx, notes) = mpsc::channel(64);
    let link = Link {
        socket: tx,
        dest: remote,
        label: format!("BFD client {} session {discr}", ctx.client_id),
    };
    let session = Session::new(discr, timers, auth, false);
    let run = tokio::spawn(runner::run(session, link, inputs_rx, notes_tx, None));
    let run_abort = run.abort_handle();
    ctx.state.register_client_task(ctx.client_id, run).await;

    let reader_inputs = inputs.clone();
    let reader = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from, ttl)) = ttl::recv(&rx, &mut buf).await {
            if from.ip() != remote.ip() {
                continue;
            }
            if !multihop && ttl.is_some_and(|t| t != 255) {
                tracing::warn!(
                    "BFD client dropped a packet from {from} with TTL {ttl:?} decision=dropped_ttl"
                );
                continue;
            }
            let packet = match packet::decode(&buf[..n]) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("BFD client dropped a datagram from {from}: {e:#}");
                    continue;
                }
            };
            if packet.your_discriminator != 0 && packet.your_discriminator != discr {
                continue;
            }
            let input = Input::Packet {
                bytes: buf[..n].to_vec(),
                packet,
            };
            if reader_inputs.send(input).await.is_err() {
                return;
            }
        }
    });
    let reader_abort = reader.abort_handle();
    ctx.state.register_client_task(ctx.client_id, reader).await;

    let (turn_tx, turns) = mpsc::channel::<(Event, u32)>(TURNS);
    let (answers_tx, answers) = mpsc::channel::<(Value, u32)>(TURNS);
    let _ = turn_tx.try_send((
        Event::new(
            &actions::STARTED_EVENT,
            json!({"peer": remote.to_string(), "local": SocketAddr::new(local_ip, remote.port()).to_string(),
                   "local_discriminator": discr, "multihop": multihop}),
        ),
        0,
    ));
    let dispatcher = tokio::spawn(run_turns(ctx.clone(), turns, answers_tx));
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    let control_ctx = ctx.clone();
    let peer = remote.ip().to_string();
    let control = tokio::spawn(async move {
        control(
            &control_ctx,
            &peer,
            &inputs,
            notes,
            turn_tx,
            answers,
            external,
        )
        .await;
        // Tell the router before going quiet (RFC 5880 §6.8.16): AdminDown, then stop.
        let (reply_tx, reply) = tokio::sync::oneshot::channel();
        let goodbye = Input::Action {
            action: json!({"type": "bfd_admin_down", "diag": "administratively_down"}),
            depth: MAX_FOLLOWUP_DEPTH,
            command: Some(ClientCommand {
                action: Value::Null,
                reply_tx,
            }),
        };
        if inputs.send(goodbye).await.is_ok() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), reply).await;
        }
        run_abort.abort();
        reader_abort.abort();
        dispatcher_abort.abort();
        control_ctx
            .state
            .update_client_status(control_ctx.client_id, ClientStatus::Disconnected)
            .await;
        control_ctx
            .state
            .remove_client_handle(control_ctx.client_id)
            .await;
        let _ = control_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, control).await;
    Ok(local)
}

/// `ip` or `ip:port`; the port defaults to the BFD port for the mode.
async fn parse_remote(raw: &str, multihop: Option<bool>) -> Result<SocketAddr> {
    let raw = raw.trim();
    anyhow::ensure!(!raw.is_empty(), "remote_addr is the router's address");
    if let Ok(ip) = raw.parse::<IpAddr>() {
        let port = if multihop == Some(true) {
            packet::MULTIHOP_PORT
        } else {
            packet::SINGLE_HOP_PORT
        };
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host(raw)
        .await
        .with_context(|| format!("cannot resolve {raw}"))?
        .next()
        .with_context(|| format!("{raw} resolved to no address"))
}

/// The source address the route to `remote` uses.
async fn route_source(remote: SocketAddr) -> Result<IpAddr> {
    let probe = tokio::net::UdpSocket::bind(if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    probe
        .connect(remote)
        .await
        .with_context(|| format!("no route to {remote}"))?;
    Ok(probe.local_addr()?.ip())
}

async fn control(
    ctx: &ConnectContext,
    peer: &str,
    inputs: &mpsc::Sender<Input>,
    mut notes: mpsc::Receiver<Note>,
    turns: mpsc::Sender<(Event, u32)>,
    mut answers: mpsc::Receiver<(Value, u32)>,
    mut external: mpsc::Receiver<ClientCommand>,
) {
    loop {
        tokio::select! {
            c = external.recv() => {
                let Some(command) = c else { return };
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "BFD",
                        None,
                        "injected_action",
                        command.action.clone(),
                        vec![],
                    )
                    .await;
                match BfdClientProtocol.execute_action(command.action.clone()) {
                    Ok(ClientActionResult::Disconnect) => {
                        crate::client::command_support::reply(command, Ok(ClientSendOutcome::Disconnected));
                        return;
                    }
                    Ok(_) => {
                        let action = command.action.clone();
                        let _ = inputs.send(Input::Action { action, depth: 0, command: Some(command) }).await;
                    }
                    Err(e) => crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected { error: e.to_string() }),
                    ),
                }
            }
            a = answers.recv() => {
                let Some((action, depth)) = a else { return };
                match BfdClientProtocol.execute_action(action.clone()) {
                    Ok(ClientActionResult::Disconnect) => return,
                    Ok(_) => {
                        let _ = inputs.send(Input::Action { action, depth, command: None }).await;
                    }
                    Err(e) => tracing::warn!("BFD client {} refused {}: {e:#}", ctx.client_id, action["type"]),
                }
            }
            n = notes.recv() => match n {
                None | Some(Note::Ended { .. }) => return,
                Some(Note::Changed { previous, snapshot, depth }) => {
                    let data = runner::state_event(peer, previous, &snapshot);
                    Log::new(Some(&ctx.status_tx)).info(format!(
                        "BFD client {}: {} -> {} ({})",
                        ctx.client_id,
                        previous.name(),
                        data["state"].as_str().unwrap_or_default(),
                        data["diag"].as_str().unwrap_or_default()
                    ));
                    if turns.try_send((Event::new(&actions::STATE_EVENT, data), depth)).is_err() {
                        tracing::warn!("BFD client {} dropped a state event: the model is {TURNS} behind decision=turn_queue_full", ctx.client_id);
                    }
                }
            },
        }
    }
}

async fn run_turns(
    ctx: ConnectContext,
    mut turns: mpsc::Receiver<(Event, u32)>,
    answers: mpsc::Sender<(Value, u32)>,
) {
    let protocol = BfdClientProtocol;
    while let Some((event, depth)) = turns.recv().await {
        if depth >= MAX_FOLLOWUP_DEPTH {
            tracing::warn!("BFD client {} not asking the model about {}: {depth} turns deep decision=followup_depth", ctx.client_id, event.id());
            continue;
        }
        let instruction = ctx
            .state
            .get_instruction_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        let memory = ctx
            .state
            .get_memory_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        match call_llm_for_client(
            &ctx.llm_client,
            &ctx.state,
            ctx.client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &protocol,
            &ctx.status_tx,
        )
        .await
        {
            Ok(result) => {
                if let Some(memory) = result.memory_updates {
                    ctx.state.set_memory_for_client(ctx.client_id, memory).await;
                }
                for action in result.actions {
                    if answers.send((action, depth + 1)).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => Log::new(Some(&ctx.status_tx)).warn(format!("BFD client handler: {e}")),
        }
    }
}
