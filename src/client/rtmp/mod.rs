//! RTMP client over the server's handshake, chunk and AMF0 code.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::rtmp::chunk::{self, Message};
use crate::server::rtmp::{amf0, AbortOnDrop};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use crate::utils::clock::Instant;
pub use actions::RtmpClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_APP: &str = "live";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FLV: u64 = 64 * 1024 * 1024;
const OUT_CHUNK_SIZE: usize = 4096;
const MAX_DATA_MESSAGES: usize = 20;

struct Conn {
    /// Where `rtmp_publish` may read FLV files from; see `client::media_root`.
    media_root: crate::client::media_root::MediaRoot,
    w: WriteHalf<TcpStream>,
    msgs: mpsc::Receiver<Result<(Message, u64)>>,
    writer: chunk::Writer,
    window: u32,
    acked: u64,
    next_tx: f64,
    _reader: AbortOnDrop,
}

impl Conn {
    async fn send(&mut self, csid: u32, m: &Message) -> Result<()> {
        let bytes = self.writer.encode(csid, m)?;
        self.w.write_all(&bytes).await?;
        Ok(())
    }

    async fn command(&mut self, stream_id: u32, name: &str, args: &[Value]) -> Result<f64> {
        self.next_tx += 1.0;
        let tx = if matches!(name, "play" | "publish" | "deleteStream" | "closeStream") {
            0.0
        } else {
            self.next_tx
        };
        let mut values = vec![json!(name), json!(tx)];
        values.extend_from_slice(args);
        self.send(
            3,
            &Message {
                type_id: chunk::COMMAND_AMF0,
                stream_id,
                timestamp: 0,
                payload: amf0::encode(&values)?,
            },
        )
        .await?;
        Ok(tx)
    }

    /// The next message from the server, answering protocol control on the way; None on close.
    async fn next(&mut self, deadline: Duration) -> Result<Option<Message>> {
        loop {
            let m = match tokio::time::timeout(deadline, self.msgs.recv()).await {
                Err(_) => bail!("the server sent nothing for {deadline:?}"),
                Ok(None) => return Ok(None),
                Ok(Some(m)) => m?,
            };
            let (m, total) = m;
            if total - self.acked >= self.window as u64 {
                self.acked = total;
                self.send(
                    2,
                    &chunk::control(chunk::ACK, (total as u32).to_be_bytes().to_vec()),
                )
                .await?;
            }
            match m.type_id {
                chunk::WINDOW_ACK_SIZE => {
                    self.window = u32::from_be_bytes(
                        m.payload
                            .get(..4)
                            .context("short Window Acknowledgement Size")?
                            .try_into()?,
                    )
                    .max(4096);
                }
                chunk::USER_CONTROL
                    if m.payload.get(..2) == Some(&[0, 6]) && m.payload.len() >= 6 =>
                {
                    let t = u32::from_be_bytes(m.payload[2..6].try_into()?);
                    self.send(2, &chunk::user_control(7, t)).await?;
                }
                chunk::SET_CHUNK_SIZE
                | chunk::ACK
                | chunk::SET_PEER_BANDWIDTH
                | chunk::ABORT
                | chunk::USER_CONTROL => {}
                _ => return Ok(Some(m)),
            }
        }
    }

    /// Wait for `_result` / `_error` to transaction `tx`, keeping onStatus codes seen on the way.
    async fn result(&mut self, tx: f64, codes: &mut Vec<String>) -> Result<Vec<Value>> {
        // One deadline for the whole wait: a server still streaming media would otherwise keep
        // a per-message timeout from ever firing.
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !left.is_zero(),
                "no answer to the command within {TIMEOUT:?}"
            );
            let m = self
                .next(left)
                .await?
                .context("the server closed the connection")?;
            if m.type_id != chunk::COMMAND_AMF0 {
                continue;
            }
            let v = amf0::decode_all(&m.payload)?;
            match v.first().and_then(Value::as_str) {
                Some("_result" | "_error") if v.get(1).and_then(Value::as_f64) == Some(tx) => {
                    return Ok(v)
                }
                Some("onStatus") => {
                    codes.extend(v.get(3).and_then(|i| i["code"].as_str()).map(str::to_owned))
                }
                _ => {}
            }
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let app = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string("app"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_APP.to_owned());
    ensure!(
        !app.is_empty() && app.len() <= 256 && !crate::utils::sanitize::has_controls(&app),
        "app is a name"
    );
    let media_root = crate::client::media_root::MediaRoot::new(
        ctx.startup_params
            .as_ref()
            .map(|p| p.get_optional_string(crate::client::media_root::MEDIA_ROOT_PARAM))
            .transpose()?
            .flatten()
            .as_deref(),
    )?;
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("RTMP connect timed out")??;
    let local = stream.local_addr()?;
    tokio::time::timeout(TIMEOUT, chunk::connect(&mut stream))
        .await
        .context("RTMP handshake timed out")??;
    let (mut r, w) = tokio::io::split(stream);
    let (msg_tx, msgs) = mpsc::channel::<Result<(Message, u64)>>(256);
    let reader = AbortOnDrop(tokio::spawn(async move {
        let mut reader = chunk::Reader::default();
        loop {
            let m = reader.read(&mut r).await;
            let failed = m.is_err();
            if let Ok(m) = &m {
                if m.type_id == chunk::SET_CHUNK_SIZE {
                    if let Err(e) = reader.set_chunk_size(&m.payload) {
                        let _ = msg_tx.send(Err(e)).await;
                        return;
                    }
                }
            }
            if msg_tx.send(m.map(|m| (m, reader.bytes))).await.is_err() || failed {
                return;
            }
        }
    }));
    let mut conn = Conn {
        media_root,
        w,
        msgs,
        writer: chunk::Writer::default(),
        window: 2_500_000,
        acked: 0,
        next_tx: 0.0,
        _reader: reader,
    };
    conn.send(
        2,
        &chunk::control(
            chunk::SET_CHUNK_SIZE,
            (OUT_CHUNK_SIZE as u32).to_be_bytes().to_vec(),
        ),
    )
    .await?;
    conn.writer.chunk_size = OUT_CHUNK_SIZE;
    let tc_url = format!("rtmp://{}/{app}", ctx.remote_addr);
    let tx = conn.command(0, "connect", &[json!({"app": app, "type": "nonprivate", "flashVer": "FMLE/3.0 (compatible; NetGet)", "tcUrl": tc_url})]).await?;
    let reply = conn.result(tx, &mut vec![]).await?;
    if reply[0] == "_error" {
        bail!(
            "connect refused: {}",
            reply
                .get(3)
                .and_then(|i| i["description"].as_str().or(i["code"].as_str()))
                .unwrap_or("no reason given")
        );
    }
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"app": app, "server": reply.get(2).cloned().unwrap_or(Value::Null)}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = RtmpClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("RTMP client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &mut conn, external, internal_rx, &event_tx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("RTMP client ended: {e:#}"));
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
    conn: &mut Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
            m = conn.msgs.recv() => match m {
                None | Some(Err(_)) => return Ok(()),
                // Between operations only control traffic is expected; nothing to report.
                Some(Ok(_)) => continue,
            },
        };
        let outcome = match RtmpClientProtocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => {
                let report = if action["type"] == "rtmp_play" {
                    play(conn, &action).await
                } else {
                    publish(conn, &action).await
                };
                match report {
                    Ok(r) => {
                        events
                            .send(Event::new(&actions::REPORT_EVENT, r))
                            .await
                            .ok();
                        Ok(ClientSendOutcome::Sent { bytes_sent: 0 })
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Some(c) = command {
            let logged = outcome
                .as_ref()
                .map(|o| serde_json::to_value(o).unwrap_or(Value::Null))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "RTMP",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "stream": action["stream"]}),
                    vec![logged],
                )
                .await;
            crate::client::command_support::reply(c, outcome);
        } else if let Err(e) = outcome {
            Log::new(Some(&ctx.status_tx)).warn(format!("RTMP action failed: {e:#}"));
        }
    }
}

fn video_codec(b: &[u8]) -> Option<String> {
    let b0 = *b.first()?;
    Some(if b0 & 0x80 != 0 {
        match b.get(1..5)? {
            b"avc1" => "avc".into(),
            b"hvc1" => "hevc".into(),
            b"av01" => "av1".into(),
            b"vp09" => "vp9".into(),
            other => String::from_utf8_lossy(other).into_owned(),
        }
    } else {
        match b0 & 0x0f {
            7 => "avc".into(),
            12 => "hevc".into(),
            n => format!("flv-{n}"),
        }
    })
}

fn audio_codec(b: &[u8]) -> Option<String> {
    Some(match b.first()? >> 4 {
        10 => "aac".into(),
        2 => "mp3".into(),
        n => format!("flv-{n}"),
    })
}

#[derive(Default)]
struct Tally {
    codes: Vec<String>,
    video: u64,
    audio: u64,
    keyframes: u64,
    video_codec: Option<String>,
    audio_codec: Option<String>,
    metadata: Option<Value>,
    first: Option<u32>,
    last: Option<u32>,
    data: Vec<Value>,
}

impl Tally {
    fn media(&mut self, type_id: u8, ts: u32, payload: &[u8]) {
        self.first.get_or_insert(ts);
        self.last = Some(ts);
        if type_id == chunk::VIDEO {
            self.video += 1;
            if payload.first().is_some_and(|b| (b >> 4) & 0x07 == 1) {
                self.keyframes += 1;
            }
            if self.video_codec.is_none() {
                self.video_codec = video_codec(payload);
            }
        } else {
            self.audio += 1;
            if self.audio_codec.is_none() {
                self.audio_codec = audio_codec(payload);
            }
        }
    }
    fn report(self, operation: &str, stream: &str) -> Value {
        json!({"operation": operation, "stream": stream, "status_codes": self.codes, "video_messages": self.video, "audio_messages": self.audio, "keyframes": self.keyframes,
               "video_codec": self.video_codec, "audio_codec": self.audio_codec, "metadata": self.metadata, "first_timestamp": self.first, "last_timestamp": self.last, "data_messages": self.data})
    }
}

async fn create_stream(conn: &mut Conn, codes: &mut Vec<String>) -> Result<u32> {
    let tx = conn.command(0, "createStream", &[Value::Null]).await?;
    let r = conn.result(tx, codes).await?;
    ensure!(r[0] == "_result", "createStream refused");
    r.get(3)
        .and_then(Value::as_f64)
        .map(|f| f as u32)
        .context("createStream gave no stream ID")
}

fn is_error(code: &str) -> bool {
    code.contains("NotFound")
        || code.contains("Failed")
        || code.contains("Denied")
        || code.contains("BadName")
        || code.contains("Rejected")
}

async fn play(conn: &mut Conn, a: &Value) -> Result<Value> {
    let stream = a["stream"].as_str().unwrap_or_default().to_owned();
    let seconds = a["seconds"].as_f64().unwrap_or(5.0);
    let mut t = Tally::default();
    let sid = create_stream(conn, &mut t.codes).await?;
    conn.command(sid, "play", &[Value::Null, json!(stream), json!(-2000.0)])
        .await?;
    let mut buffer = 3u16.to_be_bytes().to_vec();
    buffer.extend(sid.to_be_bytes());
    buffer.extend(3000u32.to_be_bytes());
    conn.send(2, &chunk::control(chunk::USER_CONTROL, buffer))
        .await?;
    let until = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let m = match tokio::time::timeout(left, conn.next(Duration::from_secs(3600))).await {
            Err(_) => break,
            Ok(m) => match m? {
                Some(m) => m,
                None => break,
            },
        };
        match m.type_id {
            chunk::AUDIO | chunk::VIDEO => t.media(m.type_id, m.timestamp, &m.payload),
            chunk::COMMAND_AMF0 => {
                let v = amf0::decode_all(&m.payload)?;
                if v.first().and_then(Value::as_str) == Some("onStatus") {
                    if let Some(code) = v.get(3).and_then(|i| i["code"].as_str()) {
                        t.codes.push(code.to_owned());
                        if is_error(code) {
                            break;
                        }
                    }
                }
            }
            chunk::DATA_AMF0 => {
                let v = amf0::decode_all(&m.payload)?;
                match v.first().and_then(Value::as_str) {
                    Some("onMetaData") => t.metadata = v.get(1).cloned(),
                    Some("|RtmpSampleAccess") => {}
                    Some(h) if t.data.len() < MAX_DATA_MESSAGES => {
                        t.data.push(json!({"handler": h, "data": v.get(1)}))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    conn.command(0, "deleteStream", &[Value::Null, json!(sid)])
        .await?;
    Ok(t.report("play", &stream))
}

/// One FLV tag: (type, timestamp, data).
fn flv_tags(file: &[u8]) -> Result<Vec<(u8, u32, &[u8])>> {
    ensure!(
        file.len() >= 13 && &file[..3] == b"FLV" && file[3] == 1,
        "not an FLV version 1 file"
    );
    let header = u32::from_be_bytes(file[5..9].try_into()?) as usize;
    let mut i = header.checked_add(4).context("bad FLV header size")?;
    let mut out = Vec::new();
    while i + 11 <= file.len() {
        let kind = file[i] & 0x1f;
        let size = u32::from_be_bytes([0, file[i + 1], file[i + 2], file[i + 3]]) as usize;
        let ts = u32::from_be_bytes([file[i + 7], file[i + 4], file[i + 5], file[i + 6]]);
        let data = file
            .get(i + 11..i + 11 + size)
            .context("FLV tag runs past the end of the file")?;
        ensure!(size <= chunk::MAX_MESSAGE, "FLV tag over the message bound");
        out.push((kind, ts, data));
        i += 11 + size + 4;
    }
    Ok(out)
}

async fn publish(conn: &mut Conn, a: &Value) -> Result<Value> {
    let stream = a["stream"].as_str().unwrap_or_default().to_owned();
    let path = a["flv_file"].as_str().unwrap_or_default();
    let realtime = a["realtime"].as_bool().unwrap_or(true);
    // The peer this file goes to is the peer whose responses the model reads: confined
    // to `media_root`, or it is a file-exfiltration primitive. See `client::media_root`.
    let path = conn
        .media_root
        .resolve(path, "the rtmp_publish 'flv_file'")?;
    let meta = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(meta.len() <= MAX_FLV, "{} is over 64 MiB", path.display());
    let file = tokio::fs::read(&path).await?;
    let tags = flv_tags(&file)?;
    let mut t = Tally::default();
    conn.command(0, "releaseStream", &[Value::Null, json!(stream)])
        .await?;
    conn.command(0, "FCPublish", &[Value::Null, json!(stream)])
        .await?;
    let sid = create_stream(conn, &mut t.codes).await?;
    conn.command(sid, "publish", &[Value::Null, json!(stream), json!("live")])
        .await?;
    loop {
        let m = conn
            .next(TIMEOUT)
            .await?
            .context("the server closed the connection")?;
        if m.type_id != chunk::COMMAND_AMF0 {
            continue;
        }
        let v = amf0::decode_all(&m.payload)?;
        if v.first().and_then(Value::as_str) != Some("onStatus") {
            continue;
        }
        let code = v
            .get(3)
            .and_then(|i| i["code"].as_str())
            .unwrap_or_default()
            .to_owned();
        t.codes.push(code.clone());
        if is_error(&code) {
            return Ok(t.report("publish", &stream));
        }
        if code == "NetStream.Publish.Start" {
            break;
        }
    }
    let started = Instant::now();
    let first = tags
        .iter()
        .find(|(k, _, _)| *k == 8 || *k == 9)
        .map(|(_, ts, _)| *ts)
        .unwrap_or(0);
    for (kind, ts, data) in tags {
        let (type_id, csid, payload) = match kind {
            8 => (chunk::AUDIO, 4, data.to_vec()),
            9 => (chunk::VIDEO, 6, data.to_vec()),
            18 => {
                let v = amf0::decode_all(data)?;
                if v.first().and_then(Value::as_str) != Some("onMetaData") {
                    continue;
                }
                let meta: Map<String, Value> = v
                    .get(1)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                t.metadata = Some(Value::Object(meta.clone()));
                let mut p = amf0::encode(&[json!("@setDataFrame"), json!("onMetaData")])?;
                p.extend(amf0::encode_ecma(&meta)?);
                (chunk::DATA_AMF0, 5, p)
            }
            _ => continue,
        };
        if realtime && type_id != chunk::DATA_AMF0 {
            let due = Duration::from_millis(ts.saturating_sub(first) as u64);
            let elapsed = started.elapsed();
            if due > elapsed {
                tokio::time::sleep(due - elapsed).await;
            }
        }
        if type_id != chunk::DATA_AMF0 {
            t.media(type_id, ts, &payload);
        }
        conn.send(
            csid,
            &Message {
                type_id,
                stream_id: sid,
                timestamp: ts,
                payload,
            },
        )
        .await?;
        // Keep acknowledging while publishing.
        while let Ok(Ok((m, total))) = conn.msgs.try_recv() {
            if total - conn.acked >= conn.window as u64 {
                conn.acked = total;
                conn.send(
                    2,
                    &chunk::control(chunk::ACK, (total as u32).to_be_bytes().to_vec()),
                )
                .await?;
            }
            if m.type_id == chunk::COMMAND_AMF0 {
                if let Some(code) = amf0::decode_all(&m.payload)?
                    .get(3)
                    .and_then(|i| i["code"].as_str())
                {
                    t.codes.push(code.to_owned());
                }
            }
        }
    }
    conn.command(0, "FCUnpublish", &[Value::Null, json!(stream)])
        .await?;
    conn.command(0, "deleteStream", &[Value::Null, json!(sid)])
        .await?;
    Ok(t.report("publish", &stream))
}
