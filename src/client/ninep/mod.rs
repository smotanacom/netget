//! 9P2000 client. One connection, attached once; each action walks from the root fid to a
//! fresh fid, does its work and clunks it.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::ninep::wire::{self, Reader, Stat, Writer};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::NinepClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};

pub const DEFAULT_UNAME: &str = "netget";
const ROOT_FID: u32 = 0;

/// An error the server answered with (Rerror): the operation failed, the session goes on.
#[derive(Debug)]
pub struct Remote(pub String);
impl std::fmt::Display for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Remote {}

fn remote(text: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Remote(text.into()))
}

pub struct Conn {
    reader: ReadHalf<TcpStream>,
    writer: WriteHalf<TcpStream>,
    msize: u32,
    next_tag: u16,
    next_fid: u32,
}

impl Conn {
    async fn rpc(&mut self, kind: u8, body: Writer) -> Result<(u8, Vec<u8>)> {
        let tag = if kind == wire::TVERSION {
            wire::NOTAG
        } else {
            self.next_tag = self.next_tag.wrapping_add(1) % wire::NOTAG;
            self.next_tag
        };
        let message = body.finish(kind, tag);
        anyhow::ensure!(
            message.len() as u32 <= self.msize,
            "9P request exceeds msize"
        );
        tokio::time::timeout(wire::IO_TIMEOUT, self.writer.write_all(&message))
            .await
            .context("9P write deadline")??;
        let (rkind, rtag, rbody) =
            wire::read_message(&mut self.reader, self.msize, wire::IO_TIMEOUT)
                .await?
                .context("9P server closed the connection")?;
        anyhow::ensure!(rtag == tag, "9P reply tag {rtag} does not answer {tag}");
        if rkind == wire::RERROR {
            return Err(remote(Reader::new(&rbody).string()?));
        }
        anyhow::ensure!(
            rkind == kind + 1,
            "9P reply type {rkind} does not answer {kind}"
        );
        Ok((rkind, rbody))
    }

    fn fid(&mut self) -> u32 {
        self.next_fid = if self.next_fid >= 0x7fff_ffff {
            1
        } else {
            self.next_fid + 1
        };
        self.next_fid
    }

    async fn clunk(&mut self, fid: u32) {
        let _ = self.rpc(wire::TCLUNK, Writer::new().u32(fid)).await;
    }

    /// Walk from the root to `path`, in steps of at most 16 elements, onto a new fid.
    async fn walk(&mut self, path: &str) -> Result<u32> {
        let elements = wire::elements(path)?;
        let fid = self.fid();
        let mut from = ROOT_FID;
        let mut chunks: Vec<&[String]> = elements.chunks(wire::MAX_WELEM).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        for chunk in chunks {
            let mut w = Writer::new().u32(from).u32(fid).u16(chunk.len() as u16);
            for name in chunk {
                w = w.string(name);
            }
            let result = self.rpc(wire::TWALK, w).await;
            let walked = match result {
                Ok((_, body)) => Reader::new(&body).u16()? as usize,
                Err(e) => {
                    if from == fid {
                        self.clunk(fid).await;
                    }
                    return Err(e);
                }
            };
            if walked != chunk.len() {
                if from == fid {
                    self.clunk(fid).await;
                }
                return Err(remote("file does not exist"));
            }
            from = fid;
        }
        Ok(fid)
    }

    /// Open `fid` and return the I/O unit to read or write in.
    async fn open(&mut self, fid: u32, mode: u8) -> Result<u32> {
        let (_, body) = self
            .rpc(wire::TOPEN, Writer::new().u32(fid).u8(mode))
            .await?;
        let mut r = Reader::new(&body);
        r.qid()?;
        Ok(self.iounit(r.u32()?))
    }

    fn iounit(&self, offered: u32) -> u32 {
        let max = self.msize - wire::IOHDRSZ;
        if offered == 0 {
            max
        } else {
            offered.min(max)
        }
    }

    async fn read_all(&mut self, fid: u32, iounit: u32) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let (_, body) = self
                .rpc(
                    wire::TREAD,
                    Writer::new().u32(fid).u64(data.len() as u64).u32(iounit),
                )
                .await?;
            let mut r = Reader::new(&body);
            let count = r.u32()? as usize;
            anyhow::ensure!(
                count <= iounit as usize,
                "9P server returned more than asked"
            );
            if count == 0 {
                return Ok(data);
            }
            data.extend_from_slice(r.take(count)?);
            anyhow::ensure!(data.len() <= wire::MAX_CONTENT, "9P content exceeds 1 MiB");
        }
    }

    async fn stat(&mut self, fid: u32) -> Result<Stat> {
        let (_, body) = self.rpc(wire::TSTAT, Writer::new().u32(fid)).await?;
        let mut r = Reader::new(&body);
        r.u16()?;
        r.stat()
    }

    /// Run one action and return its result fields (merged into ninep_result).
    async fn run(&mut self, action: &Value) -> Result<Value> {
        let path = action["path"].as_str().unwrap_or("/");
        match action["type"].as_str().unwrap_or_default() {
            "ninep_ls" => {
                let fid = self.walk(path).await?;
                let result = async {
                    let iounit = self.open(fid, wire::OREAD).await?;
                    let data = self.read_all(fid, iounit).await?;
                    let mut r = Reader::new(&data);
                    let mut entries = Vec::new();
                    while !r.is_empty() {
                        anyhow::ensure!(
                            entries.len() < wire::MAX_ENTRIES,
                            "listing exceeds 1024 entries"
                        );
                        entries.push(stat_json(&r.stat()?));
                    }
                    Ok(json!({"entries": entries}))
                }
                .await;
                self.clunk(fid).await;
                result
            }
            "ninep_cat" => {
                let fid = self.walk(path).await?;
                let result = async {
                    let iounit = self.open(fid, wire::OREAD).await?;
                    let data = self.read_all(fid, iounit).await?;
                    let (text, encoding) = wire::to_text(&data);
                    Ok(json!({"data": text, "encoding": encoding}))
                }
                .await;
                self.clunk(fid).await;
                result
            }
            "ninep_stat" => {
                let fid = self.walk(path).await?;
                let result = self.stat(fid).await;
                self.clunk(fid).await;
                Ok(json!({"stat": stat_json(&result?)}))
            }
            "ninep_write" => self.write(path, action).await,
            "ninep_mkdir" => {
                let fid = self.walk(&wire::parent(path)).await?;
                let result = self
                    .rpc(
                        wire::TCREATE,
                        Writer::new()
                            .u32(fid)
                            .string(&wire::base(path))
                            .u32(wire::DMDIR | 0o755)
                            .u8(wire::OREAD),
                    )
                    .await;
                self.clunk(fid).await;
                result.map(|_| json!({}))
            }
            "ninep_remove" => {
                let fid = self.walk(path).await?;
                // Tremove clunks the fid whatever the answer.
                self.rpc(wire::TREMOVE, Writer::new().u32(fid)).await?;
                Ok(json!({}))
            }
            "ninep_rename" => {
                let fid = self.walk(path).await?;
                let stat = Stat {
                    name: action["name"].as_str().unwrap_or_default().to_string(),
                    ..Stat::dont_touch()
                };
                let encoded = wire::encode_stat(&stat);
                let result = self
                    .rpc(
                        wire::TWSTAT,
                        Writer::new()
                            .u32(fid)
                            .u16(encoded.len() as u16)
                            .bytes(&encoded),
                    )
                    .await;
                self.clunk(fid).await;
                result.map(|_| json!({}))
            }
            other => anyhow::bail!("Unknown 9P client action {other}"),
        }
    }

    async fn write(&mut self, path: &str, action: &Value) -> Result<Value> {
        let data = wire::from_text(
            action["data"].as_str().unwrap_or_default(),
            action["encoding"].as_str(),
        )?;
        let append = action["append"] == true;
        let (fid, iounit) = match self.walk(path).await {
            Ok(fid) => {
                let mode = if append {
                    wire::OWRITE
                } else {
                    wire::OWRITE | wire::OTRUNC
                };
                match self.open(fid, mode).await {
                    Ok(iounit) => (fid, iounit),
                    Err(e) => {
                        self.clunk(fid).await;
                        return Err(e);
                    }
                }
            }
            Err(e) if action["create"] == true && e.downcast_ref::<Remote>().is_some() => {
                let fid = self.walk(&wire::parent(path)).await?;
                match self
                    .rpc(
                        wire::TCREATE,
                        Writer::new()
                            .u32(fid)
                            .string(&wire::base(path))
                            .u32(0o644)
                            .u8(wire::OWRITE),
                    )
                    .await
                {
                    Ok((_, body)) => {
                        let mut r = Reader::new(&body);
                        r.qid()?;
                        let iounit = self.iounit(r.u32()?);
                        (fid, iounit)
                    }
                    Err(e) => {
                        self.clunk(fid).await;
                        return Err(e);
                    }
                }
            }
            Err(e) => return Err(e),
        };
        let result = async {
            let mut offset = if append {
                self.stat(fid).await?.length
            } else {
                0
            };
            let mut written = 0usize;
            for chunk in data
                .chunks(iounit as usize)
                .chain(data.is_empty().then_some(&[][..]))
            {
                let (_, body) = self
                    .rpc(
                        wire::TWRITE,
                        Writer::new()
                            .u32(fid)
                            .u64(offset)
                            .u32(chunk.len() as u32)
                            .bytes(chunk),
                    )
                    .await?;
                let count = Reader::new(&body).u32()? as usize;
                anyhow::ensure!(
                    count <= chunk.len(),
                    "9P server wrote more than it was sent"
                );
                written += count;
                offset += count as u64;
                if count < chunk.len() {
                    break;
                }
            }
            Ok(json!({"bytes_written": written}))
        }
        .await;
        self.clunk(fid).await;
        result
    }
}

fn stat_json(s: &Stat) -> Value {
    json!({
        "name": s.name,
        "kind": if s.mode & wire::DMDIR != 0 { "dir" } else { "file" },
        "size": s.length,
        "mode": s.mode & 0o777,
        "owner": s.uid,
        "mtime": s.mtime,
    })
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let uname = params
        .map(|p| p.get_optional_string("uname"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_UNAME.to_string());
    let aname = params
        .map(|p| p.get_optional_string("aname"))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("9P connect deadline")??;
    let local = stream.local_addr()?;
    let (reader, writer) = tokio::io::split(stream);
    let mut conn = Conn {
        reader,
        writer,
        msize: wire::MAX_MSIZE,
        next_tag: 0,
        next_fid: ROOT_FID,
    };
    let (_, body) = conn
        .rpc(
            wire::TVERSION,
            Writer::new().u32(wire::MAX_MSIZE).string(wire::VERSION),
        )
        .await?;
    let mut r = Reader::new(&body);
    let msize = r.u32()?;
    let version = r.string()?;
    anyhow::ensure!(
        version == wire::VERSION,
        "9P server offered version {version:?}"
    );
    anyhow::ensure!(
        (wire::MIN_MSIZE..=wire::MAX_MSIZE).contains(&msize),
        "9P server offered msize {msize}"
    );
    conn.msize = msize;
    conn.rpc(
        wire::TATTACH,
        Writer::new()
            .u32(ROOT_FID)
            .u32(wire::NOFID)
            .string(&uname)
            .string(&aname),
    )
    .await
    .map_err(|e| anyhow::anyhow!("9P attach refused: {e}"))?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"version": version, "msize": msize, "uname": uname}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = NinepClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("9P client handler: {e}"))
                }
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, conn, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("9P client ended: {e}"));
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

async fn session(
    ctx: &ConnectContext,
    mut conn: Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        match NinepClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                conn.clunk(ROOT_FID).await;
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        }
        let op = action["type"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("ninep_")
            .to_string();
        let path = action["path"].as_str().unwrap_or("/").to_string();
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "9P",
                    None,
                    "injected_action",
                    json!({"op": op, "path": path}),
                    vec![],
                )
                .await;
        }
        let mut event = json!({"op": op, "path": path});
        match conn.run(&action).await {
            Ok(fields) => {
                event["ok"] = json!(true);
                if let Some(map) = fields.as_object() {
                    for (k, v) in map {
                        event[k] = v.clone();
                    }
                }
            }
            Err(e) => match e.downcast_ref::<Remote>() {
                Some(Remote(text)) => {
                    event["ok"] = json!(false);
                    event["error"] = json!(text);
                }
                // A transport or framing failure ends the session.
                None => {
                    if let Some(command) = injected.take() {
                        crate::client::command_support::reply(
                            command,
                            Err(anyhow::anyhow!(e.to_string())),
                        );
                    }
                    return Err(e);
                }
            },
        }
        if let Some(command) = injected.take() {
            crate::client::command_support::reply(
                command,
                Ok(ClientSendOutcome::Executed {
                    detail: event.to_string(),
                }),
            );
        }
        events
            .try_send(Event::new(&actions::RESULT_EVENT, event))
            .context("9P event queue full; consumer stalled")?;
    }
}
