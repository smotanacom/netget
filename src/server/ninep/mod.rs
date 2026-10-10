//! 9P2000 file server. Rust owns the message codec, version negotiation, the fid table, walks
//! (including partial walks and ".."), open modes, directory paging, offsets and every bound;
//! the handler decides what exists, what each directory lists, each file's content, and
//! whether each change is accepted. Nothing is stored.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::HashMap, net::SocketAddr, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};
use wire::{Qid, Reader, Stat, Writer};

/// Default silence allowed between messages.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
const DEFAULT_OWNER: &str = "netget";

fn idle(ctx: &SpawnContext) -> Result<Duration> {
    let secs = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Duration::from_secs(secs))
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let idle = idle(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("9P listening on {addr}"));
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
                "9P",
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
                            .warn(format!("9P connection {id} ended: {e}"));
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

/// What a fid names, and what an open fid has fetched.
struct Fid {
    path: String,
    qid: Qid,
    uname: String,
    open: Option<u8>,
    remove_on_clunk: bool,
    /// File content from the last read at offset 0.
    content: Option<Vec<u8>>,
    /// Packed directory entries from the last read at offset 0, and the paging cursor.
    entries: Vec<Vec<u8>>,
    next_index: usize,
    next_offset: u64,
}

impl Fid {
    fn new(path: String, qid: Qid, uname: String) -> Self {
        Self {
            path,
            qid,
            uname,
            open: None,
            remove_on_clunk: false,
            content: None,
            entries: Vec::new(),
            next_index: 0,
            next_offset: 0,
        }
    }
}

fn root_qid() -> Qid {
    Qid {
        kind: wire::QTDIR,
        version: 0,
        path: wire::qid_path("/"),
    }
}

fn qid_of(path: &str, entry: &Value) -> Qid {
    Qid {
        kind: if entry["kind"] == "dir" {
            wire::QTDIR
        } else {
            wire::QTFILE
        },
        version: entry["mtime"].as_u64().unwrap_or(0) as u32,
        path: wire::qid_path(path),
    }
}

fn stat_of(path: &str, name: &str, entry: &Value) -> Stat {
    let dir = entry["kind"] == "dir";
    let perm = entry["mode"]
        .as_u64()
        .unwrap_or(if dir { 0o755 } else { 0o644 }) as u32;
    let owner = entry["owner"].as_str().unwrap_or(DEFAULT_OWNER).to_string();
    let mtime = entry["mtime"].as_u64().unwrap_or(0) as u32;
    Stat {
        kind: 0,
        dev: 0,
        qid: qid_of(path, entry),
        mode: perm | if dir { wire::DMDIR } else { 0 },
        atime: mtime,
        mtime,
        length: if dir {
            0
        } else {
            entry["size"].as_u64().unwrap_or(0)
        },
        name: name.to_string(),
        uid: owner.clone(),
        gid: owner,
        muid: DEFAULT_OWNER.to_string(),
    }
}

/// A request the handler answered, refused, or could not answer.
enum Answer {
    /// The action name and its data.
    Given(String, Value),
    /// Rerror text to send.
    Refused(String),
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("9P connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event_type: &'static EventType,
    data: Value,
    expected: &[&str],
) -> Answer {
    let op = event_type.id.clone();
    let generic = crate::utils::WireFailure::Unavailable
        .prefixed_text()
        .to_string();
    let event = Event::new(event_type, data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::NinepProtocol,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            outcome(ctx, id, &op, "fail_closed_llm_error");
            return Answer::Refused(
                crate::utils::wire_failure::prefixed_wire_failure_text(&error).to_string(),
            );
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, &op, "fail_closed_invalid_reply");
        return Answer::Refused(generic);
    }
    let mut found = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name.starts_with("ninep_") => {
                found.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match found.len() {
        0 => {
            outcome(ctx, id, &op, "model_silent");
            Answer::Refused(generic)
        }
        1 => {
            let (name, data) = found.pop().unwrap_or_default();
            if name == "ninep_error" {
                outcome(ctx, id, &op, "model_reject");
                Answer::Refused(data["message"].as_str().unwrap_or("refused").to_string())
            } else if expected.contains(&name.as_str()) {
                outcome(ctx, id, &op, "model_answer");
                Answer::Given(name, data)
            } else {
                outcome(ctx, id, &op, "fail_closed_invalid_reply");
                Answer::Refused(generic)
            }
        }
        _ => {
            outcome(ctx, id, &op, "fail_closed_invalid_reply");
            Answer::Refused(generic)
        }
    }
}

/// Ask whether `path` exists. `Ok(entry)` or `Err(Rerror text)`.
async fn lookup(
    ctx: &SpawnContext,
    id: ConnectionId,
    path: &str,
    uname: &str,
) -> std::result::Result<Value, String> {
    if path == "/" {
        return Ok(json!({"kind": "dir"}));
    }
    match ask(
        ctx,
        id,
        &actions::STAT_EVENT,
        json!({"path": path, "uname": uname}),
        &["ninep_entry", "ninep_not_found"],
    )
    .await
    {
        Answer::Given(name, data) if name == "ninep_entry" => Ok(data),
        Answer::Given(..) => Err("file does not exist".into()),
        Answer::Refused(text) => Err(text),
    }
}

/// Ask for a change; `Ok(())` when accepted.
async fn change(
    ctx: &SpawnContext,
    id: ConnectionId,
    event_type: &'static EventType,
    data: Value,
) -> std::result::Result<(), String> {
    match ask(ctx, id, event_type, data, &["ninep_ok"]).await {
        Answer::Given(..) => Ok(()),
        Answer::Refused(text) => Err(text),
    }
}

struct Conn<'a> {
    ctx: &'a SpawnContext,
    id: ConnectionId,
    msize: u32,
    negotiated: bool,
    fids: HashMap<u32, Fid>,
}

fn rerror(tag: u16, text: &str) -> Vec<u8> {
    let text: String = text.chars().take(255).collect();
    Writer::new().string(&text).finish(wire::RERROR, tag)
}

impl Conn<'_> {
    fn iounit(&self) -> u32 {
        self.msize - wire::IOHDRSZ
    }

    fn fid(&mut self, fid: u32) -> std::result::Result<&mut Fid, String> {
        self.fids
            .get_mut(&fid)
            .ok_or_else(|| "unknown fid".to_string())
    }

    fn insert(&mut self, fid: u32, value: Fid) -> std::result::Result<(), String> {
        if fid == wire::NOFID {
            return Err("invalid fid".into());
        }
        if !self.fids.contains_key(&fid) && self.fids.len() >= wire::MAX_FIDS {
            return Err("too many fids".into());
        }
        self.fids.insert(fid, value);
        Ok(())
    }

    /// Answer one T-message. `Err` only for a malformed message, which ends the connection.
    async fn handle(&mut self, kind: u8, tag: u16, body: &[u8]) -> Result<Vec<u8>> {
        let mut r = Reader::new(body);
        if kind == wire::TVERSION {
            let msize = r.u32()?;
            let version = r.string()?;
            anyhow::ensure!(msize >= wire::MIN_MSIZE, "msize {msize} is too small");
            self.msize = msize.min(wire::MAX_MSIZE);
            self.fids.clear();
            let reply = if version.starts_with(wire::VERSION) {
                self.negotiated = true;
                wire::VERSION
            } else {
                "unknown"
            };
            return Ok(Writer::new()
                .u32(self.msize)
                .string(reply)
                .finish(wire::RVERSION, tag));
        }
        if !self.negotiated {
            return Ok(rerror(tag, "version not negotiated"));
        }
        let reply = match kind {
            wire::TAUTH => Err("authentication not required".to_string()),
            wire::TATTACH => {
                let fid = r.u32()?;
                let _afid = r.u32()?;
                let uname = r.string()?;
                let _aname = r.string()?;
                if self.fids.contains_key(&fid) {
                    Err("fid already in use".into())
                } else {
                    self.insert(fid, Fid::new("/".into(), root_qid(), uname))
                        .map(|_| Writer::new().qid(&root_qid()).finish(wire::RATTACH, tag))
                }
            }
            wire::TFLUSH => {
                // Requests are answered in order, so whatever was flushed has been answered.
                r.u16()?;
                Ok(Writer::new().finish(wire::RFLUSH, tag))
            }
            wire::TWALK => self.walk(tag, &mut r).await?,
            wire::TOPEN => {
                let fid = r.u32()?;
                let mode = r.u8()?;
                self.open(tag, fid, mode).await
            }
            wire::TCREATE => {
                let fid = r.u32()?;
                let name = r.string()?;
                let perm = r.u32()?;
                let mode = r.u8()?;
                self.create(tag, fid, &name, perm, mode).await
            }
            wire::TREAD => {
                let fid = r.u32()?;
                let offset = r.u64()?;
                let count = r.u32()?;
                self.read(tag, fid, offset, count).await
            }
            wire::TWRITE => {
                let fid = r.u32()?;
                let offset = r.u64()?;
                let count = r.u32()? as usize;
                let data = r.take(count)?;
                self.write(tag, fid, offset, data).await
            }
            wire::TCLUNK => {
                let fid = r.u32()?;
                match self.fids.remove(&fid) {
                    Some(f) => {
                        if f.remove_on_clunk {
                            let _ = change(
                                self.ctx,
                                self.id,
                                &actions::REMOVE_EVENT,
                                json!({"path": f.path, "uname": f.uname}),
                            )
                            .await;
                        }
                        Ok(Writer::new().finish(wire::RCLUNK, tag))
                    }
                    None => Err("unknown fid".into()),
                }
            }
            wire::TREMOVE => {
                let fid = r.u32()?;
                // The fid is clunked whether or not the remove succeeds.
                match self.fids.remove(&fid) {
                    Some(f) if f.path == "/" => Err("cannot remove the root".into()),
                    Some(f) => change(
                        self.ctx,
                        self.id,
                        &actions::REMOVE_EVENT,
                        json!({"path": f.path, "uname": f.uname}),
                    )
                    .await
                    .map(|_| Writer::new().finish(wire::RREMOVE, tag)),
                    None => Err("unknown fid".into()),
                }
            }
            wire::TSTAT => {
                let fid = r.u32()?;
                self.stat(tag, fid).await
            }
            wire::TWSTAT => {
                let fid = r.u32()?;
                let _len = r.u16()?;
                let stat = r.stat()?;
                self.wstat(tag, fid, stat).await
            }
            other => Err(format!("unknown message type {other}")),
        };
        Ok(reply.unwrap_or_else(|text| rerror(tag, &text)))
    }

    async fn walk(
        &mut self,
        tag: u16,
        r: &mut Reader<'_>,
    ) -> Result<std::result::Result<Vec<u8>, String>> {
        let fid = r.u32()?;
        let newfid = r.u32()?;
        let count = r.u16()? as usize;
        anyhow::ensure!(count <= wire::MAX_WELEM, "walk of {count} elements");
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            names.push(r.string()?);
        }
        let (start, uname, open) = match self.fids.get(&fid) {
            Some(f) => (f.path.clone(), f.uname.clone(), f.open.is_some()),
            None => return Ok(Err("unknown fid".into())),
        };
        if open {
            return Ok(Err("cannot walk an open fid".into()));
        }
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Ok(Err("fid already in use".into()));
        }
        let mut path = start.clone();
        let mut qid = match self.fids.get(&fid) {
            Some(f) => f.qid,
            None => root_qid(),
        };
        let mut qids = Vec::new();
        for name in &names {
            if !qid.is_dir() {
                break;
            }
            let next = if name == ".." {
                wire::parent(&path)
            } else if let Err(e) = wire::check_name(name) {
                if qids.is_empty() {
                    return Ok(Err(e.to_string()));
                }
                break;
            } else {
                wire::join(&path, name)
            };
            match lookup(self.ctx, self.id, &next, &uname).await {
                Ok(entry) => {
                    qid = if next == "/" {
                        root_qid()
                    } else {
                        qid_of(&next, &entry)
                    };
                    path = next;
                    qids.push(qid);
                }
                Err(text) if qids.is_empty() => return Ok(Err(text)),
                Err(_) => break,
            }
        }
        if !names.is_empty() && qids.is_empty() {
            return Ok(Err("not a directory".into()));
        }
        if qids.len() == names.len() {
            if let Err(e) = self.insert(newfid, Fid::new(path, qid, uname)) {
                return Ok(Err(e));
            }
        }
        let mut w = Writer::new().u16(qids.len() as u16);
        for q in &qids {
            w = w.qid(q);
        }
        Ok(Ok(w.finish(wire::RWALK, tag)))
    }

    async fn open(&mut self, tag: u16, fid: u32, mode: u8) -> std::result::Result<Vec<u8>, String> {
        let iounit = self.iounit();
        let (ctx, id) = (self.ctx, self.id);
        let f = self.fid(fid)?;
        if f.open.is_some() {
            return Err("fid already open".into());
        }
        let access = mode & 3;
        if f.qid.is_dir()
            && (access == wire::OWRITE || access == wire::ORDWR || mode & wire::OTRUNC != 0)
        {
            return Err("is a directory".into());
        }
        if mode & wire::OTRUNC != 0 {
            change(
                ctx,
                id,
                &actions::WSTAT_EVENT,
                json!({"path": f.path, "length": 0, "uname": f.uname}),
            )
            .await?;
        }
        f.open = Some(access);
        f.remove_on_clunk = mode & wire::ORCLOSE != 0;
        Ok(Writer::new()
            .qid(&f.qid)
            .u32(iounit)
            .finish(wire::ROPEN, tag))
    }

    async fn create(
        &mut self,
        tag: u16,
        fid: u32,
        name: &str,
        perm: u32,
        mode: u8,
    ) -> std::result::Result<Vec<u8>, String> {
        let iounit = self.iounit();
        let (ctx, id) = (self.ctx, self.id);
        wire::check_name(name).map_err(|e| e.to_string())?;
        if name == ".." {
            return Err("invalid name".into());
        }
        let f = self.fid(fid)?;
        if !f.qid.is_dir() || f.open.is_some() {
            return Err("create needs an unopened directory fid".into());
        }
        let dir = perm & wire::DMDIR != 0;
        let path = wire::join(&f.path, name);
        change(
            ctx,
            id,
            &actions::CREATE_EVENT,
            json!({"path": path, "kind": if dir {"dir"} else {"file"}, "mode": perm & 0o777, "uname": f.uname}),
        )
        .await?;
        let entry = json!({"kind": if dir {"dir"} else {"file"}});
        f.qid = qid_of(&path, &entry);
        f.path = path;
        f.open = Some(mode & 3);
        f.remove_on_clunk = mode & wire::ORCLOSE != 0;
        f.content = Some(Vec::new());
        Ok(Writer::new()
            .qid(&f.qid)
            .u32(iounit)
            .finish(wire::RCREATE, tag))
    }

    async fn read(
        &mut self,
        tag: u16,
        fid: u32,
        offset: u64,
        count: u32,
    ) -> std::result::Result<Vec<u8>, String> {
        let count = count.min(self.iounit()) as usize;
        let (ctx, id) = (self.ctx, self.id);
        let f = self.fid(fid)?;
        match f.open {
            Some(wire::OREAD | wire::ORDWR | wire::OEXEC) => {}
            _ => return Err("fid not open for reading".into()),
        }
        if f.qid.is_dir() {
            if offset == 0 {
                let listing = match ask(
                    ctx,
                    id,
                    &actions::LIST_EVENT,
                    json!({"path": f.path, "uname": f.uname}),
                    &["ninep_listing"],
                )
                .await
                {
                    Answer::Given(_, data) => data,
                    Answer::Refused(text) => return Err(text),
                };
                f.entries = listing["entries"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|e| {
                                let name = e["name"].as_str().unwrap_or_default();
                                wire::encode_stat(&stat_of(&wire::join(&f.path, name), name, e))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                f.next_index = 0;
                f.next_offset = 0;
            } else if offset != f.next_offset {
                return Err("bad offset in directory read".into());
            }
            let mut data = Vec::new();
            while let Some(entry) = f.entries.get(f.next_index) {
                if data.len() + entry.len() > count {
                    if data.is_empty() {
                        return Err("read count too small for a directory entry".into());
                    }
                    break;
                }
                data.extend_from_slice(entry);
                f.next_index += 1;
            }
            f.next_offset += data.len() as u64;
            return Ok(Writer::new()
                .u32(data.len() as u32)
                .bytes(&data)
                .finish(wire::RREAD, tag));
        }
        if offset == 0 || f.content.is_none() {
            let answer = ask(
                ctx,
                id,
                &actions::READ_EVENT,
                json!({"path": f.path, "uname": f.uname}),
                &["ninep_content"],
            )
            .await;
            let data = match answer {
                Answer::Given(_, data) => data,
                Answer::Refused(text) => return Err(text),
            };
            let bytes = wire::from_text(
                data["data"].as_str().unwrap_or_default(),
                data["encoding"].as_str(),
            )
            .map_err(|e| e.to_string())?;
            f.content = Some(bytes);
        }
        let content = f.content.as_deref().unwrap_or_default();
        let start = (offset as usize).min(content.len());
        let end = (start + count).min(content.len());
        let slice = &content[start..end];
        Ok(Writer::new()
            .u32(slice.len() as u32)
            .bytes(slice)
            .finish(wire::RREAD, tag))
    }

    async fn write(
        &mut self,
        tag: u16,
        fid: u32,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        let (ctx, id) = (self.ctx, self.id);
        let f = self.fid(fid)?;
        match f.open {
            Some(wire::OWRITE | wire::ORDWR) => {}
            _ => return Err("fid not open for writing".into()),
        }
        if f.qid.is_dir() {
            return Err("is a directory".into());
        }
        if offset as usize + data.len() > wire::MAX_CONTENT {
            return Err("file too large".into());
        }
        let (text, encoding) = wire::to_text(data);
        change(
            ctx,
            id,
            &actions::WRITE_EVENT,
            json!({"path": f.path, "offset": offset, "data": text, "encoding": encoding, "uname": f.uname}),
        )
        .await?;
        f.content = None;
        Ok(Writer::new()
            .u32(data.len() as u32)
            .finish(wire::RWRITE, tag))
    }

    async fn stat(&mut self, tag: u16, fid: u32) -> std::result::Result<Vec<u8>, String> {
        let (ctx, id) = (self.ctx, self.id);
        let f = self.fid(fid)?;
        let (path, uname) = (f.path.clone(), f.uname.clone());
        let entry = lookup(ctx, id, &path, &uname).await?;
        let stat = wire::encode_stat(&stat_of(&path, &wire::base(&path), &entry));
        Ok(Writer::new()
            .u16(stat.len() as u16)
            .bytes(&stat)
            .finish(wire::RSTAT, tag))
    }

    async fn wstat(
        &mut self,
        tag: u16,
        fid: u32,
        stat: Stat,
    ) -> std::result::Result<Vec<u8>, String> {
        let (ctx, id) = (self.ctx, self.id);
        let f = self.fid(fid)?;
        let mut event = json!({"path": f.path, "uname": f.uname});
        let mut changed = false;
        if !stat.name.is_empty() && stat.name != wire::base(&f.path) {
            wire::check_name(&stat.name).map_err(|e| e.to_string())?;
            if f.path == "/" || stat.name == ".." {
                return Err("cannot rename this".into());
            }
            event["name"] = json!(stat.name);
            changed = true;
        }
        if stat.length != u64::MAX {
            if f.qid.is_dir() {
                return Err("is a directory".into());
            }
            event["length"] = json!(stat.length);
            changed = true;
        }
        if stat.mode != u32::MAX {
            event["mode"] = json!(stat.mode & 0o777);
            changed = true;
        }
        // A wstat of all "don't touch" values is a sync request: nothing to decide.
        if changed {
            change(ctx, id, &actions::WSTAT_EVENT, event).await?;
            if !stat.name.is_empty() && stat.name != wire::base(&f.path) {
                f.path = wire::join(&wire::parent(&f.path), &stat.name);
                f.qid.path = wire::qid_path(&f.path);
            }
            f.content = None;
        }
        Ok(Writer::new().finish(wire::RWSTAT, tag))
    }
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    idle: Duration,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    let mut conn = Conn {
        ctx,
        id,
        msize: wire::MAX_MSIZE,
        negotiated: false,
        fids: HashMap::new(),
    };
    loop {
        let Some((kind, tag, body)) = wire::read_message(&mut reader, conn.msize, idle).await?
        else {
            return Ok(());
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(body.len() as u64 + 7),
                None,
                Some(1),
                None,
            )
            .await;
        let reply = conn.handle(kind, tag, &body).await?;
        tokio::time::timeout(wire::IO_TIMEOUT, writer.write_all(&reply))
            .await
            .context("9P write deadline")??;
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
}
