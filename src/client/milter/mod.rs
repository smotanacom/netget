//! Milter client: the MTA side of a mail filter conversation.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::milter::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::MilterClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// How long the filter may take to answer one command.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
const BODY_CHUNK: usize = 64 * 1024;

fn action_names(mask: u32) -> Vec<&'static str> {
    [
        (wire::F_ADDHDRS, "add_header"),
        (wire::F_CHGBODY, "change_body"),
        (wire::F_ADDRCPT, "add_rcpt"),
        (wire::F_DELRCPT, "del_rcpt"),
        (wire::F_CHGHDRS, "change_header"),
        (wire::F_QUARANTINE, "quarantine"),
    ]
    .into_iter()
    .filter(|(b, _)| mask & b != 0)
    .map(|(_, n)| n)
    .collect()
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("milter connect deadline")??;
    let local = stream.local_addr()?;
    let (mut r, mut w) = tokio::io::split(stream);
    wire::write(
        &mut w,
        wire::C_OPTNEG,
        &wire::optneg(wire::VERSION, wire::ALL_ACTIONS, wire::OFFERED_PROTOCOL),
    )
    .await?;
    let (cmd, data) = wire::read_packet(&mut r, wire::IO_TIMEOUT)
        .await?
        .context("the filter closed the connection during negotiation")?;
    ensure!(
        cmd == wire::C_OPTNEG,
        "the filter answered negotiation with {:?}",
        cmd as char
    );
    let (version, agreed, protocol) = wire::parse_optneg(&data)?;
    ensure!(
        protocol & !wire::OFFERED_PROTOCOL == 0,
        "the filter asked for protocol steps ({:#x}) this MTA did not offer",
        protocol & !wire::OFFERED_PROTOCOL
    );
    let steps: Vec<&str> = wire::PROTOCOL_STEPS
        .iter()
        .filter(|(b, _)| protocol & b != 0)
        .map(|(_, n)| *n)
        .collect();
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({"version": version, "actions": action_names(agreed), "protocol": steps}),
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
                &MilterClientProtocol,
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
                    .warn(format!("Milter client handler: {e}")),
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
            &mut r,
            &mut w,
            protocol,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        let _ = wire::write(&mut w, wire::C_QUIT, &[]).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Milter client ended: {e:#}"));
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

/// The filter's answer to one command. `implicit` when it gave none: it asked for the stage to be
/// left out or not answered, which the protocol reads as continue.
struct Verdict {
    code: u8,
    data: Vec<u8>,
    mods: Vec<Value>,
    implicit: bool,
}

impl Verdict {
    fn implicit() -> Self {
        Self {
            code: wire::R_CONTINUE,
            data: vec![],
            mods: vec![],
            implicit: true,
        }
    }
}

/// Read until the filter's verdict, collecting the modifications before it.
async fn verdict(r: &mut ReadHalf<TcpStream>) -> Result<Verdict> {
    let mut mods = Vec::new();
    loop {
        let (cmd, data) = wire::read_packet(r, REPLY_TIMEOUT)
            .await?
            .context("the filter closed the connection")?;
        let s = wire::strings(&data);
        let first = || s.first().cloned().unwrap_or_default();
        match cmd {
            wire::R_CONTINUE
            | wire::R_ACCEPT
            | wire::R_REJECT
            | wire::R_TEMPFAIL
            | wire::R_DISCARD
            | wire::R_REPLYCODE
            | wire::R_SKIP => {
                return Ok(Verdict {
                    code: cmd,
                    data,
                    mods,
                    implicit: false,
                })
            }
            wire::R_PROGRESS => {}
            wire::R_ADDHEADER => {
                mods.push(json!({"kind": "add_header", "name": first(), "value": s.get(1)}))
            }
            wire::R_CHGHEADER => {
                ensure!(data.len() >= 4, "short change_header");
                let index = u32::from_be_bytes(data[..4].try_into().unwrap());
                let f = wire::strings(&data[4..]);
                mods.push(json!({"kind": "change_header", "index": index, "name": f.first(), "value": f.get(1)}));
            }
            wire::R_ADDRCPT => mods.push(json!({"kind": "add_rcpt", "recipient": first()})),
            wire::R_DELRCPT => mods.push(json!({"kind": "del_rcpt", "recipient": first()})),
            wire::R_REPLBODY => {
                mods.push(json!({"kind": "replace_body", "body": String::from_utf8_lossy(&data)}))
            }
            wire::R_QUARANTINE => mods.push(json!({"kind": "quarantine", "reason": first()})),
            other => bail!("unexpected reply {:?} from the filter", other as char),
        }
    }
}

/// The negotiated protocol steps, and the I/O they govern.
struct Conn<'a> {
    r: &'a mut ReadHalf<TcpStream>,
    w: &'a mut WriteHalf<TcpStream>,
    protocol: u32,
}

impl Conn<'_> {
    /// Send one command and read its verdict — unless the filter asked for the stage to be left
    /// out (`skip`) or not answered (`no_reply`).
    async fn step(&mut self, cmd: u8, data: &[u8], skip: u32, no_reply: u32) -> Result<Verdict> {
        if self.protocol & skip != 0 {
            return Ok(Verdict::implicit());
        }
        wire::write(self.w, cmd, data).await?;
        if self.protocol & no_reply != 0 {
            return Ok(Verdict::implicit());
        }
        verdict(self.r).await
    }
}

fn reply_event(stage: &str, v: Verdict) -> Event {
    let text = (v.code == wire::R_REPLYCODE).then(|| {
        wire::strings(&v.data)
            .into_iter()
            .next()
            .unwrap_or_default()
    });
    Event::new(
        &actions::REPLY_EVENT,
        json!({"stage": stage, "decision": wire::reply_name(v.code), "text": text, "modifications": v.mods, "implicit": v.implicit}),
    )
}

/// Carry out one action; the event it raises, if any.
async fn perform(c: &mut Conn<'_>, a: &Value) -> Result<Option<Event>> {
    let strs = |k: &str| -> Vec<String> {
        a[k].as_array()
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Some(match a["type"].as_str().unwrap_or_default() {
        "milter_connect" => {
            let addr = a["address"].as_str().unwrap_or_default();
            let family = if addr.contains(':') { '6' } else { '4' };
            let body = wire::connect_body(
                a["hostname"].as_str().unwrap_or_default(),
                family,
                a["port"].as_u64().unwrap_or(0) as u16,
                addr,
            );
            reply_event(
                "connect",
                c.step(wire::C_CONNECT, &body, wire::P_NOCONNECT, wire::P_NR_CONN)
                    .await?,
            )
        }
        "milter_helo" => {
            let body = wire::cstrings(&[a["name"].as_str().unwrap_or_default()]);
            reply_event(
                "helo",
                c.step(wire::C_HELO, &body, wire::P_NOHELO, wire::P_NR_HELO)
                    .await?,
            )
        }
        "milter_mail" | "milter_rcpt" => {
            let (field, cmd, stage, skip, no_reply) = if a["type"] == "milter_mail" {
                (
                    "sender",
                    wire::C_MAIL,
                    "mail",
                    wire::P_NOMAIL,
                    wire::P_NR_MAIL,
                )
            } else {
                (
                    "recipient",
                    wire::C_RCPT,
                    "rcpt",
                    wire::P_NORCPT,
                    wire::P_NR_RCPT,
                )
            };
            let mut fields = vec![a[field].as_str().unwrap_or_default().to_string()];
            fields.extend(strs("esmtp_args"));
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            reply_event(
                stage,
                c.step(cmd, &wire::cstrings(&refs), skip, no_reply).await?,
            )
        }
        "milter_message" => {
            let mut v = c
                .step(wire::C_DATA, &[], wire::P_NODATA, wire::P_NR_DATA)
                .await?;
            if v.code == wire::R_CONTINUE {
                for h in a["headers"].as_array().cloned().unwrap_or_default() {
                    let body = wire::cstrings(&[
                        h["name"].as_str().unwrap_or_default(),
                        h["value"].as_str().unwrap_or_default(),
                    ]);
                    v = c
                        .step(wire::C_HEADER, &body, wire::P_NOHDRS, wire::P_NR_HDR)
                        .await?;
                    if v.code != wire::R_CONTINUE {
                        break;
                    }
                }
            }
            if v.code == wire::R_CONTINUE {
                v = c
                    .step(wire::C_EOH, &[], wire::P_NOEOH, wire::P_NR_EOH)
                    .await?;
            }
            if v.code == wire::R_CONTINUE {
                for chunk in a["body"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes()
                    .chunks(BODY_CHUNK)
                {
                    v = c
                        .step(wire::C_BODY, chunk, wire::P_NOBODY, wire::P_NR_BODY)
                        .await?;
                    if v.code != wire::R_CONTINUE {
                        break;
                    }
                }
            }
            // SMFIR_SKIP: the filter has seen enough of the body; end of message follows.
            if v.code == wire::R_CONTINUE || v.code == wire::R_SKIP {
                v = c.step(wire::C_BODYEOB, &[], 0, 0).await?;
            }
            reply_event("message", v)
        }
        "milter_abort" => {
            wire::write(c.w, wire::C_ABORT, &[]).await?;
            return Ok(None);
        }
        t => bail!("unsupported action {t}"),
    }))
}

async fn run(
    ctx: &ConnectContext,
    r: &mut ReadHalf<TcpStream>,
    w: &mut WriteHalf<TcpStream>,
    protocol: u32,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) -> Result<()> {
    let log = Log::new(Some(&ctx.status_tx));
    loop {
        let (action, depth, caller) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), 0, Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some((a, d)) => (a, d, None), None => return Ok(()) },
        };
        let reply = |c: Option<ClientCommand>, o: ClientSendOutcome| {
            if let Some(c) = c {
                crate::client::command_support::reply(c, Ok(o));
            }
        };
        match MilterClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(caller, ClientSendOutcome::Disconnected);
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                reply(
                    caller,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                continue;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "Milter client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if caller.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Milter",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        match perform(
            &mut Conn {
                r: &mut *r,
                w: &mut *w,
                protocol,
            },
            &action,
        )
        .await?
        {
            Some(event) => {
                reply(
                    caller,
                    ClientSendOutcome::Executed {
                        detail: event.data.to_string(),
                    },
                );
                events
                    .try_send((event, depth))
                    .context("milter event queue full; consumer stalled")?;
            }
            None => reply(caller, ClientSendOutcome::Sent { bytes_sent: 5 }),
        }
    }
}
