//! RTSP client: one control connection to a media server, requests made on the handler's
//! say, UDP RTP/RTCP port pairs bound per SETUP, and the media that arrives summarised per
//! stream by the RTP client's tracker.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::client::rtp::Tracker;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::RtspClientProtocol;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// A response's status line and headers at most.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// A response body (an SDP) at most.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Requests awaiting an answer at once.
pub const MAX_PENDING: usize = 16;
/// Tracks one session may set up.
pub const MAX_TRACKS: usize = 8;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("NetGet/", env!("CARGO_PKG_VERSION"));

pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Read one RTSP response, bounded in header and body size.
pub async fn read_response<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
) -> Result<Option<Response>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line).await?;
        if n == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            bail!("the connection closed inside a response");
        }
        if head.is_empty() && line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        if head.len() + line.len() > MAX_HEADER_BYTES {
            bail!("response headers longer than {MAX_HEADER_BYTES} bytes");
        }
        let blank = line == b"\r\n" || line == b"\n";
        head.extend_from_slice(&line);
        if blank {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("RTSP/") {
        bail!("not an RTSP response: {status_line}");
    }
    let status: u16 = parts
        .next()
        .unwrap_or_default()
        .parse()
        .context("bad status code")?;
    let reason = parts.next().unwrap_or_default().trim().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_BODY_BYTES {
        bail!("response body of {len} bytes (at most {MAX_BODY_BYTES})");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(Response {
        status,
        reason,
        headers,
        body,
    }))
}

/// The parts of an SDP the model needs: each media section, and the session-level control.
pub fn parse_sdp(sdp: &str) -> Value {
    let mut media: Vec<Value> = Vec::new();
    let mut session_control = Value::Null;
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            let f: Vec<&str> = m.split_whitespace().collect();
            media.push(json!({"type": f.first(), "port": f.get(1).and_then(|p| p.parse::<u64>().ok()),
                              "protocol": f.get(2), "formats": f.get(3..).unwrap_or_default(), "rtpmap": {}, "control": null}));
        } else if let Some(a) = line.strip_prefix("a=") {
            let target = media.last_mut();
            if let Some(c) = a.strip_prefix("control:") {
                match target {
                    Some(m) => m["control"] = json!(c),
                    None => session_control = json!(c),
                }
            } else if let (Some(r), Some(m)) = (a.strip_prefix("rtpmap:"), target) {
                if let Some((pt, enc)) = r.split_once(' ') {
                    m["rtpmap"][pt] = json!(enc);
                }
            }
        }
    }
    json!({"media": media, "control": session_control})
}

/// A control URL resolved against the base (Content-Base, else the request URL).
pub fn resolve(base: &str, control: &str) -> String {
    if control.starts_with("rtsp://") || control.starts_with("rtsps://") {
        control.to_string()
    } else if control == "*" || control.is_empty() {
        base.to_string()
    } else if base.ends_with('/') {
        format!("{base}{control}")
    } else {
        format!("{base}/{control}")
    }
}

/// An RTP/RTCP pair on consecutive ports, RTP's even (RFC 3550 §11).
async fn port_pair(ip: IpAddr) -> Result<(UdpSocket, UdpSocket)> {
    for _ in 0..64 {
        let rtp = UdpSocket::bind((ip, 0)).await?;
        let port = rtp.local_addr()?.port();
        if port % 2 != 0 || port == u16::MAX {
            continue;
        }
        if let Ok(rtcp) = UdpSocket::bind((ip, port + 1)).await {
            return Ok((rtp, rtcp));
        }
    }
    bail!("no free even/odd UDP port pair")
}

struct Pending {
    method: &'static str,
    depth: usize,
    caller: Option<ClientCommand>,
    /// For SETUP: the sockets the Transport header offered.
    sockets: Option<(UdpSocket, UdpSocket)>,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let url = if ctx.remote_addr.starts_with("rtsp://") {
        ctx.remote_addr.clone()
    } else {
        format!("rtsp://{}", ctx.remote_addr)
    };
    let parsed =
        reqwest::Url::parse(&url).context("remote_addr must be an rtsp:// URL or host:port")?;
    let host = parsed
        .host_str()
        .context("the URL names no host")?
        .to_string();
    let port = parsed.port().unwrap_or(554);
    let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect((host.as_str(), port)))
        .await
        .context("RTSP connect deadline")??;
    let local = stream.local_addr()?;
    let (reader, writer) = tokio::io::split(stream);
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (resp_tx, resp_rx) = mpsc::channel::<Result<Response>>(16);
    let reader_task = tokio::spawn(async move {
        let mut r = BufReader::new(reader);
        loop {
            let item = read_response(&mut r).await;
            let end = !matches!(item, Ok(Some(_)));
            let sent = match item {
                Ok(Some(resp)) => resp_tx.send(Ok(resp)).await,
                Ok(None) => {
                    resp_tx
                        .send(Err(anyhow::anyhow!("the server closed the connection")))
                        .await
                }
                Err(e) => resp_tx.send(Err(e)).await,
            };
            if end || sent.is_err() {
                return;
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    event_tx.try_send((
        Event::new(&actions::CONNECTED_EVENT, json!({"url": url})),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "RTSP",
                    None,
                    event.id(),
                    event.data.clone(),
                    vec![],
                )
                .await;
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
                &RtspClientProtocol,
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
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("RTSP client handler: {e}"))
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
        let mut s = Session {
            writer,
            url,
            local_ip: local.ip(),
            cseq: 0,
            pending: HashMap::new(),
            session: None,
            base: None,
            tracks: Vec::new(),
            receivers: Vec::new(),
            events: event_tx,
        };
        let result = s.run(&session_ctx, resp_rx, external, internal_rx).await;
        for r in s.receivers.drain(..) {
            r.abort();
        }
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("RTSP client ended: {e:#}"));
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

struct Session {
    writer: WriteHalf<TcpStream>,
    url: String,
    local_ip: IpAddr,
    cseq: u64,
    pending: HashMap<u64, Pending>,
    session: Option<String>,
    base: Option<String>,
    /// Control URLs from the last DESCRIBE, first audio/video first.
    tracks: Vec<String>,
    receivers: Vec<tokio::task::AbortHandle>,
    events: mpsc::Sender<(Event, usize)>,
}

fn reply(caller: Option<ClientCommand>, outcome: ClientSendOutcome) {
    if let Some(c) = caller {
        crate::client::command_support::reply(c, Ok(outcome));
    }
}

impl Session {
    async fn request(
        &mut self,
        method: &'static str,
        uri: &str,
        extra: &[(&str, String)],
        pending: Pending,
    ) -> Result<()> {
        if self.pending.len() >= MAX_PENDING {
            bail!("too many requests awaiting the server");
        }
        self.cseq += 1;
        let mut req = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\n",
            self.cseq
        );
        if let Some(s) = &self.session {
            req.push_str(&format!("Session: {s}\r\n"));
        }
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        tokio::time::timeout(IO_TIMEOUT, self.writer.write_all(req.as_bytes()))
            .await
            .context("RTSP write deadline")??;
        self.pending.insert(self.cseq, pending);
        Ok(())
    }

    async fn act(
        &mut self,
        action: Value,
        depth: usize,
        caller: Option<ClientCommand>,
    ) -> Result<()> {
        let pending = |method, caller| Pending {
            method,
            depth,
            caller,
            sockets: None,
        };
        let url = self.url.clone();
        match action["type"].as_str().unwrap_or_default() {
            actions::OPTIONS => {
                self.request("OPTIONS", &url, &[], pending("OPTIONS", caller))
                    .await
            }
            actions::DESCRIBE => {
                self.request(
                    "DESCRIBE",
                    &url,
                    &[("Accept", "application/sdp".into())],
                    pending("DESCRIBE", caller),
                )
                .await
            }
            actions::SETUP => {
                if self.receivers.len() / 2 >= MAX_TRACKS {
                    reply(
                        caller,
                        ClientSendOutcome::Rejected {
                            error: "too many tracks set up".into(),
                        },
                    );
                    return Ok(());
                }
                let base = self.base.clone().unwrap_or(url);
                let track = match action["track"].as_str() {
                    Some(t) => resolve(&base, t),
                    None => match self.tracks.first() {
                        Some(t) => t.clone(),
                        None => base,
                    },
                };
                let (rtp, rtcp) = port_pair(self.local_ip).await?;
                let p = rtp.local_addr()?.port();
                let mut pend = pending("SETUP", caller);
                pend.sockets = Some((rtp, rtcp));
                self.request(
                    "SETUP",
                    &track,
                    &[(
                        "Transport",
                        format!("RTP/AVP;unicast;client_port={p}-{}", p + 1),
                    )],
                    pend,
                )
                .await
            }
            actions::PLAY => {
                let range: Vec<(&str, String)> = action["range"]
                    .as_str()
                    .map(|r| vec![("Range", r.to_string())])
                    .unwrap_or_default();
                let target = self.base.clone().unwrap_or(url);
                self.request("PLAY", &target, &range, pending("PLAY", caller))
                    .await
            }
            actions::PAUSE => {
                let target = self.base.clone().unwrap_or(url);
                self.request("PAUSE", &target, &[], pending("PAUSE", caller))
                    .await
            }
            actions::TEARDOWN => {
                let target = self.base.clone().unwrap_or(url);
                self.request("TEARDOWN", &target, &[], pending("TEARDOWN", caller))
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn on_response(
        &mut self,
        ctx: &ConnectContext,
        r: Response,
        media: &mpsc::Sender<(Vec<u8>, SocketAddr)>,
    ) -> Result<()> {
        let cseq = r
            .header("CSeq")
            .and_then(|c| c.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let Some(p) = self.pending.remove(&cseq) else {
            return Ok(());
        };
        let mut headers = Map::new();
        for name in [
            "Public",
            "Content-Base",
            "Session",
            "Transport",
            "RTP-Info",
            "Range",
        ] {
            if let Some(v) = r.header(name) {
                headers.insert(name.to_string(), json!(v));
            }
        }
        let mut data =
            json!({"method": p.method, "status": r.status, "reason": r.reason, "headers": headers});
        let ok = (200..300).contains(&r.status);
        if ok {
            if let Some(s) = r.header("Session") {
                self.session = Some(s.split(';').next().unwrap_or_default().trim().to_string());
            }
            match p.method {
                "DESCRIBE" => {
                    let base = r
                        .header("Content-Base")
                        .or(r.header("Content-Location"))
                        .map(str::to_string)
                        .unwrap_or_else(|| self.url.clone());
                    let sdp = parse_sdp(&String::from_utf8_lossy(&r.body));
                    let session_base = sdp["control"]
                        .as_str()
                        .map(|c| resolve(&base, c))
                        .unwrap_or(base.clone());
                    self.tracks = sdp["media"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|m| matches!(m["type"].as_str(), Some("audio" | "video")))
                        .map(|m| resolve(&session_base, m["control"].as_str().unwrap_or("*")))
                        .collect();
                    self.base = Some(base);
                    data["sdp"] = sdp;
                }
                "SETUP" => {
                    if let Some((rtp, rtcp)) = p.sockets {
                        for sock in [rtp, rtcp] {
                            let tx = media.clone();
                            let task = tokio::spawn(async move {
                                let mut buf = vec![0u8; 2048];
                                while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                                    if tx.send((buf[..n].to_vec(), from)).await.is_err() {
                                        return;
                                    }
                                }
                            });
                            self.receivers.push(task.abort_handle());
                            ctx.state.register_client_task(ctx.client_id, task).await;
                        }
                    }
                }
                "TEARDOWN" => {
                    for h in self.receivers.drain(..) {
                        h.abort();
                    }
                    self.session = None;
                }
                _ => {}
            }
        }
        reply(
            p.caller,
            ClientSendOutcome::Executed {
                detail: data.to_string(),
            },
        );
        self.events
            .try_send((Event::new(&actions::RESPONSE_EVENT, data), p.depth))
            .context("RTSP event queue full; consumer stalled")
    }

    async fn run(
        &mut self,
        ctx: &ConnectContext,
        mut responses: mpsc::Receiver<Result<Response>>,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<(Value, usize)>,
    ) -> Result<()> {
        let log = Log::new(Some(&ctx.status_tx));
        let (media_tx, mut media_rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(1024);
        let mut tracker = Tracker::default();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            let (action, depth, mut injected) = tokio::select! {
                r = responses.recv() => {
                    let Some(r) = r else { return Ok(()) };
                    self.on_response(ctx, r?, &media_tx).await?;
                    continue;
                }
                m = media_rx.recv() => {
                    if let Some((data, from)) = m {
                        if !crate::server::rtp::media::is_rtcp(&data) {
                            if let Some(started) = tracker.on_rtp(&data, from) {
                                let _ = self.events.try_send((Event::new(&actions::STREAM_STARTED_EVENT, started), 0));
                            }
                        }
                    }
                    continue;
                }
                _ = tick.tick() => {
                    for data in tracker.sweep() {
                        let _ = self.events.try_send((Event::new(&actions::STREAM_ENDED_EVENT, data), 0));
                    }
                    continue;
                }
                command = external.recv() => match command {
                    Some(c) => (c.action.clone(), 0, Some(c)),
                    None => return Ok(()),
                },
                action = internal.recv() => match action {
                    Some((a, depth)) => (a, depth, None),
                    None => return Ok(()),
                },
            };
            match RtspClientProtocol.execute_action(action.clone()) {
                Ok(ClientActionResult::Disconnect) => {
                    reply(injected.take(), ClientSendOutcome::Disconnected);
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    log.warn(format!("RTSP client action refused: {e}"));
                    reply(
                        injected.take(),
                        ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                    );
                    continue;
                }
            }
            if depth > MAX_FOLLOWUP_DEPTH {
                log.warn(format!(
                    "RTSP client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
                ));
                continue;
            }
            if injected.is_some() {
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "RTSP",
                        None,
                        "injected_action",
                        action.clone(),
                        vec![],
                    )
                    .await;
            }
            self.act(action, depth, injected.take()).await?;
        }
    }
}
