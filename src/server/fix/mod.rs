//! FIX acceptor. Rust owns framing, the Logon handshake rules, the session layer and its timers;
//! the handler admits each counterparty and answers each application message.
pub mod actions;
pub mod codec;
pub mod dict;
pub mod session;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::Result;
use codec::{Frame, Message};
use serde_json::{json, Value};
use session::Session;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_COMP_ID: &str = "NETGET";
pub const DEFAULT_BEGIN_STRING: &str = "FIX.4.4";
pub const LOGON_TIMEOUT: Duration = Duration::from_secs(10);
pub const BEGIN_STRINGS: &[&str] = &["FIX.4.0", "FIX.4.1", "FIX.4.2", "FIX.4.3", "FIX.4.4"];
/// How long to wait for the counterparty's Logout after sending ours.
pub const LOGOUT_WAIT: Duration = Duration::from_secs(5);
/// HeartBtInt 0 means no heartbeats; the timers then run this rarely.
const NO_HEARTBEAT: Duration = Duration::from_secs(86_400);

struct Shared {
    ctx: SpawnContext,
    comp_id: String,
    begin: String,
    logon_timeout: Duration,
}

pub fn comp_id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_graphic())
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str, d: &str| -> Result<String> {
        Ok(p.map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| d.to_owned()))
    };
    let comp_id = s("sender_comp_id", DEFAULT_COMP_ID)?;
    anyhow::ensure!(
        comp_id_ok(&comp_id),
        "sender_comp_id is 1 to 64 printable characters without spaces"
    );
    let begin = s("begin_string", DEFAULT_BEGIN_STRING)?;
    anyhow::ensure!(
        BEGIN_STRINGS.contains(&begin.as_str()),
        "begin_string is one of {BEGIN_STRINGS:?}"
    );
    let logon_timeout = p
        .map(|p| p.get_optional_u64("logon_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(LOGON_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=120).contains(&logon_timeout),
        "logon_timeout_secs must be 1..=120"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("FIX acceptor {comp_id} ({begin}) on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        comp_id,
        begin,
        logon_timeout: Duration::from_secs(logon_timeout),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) =
                match accept_bounded(&listener, &limiter, b"", "FIX", Some(&shared.ctx.status_tx))
                    .await
                {
                    Ok(v) => v,
                    Err(_) => break,
                };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
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
            let child = shared.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = connection(&child, id, stream).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("FIX connection {id}: {e:#}"));
                    }
                    child
                        .ctx
                        .state
                        .remove_peer_handle(server_id, id.as_u32())
                        .await;
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("FIX connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// Ask the handler; every answer it gave, or the fail-closed reason.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Vec<Value>, anyhow::Error> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::FixProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, operation, "fail_closed_invalid_reply");
        anyhow::bail!("the handler's answer was invalid");
    }
    let mut answers = Vec::new();
    let mut pending: Vec<ActionResult> = result.protocol_results.into_iter().rev().collect();
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items.into_iter().rev()),
            _ => {}
        }
    }
    if answers.is_empty() {
        outcome(ctx, id, operation, "model_silent");
        anyhow::bail!("the handler gave no answer");
    }
    Ok(answers)
}

struct Conn<'a> {
    shared: &'a Shared,
    id: ConnectionId,
    r: tokio::io::ReadHalf<TcpStream>,
    w: tokio::io::WriteHalf<TcpStream>,
    buf: Vec<u8>,
}

impl Conn<'_> {
    async fn write(&mut self, chunks: &[Vec<u8>]) -> Result<()> {
        for c in chunks.iter().filter(|c| !c.is_empty()) {
            self.w.write_all(c).await?;
            self.shared
                .ctx
                .state
                .update_connection_stats(
                    self.shared.ctx.server_id,
                    self.id,
                    None,
                    Some(c.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
        Ok(())
    }

    /// The next whole message, skipping garbled input; `None` on EOF.
    async fn next(&mut self) -> Result<Option<(String, Message)>> {
        loop {
            match codec::frame(&self.buf) {
                Frame::Message {
                    begin,
                    message,
                    consumed,
                } => {
                    self.buf.drain(..consumed);
                    self.shared
                        .ctx
                        .state
                        .update_connection_stats(
                            self.shared.ctx.server_id,
                            self.id,
                            Some(consumed as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;
                    return Ok(Some((begin, message)));
                }
                Frame::Garbled { consumed, reason } => {
                    self.buf.drain(..consumed);
                    Log::new(Some(&self.shared.ctx.status_tx)).warn(format!(
                        "FIX connection {}: ignored garbled input ({reason})",
                        self.id
                    ));
                }
                Frame::Incomplete => {
                    anyhow::ensure!(
                        self.buf.len() <= codec::MAX_BODY + 64,
                        "unframed input over {} bytes",
                        codec::MAX_BODY
                    );
                    let mut chunk = [0u8; 8192];
                    let n = self.r.read(&mut chunk).await?;
                    if n == 0 {
                        return Ok(None);
                    }
                    self.buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
}

async fn connection(shared: &Shared, id: ConnectionId, stream: TcpStream) -> Result<()> {
    let ctx = &shared.ctx;
    let (r, w) = tokio::io::split(stream);
    let mut c = Conn {
        shared,
        id,
        r,
        w,
        buf: Vec::new(),
    };
    // The Logon phase: the first message must be a valid Logon (FIX 4.4 vol 2, "Logon").
    let Ok(first) = tokio::time::timeout(shared.logon_timeout, c.next()).await else {
        outcome(ctx, id, "logon", "protocol_refusal");
        return Ok(());
    };
    let Some((begin, logon)) = first? else {
        return Ok(());
    };
    if logon.msg_type() != "A" || begin != shared.begin {
        outcome(ctx, id, "logon", "protocol_refusal");
        anyhow::bail!(
            "the first message was {} {}, not a {} Logon; disconnected",
            begin,
            logon.msg_type(),
            shared.begin
        );
    }
    let peer = logon.get(49).unwrap_or_default().to_owned();
    let heartbeat = logon
        .get(108)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|h| *h <= 3600);
    let refusal = if !comp_id_ok(&peer) {
        Some("SenderCompID missing or invalid")
    } else if logon.get(56) != Some(shared.comp_id.as_str()) {
        Some("TargetCompID is not this acceptor")
    } else if heartbeat.is_none() {
        Some("HeartBtInt must be 0 to 3600")
    } else if logon.get(98).unwrap_or("0") != "0" {
        Some("only EncryptMethod 0 is supported")
    } else if logon.seq().is_none() {
        Some("MsgSeqNum missing")
    } else {
        None
    };
    let hb = heartbeat.unwrap_or(30);
    let mut session = Session::new(
        &shared.begin,
        &shared.comp_id,
        if peer.is_empty() { "UNKNOWN" } else { &peer },
        if hb == 0 {
            NO_HEARTBEAT
        } else {
            Duration::from_secs(hb)
        },
    );
    if let Some(text) = refusal {
        outcome(ctx, id, "logon", "protocol_refusal");
        let bytes = session.logout(Some(text));
        c.write(&[bytes]).await?;
        return Ok(());
    }
    let reset = logon.get(141) == Some("Y");
    let event = Event::new(
        &actions::LOGON_EVENT,
        json!({"sender_comp_id": peer, "target_comp_id": shared.comp_id, "heartbeat_secs": hb, "reset_seq_num": reset, "username": logon.get(553), "password": logon.get(554)}),
    );
    match ask(shared, id, event, "logon")
        .await
        .map(|mut a| a.remove(0))
    {
        Ok(a) if a["type"] == "fix_accept_logon" => outcome(ctx, id, "logon", "model_answer"),
        Ok(a) if a["type"] == "fix_reject_logon" => {
            outcome(ctx, id, "logon", "model_reject");
            let bytes = session.logout(a["text"].as_str());
            c.write(&[bytes]).await?;
            return Ok(());
        }
        other => {
            if let Ok(a) = other {
                outcome(ctx, id, "logon", "fail_closed_invalid_reply");
                Log::new(Some(&ctx.status_tx)).warn(format!(
                    "FIX connection {id}: {} does not answer a Logon",
                    a["type"]
                ));
            }
            let bytes = session.logout(Some("logon cannot be processed now"));
            c.write(&[bytes]).await?;
            return Ok(());
        }
    }
    session.accept_logon(logon.seq().unwrap_or(1));
    let mut reply = vec![(98, "0".to_owned()), (108, hb.to_string())];
    if reset {
        reply.push((141, "Y".into()));
    }
    let bytes = session.encode("A", &reply)?;
    c.write(&[bytes]).await?;
    let mut commands =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    session_loop(&mut c, &mut session, &mut commands).await
}

async fn session_loop(
    c: &mut Conn<'_>,
    session: &mut Session,
    commands: &mut mpsc::Receiver<ClientCommand>,
) -> Result<()> {
    let ctx = &c.shared.ctx;
    let id = c.id;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut closing: Option<Instant> = None;
    loop {
        if closing.is_some_and(|t| t.elapsed() >= LOGOUT_WAIT) {
            return Ok(());
        }
        enum Wake {
            Message(Option<Message>),
            Tick,
            Command(Option<ClientCommand>),
        }
        let wake = tokio::select! {
            m = c.next() => Wake::Message(m?.map(|(_, m)| m)),
            _ = tick.tick() => Wake::Tick,
            cmd = commands.recv(), if closing.is_none() => Wake::Command(cmd),
        };
        let message = match wake {
            Wake::Message(Some(m)) => m,
            Wake::Message(None) => return Ok(()),
            Wake::Tick => {
                let (send, close) = session.tick();
                c.write(&send).await?;
                if let Some(reason) = close {
                    Log::new(Some(&ctx.status_tx)).warn(format!("FIX connection {id}: {reason}"));
                    return Ok(());
                }
                continue;
            }
            Wake::Command(None) => continue,
            Wake::Command(Some(cmd)) => {
                let action = cmd.action.clone();
                let outcome = injected(c, session, &action).await;
                if session.logout_sent {
                    closing.get_or_insert_with(Instant::now);
                }
                ctx.state
                    .record_access_log(
                        crate::state::AccessLogOwner::Server(ctx.server_id.as_u32()),
                        "FIX",
                        Some(id.as_u32()),
                        "injected_action",
                        json!({"type": action["type"], "msg_type": action["msg_type"]}),
                        vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                    )
                    .await;
                let _ = cmd.reply_tx.send(Ok(outcome));
                continue;
            }
        };
        let step = session.on_message(message);
        c.write(&step.send).await?;
        if let Some(reason) = step.close {
            Log::new(Some(&ctx.status_tx)).info(format!("FIX connection {id}: {reason}"));
            return Ok(());
        }
        let Some(app) = step.app else { continue };
        if closing.is_some() {
            continue;
        }
        let msg_type = app.msg_type().to_owned();
        let seq = app.seq().unwrap_or(0);
        let fields: Vec<Value> = session::body(&app)
            .into_iter()
            .map(|(tag, value)| json!({"tag": tag, "name": dict::field_name(tag), "value": value}))
            .collect();
        let event = Event::new(
            &actions::MESSAGE_EVENT,
            json!({"msg_type": msg_type, "msg_type_name": dict::message_name(&msg_type), "seq": seq, "sender_comp_id": session.target, "fields": fields}),
        );
        match ask(c.shared, id, event, "message").await {
            Ok(answers) => {
                outcome(
                    ctx,
                    id,
                    "message",
                    if answers.iter().any(|a| a["type"] == "fix_reject") {
                        "model_reject"
                    } else {
                        "model_answer"
                    },
                );
                for a in answers {
                    apply(c, session, &a, seq, &msg_type).await?;
                }
                if session.logout_sent {
                    closing.get_or_insert_with(Instant::now);
                }
            }
            Err(e) => {
                let text = crate::utils::WireFailure::classify(&e).text();
                let bytes = business_reject(session, seq, &msg_type, 4, text)?;
                c.write(&[bytes]).await?;
            }
        }
    }
}

fn business_reject(
    session: &mut Session,
    ref_seq: u32,
    ref_type: &str,
    reason: u32,
    text: &str,
) -> Result<Vec<u8>> {
    session.encode(
        "j",
        &[
            (45, ref_seq.to_string()),
            (372, ref_type.to_owned()),
            (380, reason.to_string()),
            (58, text.to_owned()),
        ],
    )
}

/// Apply one handler answer to an application message.
async fn apply(
    c: &mut Conn<'_>,
    session: &mut Session,
    a: &Value,
    ref_seq: u32,
    ref_type: &str,
) -> Result<()> {
    let bytes = match a["type"].as_str() {
        Some("fix_send") => {
            let msg_type = actions::app_msg_type(a)?;
            session.encode(&msg_type, &actions::body_fields(a)?)?
        }
        Some("fix_reject") => {
            let reason = actions::BUSINESS_REJECT
                .iter()
                .find(|(n, _)| a["reason"] == *n)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            business_reject(
                session,
                ref_seq,
                ref_type,
                reason,
                a["text"].as_str().unwrap_or_default(),
            )?
        }
        Some("fix_logout") => session.logout(a["text"].as_str()),
        _ => return Ok(()),
    };
    c.write(&[bytes]).await
}

/// An action injected from the dashboard or MCP: fix_send, fix_logout or disconnect.
async fn injected(c: &mut Conn<'_>, session: &mut Session, a: &Value) -> ClientSendOutcome {
    let checked = match a["type"].as_str() {
        Some("fix_send" | "fix_logout") => actions::check_answer(a),
        Some("disconnect") => Ok(()),
        _ => Err(anyhow::anyhow!(
            "a FIX session accepts fix_send, fix_logout or disconnect"
        )),
    };
    if let Err(e) = checked {
        return ClientSendOutcome::Rejected {
            error: e.to_string(),
        };
    }
    let bytes = match a["type"].as_str() {
        Some("fix_send") => match actions::app_msg_type(a)
            .and_then(|t| session.encode(&t, &actions::body_fields(a)?))
        {
            Ok(b) => b,
            Err(e) => {
                return ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }
            }
        },
        Some("fix_logout") => session.logout(a["text"].as_str()),
        _ => session.logout(None),
    };
    let n = bytes.len();
    match c.write(&[bytes]).await {
        Ok(()) if a["type"] == "disconnect" => ClientSendOutcome::Disconnected,
        Ok(()) => ClientSendOutcome::Sent { bytes_sent: n },
        Err(e) => ClientSendOutcome::Rejected {
            error: e.to_string(),
        },
    }
}
