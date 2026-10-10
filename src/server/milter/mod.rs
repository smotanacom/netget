//! Milter (mail filter) server. Rust owns the packets, option negotiation, macros and the
//! collection of headers and body; the handler decides at each SMTP stage and may modify the
//! message at its end.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{ensure, Result};
use serde_json::{json, Map, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::net::{TcpListener, TcpStream};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
/// Most macros remembered for one connection.
pub const MAX_MACROS: usize = 256;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let secs = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    let idle = Duration::from_secs(secs);
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Milter listening on {addr}"));
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"",
                "Milter",
                Some(&ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: addr,
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
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, id, socket, idle).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("Milter connection {id} ended: {e:#}"));
                    }
                    child
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(addr)
}

#[derive(Default)]
struct Message {
    sender: String,
    recipients: Vec<String>,
    headers: Vec<Value>,
    body: Vec<u8>,
    truncated: bool,
}

struct Session {
    actions: u32,
    macros: Map<String, Value>,
    connection: Value,
    message: Message,
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, stage: &str, decision: &str) {
    let line = format!("Milter connection {id} stage={stage} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed") {
        log.error(line)
    } else {
        log.info(line)
    }
}

/// The handler's actions for one stage, or `None` when it failed (the caller tempfails).
async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    stage: &str,
) -> Option<Vec<Value>> {
    match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::MilterProtocol,
    )
    .await
    {
        Ok(r) if r.failures.is_empty() => {
            let mut out = Vec::new();
            let mut stack = r.protocol_results;
            while let Some(item) = stack.pop() {
                match item {
                    ActionResult::Custom { name, data } if name.starts_with("milter_") => {
                        out.push(data)
                    }
                    ActionResult::Multiple(items) => stack.extend(items),
                    _ => {}
                }
            }
            out.reverse();
            Some(out)
        }
        Ok(_) => {
            outcome(ctx, id, stage, "fail_closed_invalid_reply");
            None
        }
        Err(_) => {
            outcome(ctx, id, stage, "fail_closed_llm_error");
            None
        }
    }
}

const DECISIONS: [&str; 6] = [
    "milter_continue",
    "milter_accept",
    "milter_reject",
    "milter_tempfail",
    "milter_discard",
    "milter_reply",
];

/// Write the stage's replies: modifications (end of message only, and only those the MTA
/// allowed), then one decision. Returns the decision's name.
async fn answer<W: tokio::io::AsyncWrite + Unpin>(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut W,
    s: &Session,
    stage: &str,
    answers: Option<Vec<Value>>,
    end_of_message: bool,
) -> Result<&'static str> {
    let Some(answers) = answers else {
        wire::write(w, wire::R_TEMPFAIL, &[]).await?;
        return Ok("tempfail");
    };
    let log = Log::new(Some(&ctx.status_tx));
    let mut decision = None;
    for a in &answers {
        let name = a["type"].as_str().unwrap_or_default();
        if DECISIONS.contains(&name) {
            decision = Some(a.clone());
            continue;
        }
        if !end_of_message {
            log.warn(format!(
                "Milter: {name} is only possible at end of message; ignored at {stage}"
            ));
            continue;
        }
        let needed = match name {
            "milter_add_header" => wire::F_ADDHDRS,
            "milter_change_header" => wire::F_CHGHDRS,
            "milter_add_rcpt" => wire::F_ADDRCPT,
            "milter_del_rcpt" => wire::F_DELRCPT,
            "milter_replace_body" => wire::F_CHGBODY,
            "milter_quarantine" => wire::F_QUARANTINE,
            _ => 0,
        };
        if s.actions & needed == 0 {
            log.warn(format!("Milter: the MTA did not allow {name}; not sent"));
            continue;
        }
        let (cmd, data) = actions::reply_packet(a)?;
        wire::write(w, cmd, &data).await?;
    }
    let (cmd, data) = match &decision {
        Some(d) => actions::reply_packet(d)?,
        // Saying nothing lets the message through this stage.
        None if end_of_message => (wire::R_ACCEPT, vec![]),
        None => (wire::R_CONTINUE, vec![]),
    };
    wire::write(w, cmd, &data).await?;
    let name = wire::reply_name(cmd);
    outcome(
        ctx,
        id,
        stage,
        if decision.is_some() {
            "model_answer"
        } else {
            "model_silent_continue"
        },
    );
    Ok(name)
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    idle: Duration,
) -> Result<()> {
    let (mut r, mut w) = tokio::io::split(socket);
    let mut s = Session {
        actions: 0,
        macros: Map::new(),
        connection: json!({}),
        message: Message::default(),
    };
    while let Some((cmd, data)) = wire::read_packet(&mut r, idle).await? {
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(data.len() as u64 + 5),
                None,
                Some(1),
                None,
            )
            .await;
        let stage_event = |e: &'static std::sync::LazyLock<crate::protocol::EventType>,
                           mut v: Value,
                           s: &Session| {
            v["connection"] = s.connection.clone();
            v["macros"] = Value::Object(s.macros.clone());
            Event::new(e, v)
        };
        match cmd {
            wire::C_OPTNEG => {
                let (version, actions, _protocol) = wire::parse_optneg(&data)?;
                ensure!(version >= 2, "milter protocol version {version} is too old");
                s.actions = actions & wire::ALL_ACTIONS;
                wire::write(
                    &mut w,
                    wire::C_OPTNEG,
                    &wire::optneg(version.min(wire::VERSION), s.actions, 0),
                )
                .await?;
            }
            wire::C_MACRO => {
                let fields = wire::strings(data.get(1..).unwrap_or_default());
                for pair in fields.chunks(2) {
                    if let [k, v] = pair {
                        if s.macros.len() < MAX_MACROS || s.macros.contains_key(k) {
                            s.macros.insert(k.clone(), json!(v));
                        }
                    }
                }
            }
            wire::C_CONNECT => {
                let (hostname, family, port, address) = wire::parse_connect(&data)?;
                s.connection = json!({"hostname": hostname, "family": family.to_string(), "address": address, "port": port});
                let e = stage_event(
                    &actions::CONNECT_EVENT,
                    json!({"hostname": hostname, "address": address, "port": port}),
                    &s,
                );
                let a = ask(ctx, id, e, "connect").await;
                answer(ctx, id, &mut w, &s, "connect", a, false).await?;
            }
            wire::C_HELO => {
                let helo = wire::strings(&data).into_iter().next().unwrap_or_default();
                s.connection["helo"] = json!(helo);
                let e = stage_event(&actions::HELO_EVENT, json!({"helo": helo}), &s);
                let a = ask(ctx, id, e, "helo").await;
                answer(ctx, id, &mut w, &s, "helo", a, false).await?;
            }
            wire::C_MAIL => {
                let mut f = wire::strings(&data);
                ensure!(!f.is_empty(), "MAIL without a sender");
                let sender = f.remove(0);
                s.message = Message {
                    sender: sender.clone(),
                    ..Message::default()
                };
                let e = stage_event(
                    &actions::MAIL_EVENT,
                    json!({"sender": sender, "esmtp_args": f}),
                    &s,
                );
                let a = ask(ctx, id, e, "mail").await;
                answer(ctx, id, &mut w, &s, "mail", a, false).await?;
            }
            wire::C_RCPT => {
                let mut f = wire::strings(&data);
                ensure!(!f.is_empty(), "RCPT without a recipient");
                let rcpt = f.remove(0);
                let e = stage_event(
                    &actions::RCPT_EVENT,
                    json!({"recipient": rcpt, "esmtp_args": f}),
                    &s,
                );
                let a = ask(ctx, id, e, "rcpt").await;
                if answer(ctx, id, &mut w, &s, "rcpt", a, false).await? == "continue" {
                    s.message.recipients.push(rcpt);
                }
            }
            wire::C_HEADER => {
                let f = wire::strings(&data);
                if s.message.headers.len() < wire::MAX_HEADERS {
                    s.message.headers.push(json!({"name": f.first().cloned().unwrap_or_default(), "value": f.get(1).cloned().unwrap_or_default()}));
                }
                wire::write(&mut w, wire::R_CONTINUE, &[]).await?;
            }
            wire::C_BODY => {
                let room = wire::MAX_BODY.saturating_sub(s.message.body.len());
                s.message.truncated |= data.len() > room;
                s.message
                    .body
                    .extend_from_slice(&data[..data.len().min(room)]);
                wire::write(&mut w, wire::R_CONTINUE, &[]).await?;
            }
            wire::C_BODYEOB => {
                if !data.is_empty() {
                    let room = wire::MAX_BODY.saturating_sub(s.message.body.len());
                    s.message.truncated |= data.len() > room;
                    s.message
                        .body
                        .extend_from_slice(&data[..data.len().min(room)]);
                }
                let m = &s.message;
                let e = stage_event(
                    &actions::MESSAGE_EVENT,
                    json!({"sender": m.sender, "recipients": m.recipients, "headers": m.headers,
                           "body": String::from_utf8_lossy(&m.body), "body_truncated": m.truncated}),
                    &s,
                );
                let a = ask(ctx, id, e, "message").await;
                answer(ctx, id, &mut w, &s, "message", a, true).await?;
                s.message = Message::default();
            }
            wire::C_DATA | wire::C_EOH | wire::C_UNKNOWN => {
                wire::write(&mut w, wire::R_CONTINUE, &[]).await?
            }
            wire::C_ABORT => s.message = Message::default(),
            wire::C_QUIT_NC => {
                s.message = Message::default();
                s.macros.clear();
                s.connection = json!({});
            }
            wire::C_QUIT => return Ok(()),
            other => anyhow::bail!("unknown milter command {:?}", other as char),
        }
    }
    Ok(())
}
