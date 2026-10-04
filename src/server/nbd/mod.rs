//! Network Block Device server. Rust owns the fixed newstyle handshake, option haggling and
//! transmission; the handler describes each export once per connection and Rust serves every
//! read from that description. Exports are read-only.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wire::*;

/// The whole handshake, and the rest of any message once its first byte has arrived.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(60);
pub const MESSAGE_DEADLINE: Duration = Duration::from_secs(60);
pub const MAX_OPTIONS: usize = 64;
/// Block status descriptors in one reply.
pub const MAX_DESCRIPTORS: usize = 1024;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("NBD server on {local}"));
    let shared = Arc::new(ctx.clone());
    let server_id = ctx.server_id;
    let accept =
        tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                let (stream, peer, permit) =
                    match accept_bounded(&listener, &limiter, b"", "NBD", Some(&shared.status_tx))
                        .await
                    {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                let id = ConnectionId::new(shared.state.get_next_unified_id().await);
                let now = Instant::now();
                shared
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
                    .state
                    .spawn_server_task(server_id, async move {
                        let _permit = permit;
                        let mut c = Conn {
                            ctx: &child,
                            id,
                            peer,
                            stream,
                            structured: false,
                            meta: false,
                            exports: HashMap::new(),
                        };
                        if let Err(e) = c.run().await {
                            Log::new(Some(&child.status_tx))
                                .debug(format!("NBD connection {id}: {e:#}"));
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
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("NBD connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// What the handler said about an export or the list.
enum Answer {
    Export(Arc<Export>),
    List(Vec<(String, Option<String>)>),
    Refused(u32),
}

struct Conn<'a> {
    ctx: &'a SpawnContext,
    id: ConnectionId,
    peer: SocketAddr,
    stream: TcpStream,
    structured: bool,
    meta: bool,
    exports: HashMap<String, Option<Arc<Export>>>,
}

fn opt_reply(option: u32, kind: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + data.len());
    out.extend(REPLY_MAGIC.to_be_bytes());
    out.extend(option.to_be_bytes());
    out.extend(kind.to_be_bytes());
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(data);
    out
}

fn chunk(flags: u16, kind: u16, cookie: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + payload.len());
    out.extend(STRUCTURED_REPLY_MAGIC.to_be_bytes());
    out.extend(flags.to_be_bytes());
    out.extend(kind.to_be_bytes());
    out.extend(cookie.to_be_bytes());
    out.extend((payload.len() as u32).to_be_bytes());
    out.extend(payload);
    out
}

fn simple(error: u32, cookie: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + data.len());
    out.extend(SIMPLE_REPLY_MAGIC.to_be_bytes());
    out.extend(error.to_be_bytes());
    out.extend(cookie.to_be_bytes());
    out.extend(data);
    out
}

fn transmission_flags(structured: bool) -> u16 {
    let mut f = TFLAG_HAS_FLAGS
        | TFLAG_READ_ONLY
        | TFLAG_SEND_FLUSH
        | TFLAG_CAN_MULTI_CONN
        | TFLAG_SEND_CACHE;
    if structured {
        f |= TFLAG_SEND_DF;
    }
    f
}

impl Conn<'_> {
    async fn read_exact(&mut self, buf: &mut [u8], deadline: tokio::time::Instant) -> Result<()> {
        tokio::time::timeout_at(deadline, self.stream.read_exact(buf))
            .await
            .context("the client stalled")??;
        Ok(())
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        self.ctx
            .state
            .update_connection_stats(
                self.ctx.server_id,
                self.id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    async fn ask(&mut self, event: Event, operation: &str) -> Answer {
        let result = match call_llm(
            &self.ctx.llm_client,
            &self.ctx.state,
            self.ctx.server_id,
            Some(self.id),
            &event,
            &actions::NbdProtocol,
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                outcome(self.ctx, self.id, operation, "fail_closed_llm_error");
                return Answer::Refused(REP_ERR_POLICY);
            }
        };
        let mut answers = Vec::new();
        let mut pending = result.protocol_results;
        while let Some(r) = pending.pop() {
            match r {
                ActionResult::Custom { data, .. } => answers.push(data),
                ActionResult::Multiple(items) => pending.extend(items),
                _ => {}
            }
        }
        if !result.failures.is_empty() || answers.len() != 1 {
            outcome(
                self.ctx,
                self.id,
                operation,
                if answers.is_empty() && result.failures.is_empty() {
                    "model_silent"
                } else {
                    "fail_closed_invalid_reply"
                },
            );
            return Answer::Refused(REP_ERR_POLICY);
        }
        let a = answers.remove(0);
        match a["type"].as_str() {
            Some("nbd_export") if operation == "export" => match Export::from_action(&a) {
                Ok(e) => {
                    outcome(self.ctx, self.id, operation, "model_answer");
                    Answer::Export(Arc::new(e))
                }
                Err(_) => {
                    outcome(self.ctx, self.id, operation, "fail_closed_invalid_reply");
                    Answer::Refused(REP_ERR_POLICY)
                }
            },
            Some("nbd_list_exports") if operation == "list" => {
                outcome(self.ctx, self.id, operation, "model_answer");
                Answer::List(
                    a["exports"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|e| {
                            Some((
                                e["name"].as_str()?.to_owned(),
                                e["description"].as_str().map(str::to_owned),
                            ))
                        })
                        .collect(),
                )
            }
            Some("nbd_reject") => {
                outcome(self.ctx, self.id, operation, "model_reject");
                Answer::Refused(if a["reason"] == "unknown" {
                    REP_ERR_UNKNOWN
                } else {
                    REP_ERR_POLICY
                })
            }
            _ => {
                outcome(self.ctx, self.id, operation, "fail_closed_invalid_reply");
                Answer::Refused(REP_ERR_POLICY)
            }
        }
    }

    /// The handler's description of an export, asked once per name per connection.
    async fn export(&mut self, name: &str, option: &str) -> Result<Arc<Export>, u32> {
        if let Some(known) = self.exports.get(name) {
            return known.clone().ok_or(REP_ERR_UNKNOWN);
        }
        let event = Event::new(
            &actions::EXPORT_EVENT,
            json!({"export": name, "option": option}),
        );
        match self.ask(event, "export").await {
            Answer::Export(e) => {
                self.exports.insert(name.to_owned(), Some(e.clone()));
                Ok(e)
            }
            Answer::Refused(code) => {
                if code == REP_ERR_UNKNOWN {
                    self.exports.insert(name.to_owned(), None);
                }
                Err(code)
            }
            Answer::List(_) => Err(REP_ERR_POLICY),
        }
    }

    async fn run(&mut self) -> Result<()> {
        let mut hello = Vec::with_capacity(18);
        hello.extend(NBDMAGIC.to_be_bytes());
        hello.extend(IHAVEOPT.to_be_bytes());
        hello.extend((FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes());
        self.write(&hello).await?;
        let deadline = tokio::time::Instant::now() + HANDSHAKE_DEADLINE;
        let mut b4 = [0u8; 4];
        self.read_exact(&mut b4, deadline).await?;
        let client_flags = u32::from_be_bytes(b4);
        let (fixed, no_zeroes) = (client_flags & 1 != 0, client_flags & 2 != 0);
        ensure!(
            client_flags & !3 == 0,
            "unknown client flags {client_flags:#x}"
        );
        let mut options = 0;
        let export = loop {
            options += 1;
            if options > MAX_OPTIONS {
                outcome(self.ctx, self.id, "negotiate", "protocol_refusal");
                bail!("more than {MAX_OPTIONS} options");
            }
            let mut head = [0u8; 16];
            self.read_exact(&mut head, deadline).await?;
            ensure!(
                u64::from_be_bytes(head[..8].try_into()?) == IHAVEOPT,
                "an option without IHAVEOPT"
            );
            let option = u32::from_be_bytes(head[8..12].try_into()?);
            let len = u32::from_be_bytes(head[12..16].try_into()?) as usize;
            if len > MAX_OPTION {
                outcome(self.ctx, self.id, "negotiate", "protocol_refusal");
                bail!("option {option} declares {len} bytes");
            }
            let mut data = vec![0u8; len];
            self.read_exact(&mut data, deadline).await?;
            self.ctx
                .state
                .update_connection_stats(
                    self.ctx.server_id,
                    self.id,
                    Some(16 + len as u64),
                    None,
                    Some(1),
                    None,
                )
                .await;
            if !fixed && option != OPT_EXPORT_NAME {
                bail!("a client without fixed newstyle may only send EXPORT_NAME");
            }
            match option {
                OPT_EXPORT_NAME => {
                    let name = String::from_utf8_lossy(&data).into_owned();
                    match self.export(&name, "export_name").await {
                        Ok(e) => {
                            let mut out = e.size.to_be_bytes().to_vec();
                            out.extend(transmission_flags(self.structured).to_be_bytes());
                            if !no_zeroes {
                                out.extend([0u8; 124]);
                            }
                            self.write(&out).await?;
                            break e;
                        }
                        // EXPORT_NAME has no error reply: closing is the refusal.
                        Err(_) => return Ok(()),
                    }
                }
                OPT_ABORT => {
                    self.write(&opt_reply(option, REP_ACK, &[])).await?;
                    return Ok(());
                }
                OPT_LIST => {
                    if len != 0 {
                        self.write(&opt_reply(option, REP_ERR_INVALID, b"LIST takes no data"))
                            .await?;
                        continue;
                    }
                    match self
                        .ask(
                            Event::new(
                                &actions::LIST_EVENT,
                                json!({"client": self.peer.to_string()}),
                            ),
                            "list",
                        )
                        .await
                    {
                        Answer::List(list) => {
                            for (name, description) in list {
                                let mut d = (name.len() as u32).to_be_bytes().to_vec();
                                d.extend(name.as_bytes());
                                d.extend(description.unwrap_or_default().as_bytes());
                                self.write(&opt_reply(option, REP_SERVER, &d)).await?;
                            }
                            self.write(&opt_reply(option, REP_ACK, &[])).await?;
                        }
                        _ => {
                            self.write(&opt_reply(
                                option,
                                REP_ERR_POLICY,
                                b"listing is not allowed",
                            ))
                            .await?
                        }
                    }
                }
                OPT_STRUCTURED_REPLY => {
                    if len != 0 {
                        self.write(&opt_reply(option, REP_ERR_INVALID, &[])).await?;
                    } else {
                        self.structured = true;
                        self.write(&opt_reply(option, REP_ACK, &[])).await?;
                    }
                }
                OPT_LIST_META_CONTEXT | OPT_SET_META_CONTEXT => {
                    let Some(queries) = meta_queries(&data) else {
                        self.write(&opt_reply(
                            option,
                            REP_ERR_INVALID,
                            b"malformed meta context request",
                        ))
                        .await?;
                        continue;
                    };
                    if option == OPT_SET_META_CONTEXT && !self.structured {
                        self.write(&opt_reply(
                            option,
                            REP_ERR_INVALID,
                            b"negotiate structured replies first",
                        ))
                        .await?;
                        continue;
                    }
                    let wanted = if option == OPT_LIST_META_CONTEXT {
                        queries.is_empty()
                            || queries.iter().any(|q| q == "base:" || q == BASE_ALLOCATION)
                    } else {
                        queries.iter().any(|q| q == BASE_ALLOCATION)
                    };
                    if option == OPT_SET_META_CONTEXT {
                        self.meta = wanted;
                    }
                    if wanted {
                        let mut d = 1u32.to_be_bytes().to_vec();
                        d.extend(BASE_ALLOCATION.as_bytes());
                        self.write(&opt_reply(option, REP_META_CONTEXT, &d)).await?;
                    }
                    self.write(&opt_reply(option, REP_ACK, &[])).await?;
                }
                OPT_INFO | OPT_GO => {
                    let Some((name, infos)) = info_request(&data) else {
                        self.write(&opt_reply(
                            option,
                            REP_ERR_INVALID,
                            b"malformed INFO/GO request",
                        ))
                        .await?;
                        continue;
                    };
                    let which = if option == OPT_GO { "go" } else { "info" };
                    let e = match self.export(&name, which).await {
                        Ok(e) => e,
                        Err(code) => {
                            let text: &[u8] = if code == REP_ERR_UNKNOWN {
                                b"no such export"
                            } else {
                                b"refused"
                            };
                            self.write(&opt_reply(option, code, text)).await?;
                            continue;
                        }
                    };
                    let mut info = INFO_EXPORT.to_be_bytes().to_vec();
                    info.extend(e.size.to_be_bytes());
                    info.extend(transmission_flags(self.structured).to_be_bytes());
                    self.write(&opt_reply(option, REP_INFO, &info)).await?;
                    if infos.contains(&INFO_NAME) {
                        let mut d = INFO_NAME.to_be_bytes().to_vec();
                        d.extend(name.as_bytes());
                        self.write(&opt_reply(option, REP_INFO, &d)).await?;
                    }
                    if let (true, Some(desc)) = (infos.contains(&INFO_DESCRIPTION), &e.description)
                    {
                        let mut d = INFO_DESCRIPTION.to_be_bytes().to_vec();
                        d.extend(desc.as_bytes());
                        self.write(&opt_reply(option, REP_INFO, &d)).await?;
                    }
                    if infos.contains(&INFO_BLOCK_SIZE) {
                        let mut d = INFO_BLOCK_SIZE.to_be_bytes().to_vec();
                        for v in [e.block_min, e.block_preferred, e.block_max] {
                            d.extend(v.to_be_bytes());
                        }
                        self.write(&opt_reply(option, REP_INFO, &d)).await?;
                    }
                    self.write(&opt_reply(option, REP_ACK, &[])).await?;
                    if option == OPT_GO {
                        break e;
                    }
                }
                _ => {
                    self.write(&opt_reply(option, REP_ERR_UNSUP, b"not supported"))
                        .await?
                }
            }
        };
        self.transmission(&export).await
    }

    async fn transmission(&mut self, e: &Export) -> Result<()> {
        loop {
            let mut req = [0u8; 28];
            let n = self.stream.read(&mut req[..1]).await?;
            if n == 0 {
                return Ok(());
            }
            let deadline = tokio::time::Instant::now() + MESSAGE_DEADLINE;
            self.read_exact(&mut req[1..], deadline).await?;
            ensure!(
                u32::from_be_bytes(req[..4].try_into()?) == REQUEST_MAGIC,
                "bad request magic"
            );
            let flags = u16::from_be_bytes(req[4..6].try_into()?);
            let kind = u16::from_be_bytes(req[6..8].try_into()?);
            let cookie = u64::from_be_bytes(req[8..16].try_into()?);
            let offset = u64::from_be_bytes(req[16..24].try_into()?);
            let length = u32::from_be_bytes(req[24..28].try_into()?);
            self.ctx
                .state
                .update_connection_stats(self.ctx.server_id, self.id, Some(28), None, Some(1), None)
                .await;
            let in_range = offset
                .checked_add(length as u64)
                .is_some_and(|end| end <= e.size);
            match kind {
                CMD_DISC => return Ok(()),
                CMD_WRITE => {
                    // Drain the payload so the stream stays in step, then refuse.
                    ensure!(length <= MAX_REQUEST, "a write of {length} bytes");
                    let mut sink = vec![0u8; length as usize];
                    self.read_exact(&mut sink, deadline).await?;
                    self.fail(cookie, EPERM, None, "the export is read-only")
                        .await?;
                }
                CMD_TRIM | CMD_WRITE_ZEROES => {
                    self.fail(cookie, EPERM, None, "the export is read-only")
                        .await?
                }
                CMD_FLUSH | CMD_CACHE => self.ok(cookie).await?,
                CMD_READ => {
                    if length == 0 || length > e.block_max || !in_range {
                        self.fail(
                            cookie,
                            EINVAL,
                            None,
                            "the read is outside the export or over the maximum block size",
                        )
                        .await?;
                    } else if let Some((at, err)) = e.error_in(offset, length as u64) {
                        self.fail(cookie, err, Some(at), "this region fails reads")
                            .await?;
                    } else if !self.structured {
                        self.write(&simple(0, cookie, &e.read(offset, length)))
                            .await?;
                    } else if flags & CMD_FLAG_DF != 0 {
                        let mut p = offset.to_be_bytes().to_vec();
                        p.extend(e.read(offset, length));
                        self.write(&chunk(REPLY_FLAG_DONE, REPLY_OFFSET_DATA, cookie, &p))
                            .await?;
                    } else {
                        let pieces = e.pieces(offset, length);
                        let last = pieces.len() - 1;
                        for (i, piece) in pieces.into_iter().enumerate() {
                            let done = if i == last { REPLY_FLAG_DONE } else { 0 };
                            let bytes = match piece {
                                Piece::Data(o, data) => {
                                    let mut p = o.to_be_bytes().to_vec();
                                    p.extend(data);
                                    chunk(done, REPLY_OFFSET_DATA, cookie, &p)
                                }
                                Piece::Hole(o, n) => {
                                    let mut p = o.to_be_bytes().to_vec();
                                    p.extend(n.to_be_bytes());
                                    chunk(done, REPLY_OFFSET_HOLE, cookie, &p)
                                }
                            };
                            self.write(&bytes).await?;
                        }
                    }
                }
                CMD_BLOCK_STATUS => {
                    if !self.meta || length == 0 || !in_range {
                        self.fail(
                            cookie,
                            EINVAL,
                            None,
                            "block status needs base:allocation and a range inside the export",
                        )
                        .await?;
                    } else {
                        let mut p = 1u32.to_be_bytes().to_vec();
                        for (_, len, data) in e
                            .runs(offset, length as u64)
                            .into_iter()
                            .take(MAX_DESCRIPTORS)
                        {
                            p.extend((len as u32).to_be_bytes());
                            p.extend(
                                (if data { 0 } else { STATE_HOLE | STATE_ZERO }).to_be_bytes(),
                            );
                        }
                        self.write(&chunk(REPLY_FLAG_DONE, REPLY_BLOCK_STATUS, cookie, &p))
                            .await?;
                    }
                }
                _ => self.fail(cookie, EINVAL, None, "unknown command").await?,
            }
        }
    }

    async fn ok(&mut self, cookie: u64) -> Result<()> {
        if self.structured {
            self.write(&chunk(REPLY_FLAG_DONE, REPLY_NONE, cookie, &[]))
                .await
        } else {
            self.write(&simple(0, cookie, &[])).await
        }
    }

    async fn fail(
        &mut self,
        cookie: u64,
        errno: u32,
        at: Option<u64>,
        message: &str,
    ) -> Result<()> {
        if !self.structured {
            return self.write(&simple(errno, cookie, &[])).await;
        }
        let mut p = errno.to_be_bytes().to_vec();
        p.extend((message.len() as u16).to_be_bytes());
        p.extend(message.as_bytes());
        let kind = match at {
            Some(o) => {
                p.extend(o.to_be_bytes());
                REPLY_ERROR_OFFSET
            }
            None => REPLY_ERROR,
        };
        self.write(&chunk(REPLY_FLAG_DONE, kind, cookie, &p)).await
    }
}

/// An INFO/GO request's export name and requested info types.
fn info_request(d: &[u8]) -> Option<(String, Vec<u16>)> {
    let n = u32::from_be_bytes(d.get(..4)?.try_into().ok()?) as usize;
    let name = String::from_utf8(d.get(4..4 + n)?.to_vec()).ok()?;
    let rest = d.get(4 + n..)?;
    let count = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?) as usize;
    let list = rest.get(2..)?;
    if list.len() != count * 2 {
        return None;
    }
    Some((
        name,
        list.chunks(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect(),
    ))
}

/// A meta context request's queries.
fn meta_queries(d: &[u8]) -> Option<Vec<String>> {
    let n = u32::from_be_bytes(d.get(..4)?.try_into().ok()?) as usize;
    let mut at = 4 + n;
    d.get(4..at)?;
    let count = u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?) as usize;
    at += 4;
    let mut out = Vec::new();
    for _ in 0..count.min(256) {
        let l = u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?) as usize;
        out.push(String::from_utf8(d.get(at + 4..at + 4 + l)?.to_vec()).ok()?);
        at += 4 + l;
    }
    (at == d.len() && out.len() == count).then_some(out)
}
