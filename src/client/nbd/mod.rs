//! Network Block Device client: fixed newstyle negotiation, then one request at a time.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::nbd::wire::*;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::NbdClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_EXPORT: &str = "";
pub const DEFAULT_LIST_EXPORTS: bool = false;
const TIMEOUT: Duration = Duration::from_secs(30);
const SHOWN: usize = 4096;

struct Session {
    stream: TcpStream,
    meta: bool,
    cookie: u64,
}

async fn read_n(s: &mut TcpStream, n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    tokio::time::timeout(TIMEOUT, s.read_exact(&mut b))
        .await
        .context("the server stalled")??;
    Ok(b)
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap_or_default())
}

async fn option(s: &mut TcpStream, opt: u32, data: &[u8]) -> Result<()> {
    let mut out = IHAVEOPT.to_be_bytes().to_vec();
    out.extend(opt.to_be_bytes());
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(data);
    s.write_all(&out).await?;
    Ok(())
}

/// One option reply: (type, data).
async fn reply(s: &mut TcpStream, opt: u32) -> Result<(u32, Vec<u8>)> {
    let h = read_n(s, 20).await?;
    ensure!(be64(&h) == REPLY_MAGIC, "bad option reply magic");
    ensure!(be32(&h[8..]) == opt, "a reply to another option");
    let len = be32(&h[16..]) as usize;
    ensure!(len <= MAX_OPTION, "an option reply of {len} bytes");
    Ok((be32(&h[12..]), read_n(s, len).await?))
}

fn rep_error(code: u32) -> &'static str {
    match code {
        REP_ERR_UNSUP => "unsupported",
        REP_ERR_POLICY => "refused by policy",
        REP_ERR_INVALID => "invalid",
        REP_ERR_UNKNOWN => "unknown export",
        REP_ERR_TOO_BIG => "too big",
        _ => "refused",
    }
}

fn shown(data: &[u8]) -> (Json, &'static str) {
    let head = &data[..data.len().min(SHOWN)];
    match std::str::from_utf8(head) {
        Ok(t) => (json!(t), "utf8"),
        Err(_) => (json!(hex::encode(head)), "hex"),
    }
}

impl Session {
    fn request(&mut self, kind: u16, offset: u64, length: u32) -> Vec<u8> {
        self.cookie += 1;
        let mut r = REQUEST_MAGIC.to_be_bytes().to_vec();
        r.extend(0u16.to_be_bytes());
        r.extend(kind.to_be_bytes());
        r.extend(self.cookie.to_be_bytes());
        r.extend(offset.to_be_bytes());
        r.extend(length.to_be_bytes());
        r
    }

    /// The reply to the current request: data placed into `buf` (for reads), block status
    /// descriptors, or an error with its offset.
    async fn answer(
        &mut self,
        buf: &mut [u8],
        base: u64,
    ) -> Result<(u32, Option<u64>, Vec<(u32, u32)>)> {
        let s = &mut self.stream;
        let first = read_n(s, 4).await?;
        let magic = be32(&first);
        if magic == SIMPLE_REPLY_MAGIC {
            let rest = read_n(s, 12).await?;
            ensure!(
                be64(&rest[4..]) == self.cookie,
                "a reply for another request"
            );
            let err = be32(&rest);
            if err == 0 && !buf.is_empty() {
                tokio::time::timeout(TIMEOUT, s.read_exact(buf))
                    .await
                    .context("the server stalled")??;
            }
            return Ok((err, None, vec![]));
        }
        ensure!(
            magic == STRUCTURED_REPLY_MAGIC,
            "bad reply magic {magic:#x}"
        );
        let mut header = first;
        let mut statuses = Vec::new();
        let mut error = (0, None);
        loop {
            header.extend(read_n(s, 16).await?);
            let flags = u16::from_be_bytes([header[4], header[5]]);
            let kind = u16::from_be_bytes([header[6], header[7]]);
            ensure!(
                be64(&header[8..]) == self.cookie,
                "a reply for another request"
            );
            let len = be32(&header[16..]) as usize;
            ensure!(
                len <= buf.len() + 4096 + 8 * 1024,
                "a reply chunk of {len} bytes"
            );
            let p = read_n(s, len).await?;
            match kind {
                REPLY_OFFSET_DATA => {
                    ensure!(len >= 8, "short data chunk");
                    let o = be64(&p).checked_sub(base).context("data before the read")? as usize;
                    let d = &p[8..];
                    ensure!(o + d.len() <= buf.len(), "data past the read");
                    buf[o..o + d.len()].copy_from_slice(d);
                }
                REPLY_OFFSET_HOLE => {
                    ensure!(len == 12, "bad hole chunk");
                    let o = be64(&p).checked_sub(base).context("hole before the read")? as usize;
                    let n = be32(&p[8..]) as usize;
                    ensure!(o + n <= buf.len(), "hole past the read");
                    buf[o..o + n].fill(0);
                }
                REPLY_BLOCK_STATUS => {
                    ensure!(len >= 4 && (len - 4) % 8 == 0, "bad block status chunk");
                    statuses.extend(p[4..].chunks(8).map(|c| (be32(c), be32(&c[4..]))));
                }
                REPLY_ERROR | REPLY_ERROR_OFFSET => {
                    ensure!(len >= 6, "short error chunk");
                    let mlen = u16::from_be_bytes([p[4], p[5]]) as usize;
                    let at = (kind == REPLY_ERROR_OFFSET && p.len() >= 6 + mlen + 8)
                        .then(|| be64(&p[6 + mlen..]));
                    error = (be32(&p), at);
                }
                REPLY_NONE => {}
                other => bail!("unknown reply chunk type {other}"),
            }
            if flags & REPLY_FLAG_DONE != 0 {
                return Ok((error.0, error.1, statuses));
            }
            header = read_n(s, 4).await?;
            ensure!(be32(&header) == STRUCTURED_REPLY_MAGIC, "bad reply magic");
        }
    }

    async fn act(&mut self, v: &Json) -> Result<Event> {
        let offset = v["offset"].as_u64().unwrap_or(0);
        let length = v["length"].as_u64().unwrap_or(0) as u32;
        match v["type"].as_str().unwrap_or_default() {
            "nbd_read" => {
                let r = self.request(CMD_READ, offset, length);
                self.stream.write_all(&r).await?;
                let mut buf = vec![0u8; length as usize];
                let (err, at, _) = self.answer(&mut buf, offset).await?;
                let mut data = json!({"offset": offset, "length": length});
                if err != 0 {
                    data["error"] = json!(errno_name(err));
                    data["error_offset"] = json!(at);
                } else {
                    let (text, enc) = shown(&buf);
                    data["data"] = text;
                    data["data_encoding"] = json!(enc);
                    data["sha256"] = json!(hex::encode(Sha256::digest(&buf)));
                    data["all_zero"] = json!(buf.iter().all(|b| *b == 0));
                }
                Ok(Event::new(&actions::READ_EVENT, data))
            }
            "nbd_block_status" => {
                if !self.meta {
                    return Ok(Event::new(
                        &actions::STATUS_EVENT,
                        json!({"offset": offset, "error": "the server offered no base:allocation context"}),
                    ));
                }
                let r = self.request(CMD_BLOCK_STATUS, offset, length);
                self.stream.write_all(&r).await?;
                let (err, _, statuses) = self.answer(&mut [], offset).await?;
                if err != 0 {
                    return Ok(Event::new(
                        &actions::STATUS_EVENT,
                        json!({"offset": offset, "error": errno_name(err)}),
                    ));
                }
                let mut at = offset;
                let extents: Vec<Json> = statuses
                    .into_iter()
                    .take(MAX_EXTENTS)
                    .map(|(len, flags)| {
                        let e = json!({"offset": at, "length": len, "hole": flags & STATE_HOLE != 0, "zero": flags & STATE_ZERO != 0});
                        at += len as u64;
                        e
                    })
                    .collect();
                Ok(Event::new(
                    &actions::STATUS_EVENT,
                    json!({"offset": offset, "extents": extents}),
                ))
            }
            "nbd_flush" => {
                let r = self.request(CMD_FLUSH, 0, 0);
                self.stream.write_all(&r).await?;
                let (err, _, _) = self.answer(&mut [], 0).await?;
                Ok(Event::new(
                    &actions::FLUSH_EVENT,
                    json!({"error": errno_name(err)}),
                ))
            }
            other => bail!("{other} is not an NBD request"),
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let export = p
        .map(|p| p.get_optional_string("export"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_EXPORT.to_owned());
    ensure!(export.len() <= 4096, "export is at most 4096 bytes");
    let list = p
        .map(|p| p.get_optional_bool("list_exports"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_LIST_EXPORTS);
    let mut s = tokio::time::timeout(TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("NBD connect timed out")??;
    let local = s.local_addr()?;
    let hello = read_n(&mut s, 18).await?;
    ensure!(
        be64(&hello) == NBDMAGIC && be64(&hello[8..]) == IHAVEOPT,
        "not a newstyle NBD server"
    );
    let server_flags = u16::from_be_bytes([hello[16], hello[17]]);
    ensure!(
        server_flags & FLAG_FIXED_NEWSTYLE != 0,
        "the server does not speak fixed newstyle"
    );
    let client_flags = u32::from(server_flags & (FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES));
    s.write_all(&client_flags.to_be_bytes()).await?;

    let mut exports = Vec::new();
    if list {
        option(&mut s, OPT_LIST, &[]).await?;
        loop {
            let (kind, d) = reply(&mut s, OPT_LIST).await?;
            match kind {
                REP_SERVER if d.len() >= 4 => {
                    let n = (be32(&d) as usize).min(d.len() - 4);
                    exports.push(json!({"name": String::from_utf8_lossy(&d[4..4 + n]), "description": String::from_utf8_lossy(&d[4 + n..])}));
                }
                REP_ACK => break,
                k if k & 0x8000_0000 != 0 => break,
                _ => bail!("unexpected reply {kind} to LIST"),
            }
            ensure!(exports.len() <= 1024, "more than 1024 exports");
        }
    }
    option(&mut s, OPT_STRUCTURED_REPLY, &[]).await?;
    let structured = reply(&mut s, OPT_STRUCTURED_REPLY).await?.0 == REP_ACK;
    let mut meta = None;
    if structured {
        let mut d = (export.len() as u32).to_be_bytes().to_vec();
        d.extend(export.as_bytes());
        d.extend(1u32.to_be_bytes());
        d.extend((BASE_ALLOCATION.len() as u32).to_be_bytes());
        d.extend(BASE_ALLOCATION.as_bytes());
        option(&mut s, OPT_SET_META_CONTEXT, &d).await?;
        loop {
            let (kind, d) = reply(&mut s, OPT_SET_META_CONTEXT).await?;
            match kind {
                REP_META_CONTEXT if d.len() >= 4 && &d[4..] == BASE_ALLOCATION.as_bytes() => {
                    meta = Some(be32(&d))
                }
                REP_META_CONTEXT => {}
                _ => break,
            }
        }
    }
    let mut go = (export.len() as u32).to_be_bytes().to_vec();
    go.extend(export.as_bytes());
    go.extend(3u16.to_be_bytes());
    for i in [INFO_NAME, INFO_DESCRIPTION, INFO_BLOCK_SIZE] {
        go.extend(i.to_be_bytes());
    }
    option(&mut s, OPT_GO, &go).await?;
    let mut info = json!({"export": export, "structured_replies": structured, "base_allocation": meta.is_some()});
    if list {
        info["exports"] = json!(exports);
    }
    loop {
        let (kind, d) = reply(&mut s, OPT_GO).await?;
        match kind {
            REP_ACK => break,
            REP_INFO if d.len() >= 2 => match u16::from_be_bytes([d[0], d[1]]) {
                INFO_EXPORT if d.len() == 12 => {
                    info["size"] = json!(be64(&d[2..]));
                    info["read_only"] =
                        json!(u16::from_be_bytes([d[10], d[11]]) & TFLAG_READ_ONLY != 0);
                }
                INFO_DESCRIPTION => info["description"] = json!(String::from_utf8_lossy(&d[2..])),
                INFO_BLOCK_SIZE if d.len() == 14 => {
                    info["block_size"] = json!({"minimum": be32(&d[2..]), "preferred": be32(&d[6..]), "maximum": be32(&d[10..])})
                }
                _ => {}
            },
            k if k & 0x8000_0000 != 0 => bail!(
                "the server refused export {export:?}: {} ({})",
                rep_error(k),
                String::from_utf8_lossy(&d)
            ),
            _ => {}
        }
    }
    ensure!(
        info.get("size").is_some(),
        "the server did not describe the export"
    );

    let mut session = Session {
        stream: s,
        meta: meta.is_some(),
        cookie: 0,
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Json>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, info))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = NbdClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("NBD client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &mut session, external, internal_rx, &event_tx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("NBD client ended: {e:#}"));
        }
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

async fn run(
    ctx: &ConnectContext,
    s: &mut Session,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Json>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let outcome = match NbdClientProtocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(ClientActionResult::Disconnect) => {
                let r = s.request(CMD_DISC, 0, 0);
                let _ = s.stream.write_all(&r).await;
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => match s.act(&action).await {
                Ok(event) => {
                    events.send(event).await.ok();
                    Ok(ClientSendOutcome::Sent { bytes_sent: 28 })
                }
                Err(e) => Err(e),
            },
        };
        let failed = outcome.is_err();
        if let Some(c) = command {
            let logged = outcome
                .as_ref()
                .map(|o| serde_json::to_value(o).unwrap_or(Json::Null))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "NBD",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![logged],
                )
                .await;
            crate::client::command_support::reply(c, outcome);
        }
        if failed {
            bail!("the NBD connection failed");
        }
    }
}
