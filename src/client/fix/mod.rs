//! FIX initiator over the acceptor's codec and session layer.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::fix::codec::{self, Frame, Message};
use crate::server::fix::session::{self, Session};
use crate::server::fix::{actions as server_actions, comp_id_ok, dict, BEGIN_STRINGS, LOGOUT_WAIT};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use crate::utils::clock::Instant;
pub use actions::FixClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_TARGET: &str = "EXCHANGE";
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);
const LOGON_WAIT: Duration = Duration::from_secs(10);

struct Wire {
    r: ReadHalf<TcpStream>,
    w: WriteHalf<TcpStream>,
    buf: Vec<u8>,
}

impl Wire {
    async fn write(&mut self, chunks: &[Vec<u8>]) -> Result<()> {
        for c in chunks.iter().filter(|c| !c.is_empty()) {
            self.w.write_all(c).await?;
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<Option<Message>> {
        loop {
            match codec::frame(&self.buf) {
                Frame::Message {
                    message, consumed, ..
                } => {
                    self.buf.drain(..consumed);
                    return Ok(Some(message));
                }
                Frame::Garbled { consumed, .. } => {
                    self.buf.drain(..consumed);
                }
                Frame::Incomplete => {
                    ensure!(
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

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str| -> Result<Option<String>> {
        Ok(p.map(|p| p.get_optional_string(k)).transpose()?.flatten())
    };
    let sender =
        s("sender_comp_id")?.unwrap_or_else(|| crate::server::fix::DEFAULT_COMP_ID.to_owned());
    let target = s("target_comp_id")?.unwrap_or_else(|| DEFAULT_TARGET.to_owned());
    ensure!(
        comp_id_ok(&sender) && comp_id_ok(&target),
        "CompIDs are 1 to 64 printable characters without spaces"
    );
    let begin =
        s("begin_string")?.unwrap_or_else(|| crate::server::fix::DEFAULT_BEGIN_STRING.to_owned());
    ensure!(
        BEGIN_STRINGS.contains(&begin.as_str()),
        "begin_string is one of {BEGIN_STRINGS:?}"
    );
    let hb = p
        .map(|p| p.get_optional_u64("heartbeat_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_HEARTBEAT.as_secs());
    ensure!(hb <= 3600, "heartbeat_secs is 0 to 3600");
    let reset = p
        .map(|p| p.get_optional_bool("reset_seq_num"))
        .transpose()?
        .flatten()
        .unwrap_or(true);
    let (username, password) = (s("username")?, s("password")?);
    for v in [&username, &password].into_iter().flatten() {
        ensure!(
            !v.is_empty() && v.len() <= 256 && !crate::utils::sanitize::has_controls(&v),
            "username and password are 1 to 256 printable characters"
        );
    }
    let stream = tokio::time::timeout(LOGON_WAIT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("FIX connect timed out")??;
    let local = stream.local_addr()?;
    let (r, w) = tokio::io::split(stream);
    let mut wire = Wire {
        r,
        w,
        buf: Vec::new(),
    };
    let mut session = Session::new(
        &begin,
        &sender,
        &target,
        if hb == 0 {
            Duration::from_secs(86_400)
        } else {
            Duration::from_secs(hb)
        },
    );
    let mut logon = vec![(98, "0".to_owned()), (108, hb.to_string())];
    if reset {
        logon.push((141, "Y".into()));
    }
    if let Some(u) = &username {
        logon.push((553, u.clone()));
    }
    if let Some(pw) = &password {
        logon.push((554, pw.clone()));
    }
    let bytes = session.encode("A", &logon)?;
    wire.write(&[bytes]).await?;
    let answer = tokio::time::timeout(LOGON_WAIT, wire.next())
        .await
        .context("no answer to the Logon")??
        .context("the acceptor closed the connection during logon")?;
    match answer.msg_type() {
        "A" => session.accept_logon(answer.seq().unwrap_or(1)),
        "5" => bail!(
            "logon refused: {}",
            answer.get(58).unwrap_or("no reason given")
        ),
        other => bail!("the acceptor answered the Logon with MsgType {other}"),
    }
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(64);
    event_tx.try_send(Event::new(&actions::LOGGED_ON_EVENT, json!({"sender_comp_id": sender, "target_comp_id": target, "heartbeat_secs": answer.get(108).and_then(|v| v.parse::<u64>().ok()).unwrap_or(hb)})))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = FixClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("FIX client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let reason = match run(
            &session_ctx,
            &mut wire,
            &mut session,
            external,
            internal_rx,
            &event_tx,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => format!("{e:#}"),
        };
        let _ = event_tx
            .send(Event::new(
                &actions::LOGGED_OUT_EVENT,
                json!({"reason": reason}),
            ))
            .await;
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
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

/// Run the session until it ends; the reason.
async fn run(
    ctx: &ConnectContext,
    wire: &mut Wire,
    session: &mut Session,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: &mpsc::Sender<Event>,
) -> Result<String> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut closing: Option<Instant> = None;
    enum Wake {
        Message(Option<Message>),
        Tick,
        Action(Value, Option<ClientCommand>),
        Idle,
    }
    loop {
        if closing.is_some_and(|t| t.elapsed() >= LOGOUT_WAIT) {
            return Ok("no Logout answer from the acceptor".into());
        }
        let wake = tokio::select! {
            m = wire.next() => Wake::Message(m?),
            _ = tick.tick() => Wake::Tick,
            c = external.recv(), if closing.is_none() => match c { Some(c) => Wake::Action(c.action.clone(), Some(c)), None => Wake::Idle },
            a = internal.recv(), if closing.is_none() => match a { Some(a) => Wake::Action(a, None), None => Wake::Idle },
        };
        match wake {
            Wake::Idle => {}
            Wake::Message(None) => return Ok("the acceptor closed the connection".into()),
            Wake::Tick => {
                let (send, close) = session.tick();
                wire.write(&send).await?;
                if let Some(reason) = close {
                    return Ok(reason);
                }
            }
            Wake::Action(action, command) => {
                let outcome = match FixClientProtocol.execute_action(action.clone()) {
                    Err(e) => ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                    Ok(ClientActionResult::Disconnect) => {
                        let bytes = session.logout(None);
                        let _ = wire.write(&[bytes]).await;
                        if let Some(c) = command {
                            crate::client::command_support::reply(
                                c,
                                Ok(ClientSendOutcome::Disconnected),
                            );
                        }
                        return Ok("disconnected".into());
                    }
                    Ok(_) => {
                        let bytes = match action["type"].as_str() {
                            Some("fix_logout") => Ok(session.logout(action["text"].as_str())),
                            _ => server_actions::app_msg_type(&action).and_then(|t| {
                                session.encode(&t, &server_actions::body_fields(&action)?)
                            }),
                        };
                        match bytes {
                            Ok(b) => {
                                let n = b.len();
                                wire.write(&[b]).await?;
                                ClientSendOutcome::Sent { bytes_sent: n }
                            }
                            Err(e) => ClientSendOutcome::Rejected {
                                error: e.to_string(),
                            },
                        }
                    }
                };
                if session.logout_sent {
                    closing.get_or_insert_with(Instant::now);
                }
                if let Some(c) = command {
                    ctx.state
                        .record_access_log(
                            AccessLogOwner::Client(ctx.client_id.as_u32()),
                            "FIX",
                            None,
                            "injected_action",
                            json!({"type": action["type"], "msg_type": action["msg_type"]}),
                            vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                        )
                        .await;
                    crate::client::command_support::reply(c, Ok(outcome));
                }
            }
            Wake::Message(Some(m)) => {
                let step = session.on_message(m);
                wire.write(&step.send).await?;
                if let Some(reason) = step.close {
                    return Ok(reason);
                }
                if let Some(app) = step.app {
                    let msg_type = app.msg_type().to_owned();
                    let fields: Vec<Value> = session::body(&app).into_iter().map(|(tag, value)| json!({"tag": tag, "name": dict::field_name(tag), "value": value})).collect();
                    let event = Event::new(
                        &actions::MESSAGE_EVENT,
                        json!({"msg_type": msg_type, "msg_type_name": dict::message_name(&msg_type), "seq": app.seq(), "fields": fields}),
                    );
                    if events.send(event).await.is_err() {
                        return Ok("event consumer stopped".into());
                    }
                }
            }
        }
    }
}
