//! One established WebTransport session, shared by the server and the client: incoming streams
//! are read to their end and datagrams taken as they come, each raises one event, and the
//! handler's answer is carried out here. Handler turns are serialised per session.
use super::actions::{validate, DATAGRAM_EVENT, STREAM_EVENT, STREAM_REPLY_EVENT};
use crate::client::command_support;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use anyhow::{bail, ensure, Context, Result};
use futures::future::BoxFuture;
use futures::{stream::FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use wtransport::{Connection, RecvStream, SendStream, VarInt};

/// One stream's whole content, either direction.
pub const MAX_STREAM_BYTES: usize = 1024 * 1024;
/// From a stream's first byte to its end.
pub const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// Incoming streams, datagrams and injected actions being handled at once per session.
pub const MAX_IN_FLIGHT: usize = 32;
/// webtransport_open_bi → webtransport_stream_reply → webtransport_open_bi … stops here.
pub const MAX_FOLLOWUP_DEPTH: usize = 4;
/// Stream error codes NetGet resets with.
pub const CODE_REFUSED: u32 = 0x1;
pub const CODE_FAILED: u32 = 0x2;

pub type AskFn = Arc<dyn Fn(Event) -> BoxFuture<'static, Result<Vec<Value>>> + Send + Sync>;
pub type TrafficFn = Arc<dyn Fn(u64, u64) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct Session {
    pub conn: Connection,
    ask: AskFn,
    traffic: TrafficFn,
    status_tx: mpsc::UnboundedSender<String>,
    label: &'static str,
    turn: tokio::sync::Mutex<()>,
}

/// The bytes an action carries in `data`, decoded per `encoding`.
pub fn payload(v: &Value) -> Result<Vec<u8>> {
    let data = v["data"].as_str().context("data is text")?;
    let bytes = match v["encoding"].as_str().unwrap_or("utf8") {
        "utf8" => data.as_bytes().to_vec(),
        "hex" => hex::decode(data).context("data is not hex")?,
        other => bail!("encoding {other:?} is utf8 or hex"),
    };
    ensure!(
        bytes.len() <= MAX_STREAM_BYTES,
        "data exceeds {MAX_STREAM_BYTES} bytes"
    );
    Ok(bytes)
}

/// Bytes that arrived, as `(data, encoding)`: the text when it is UTF-8, hex otherwise.
pub fn describe(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), "utf8"),
        Err(_) => (hex::encode(bytes), "hex"),
    }
}

/// Read a stream to its end within the bounds; a stream past them is stopped.
pub async fn read_all(mut recv: RecvStream) -> Result<Vec<u8>> {
    let read = async {
        let mut data = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        while let Some(n) = recv.read(&mut buf).await? {
            if n > MAX_STREAM_BYTES - data.len() {
                return Ok(None);
            }
            data.extend_from_slice(&buf[..n]);
        }
        Ok::<_, anyhow::Error>(Some(data))
    };
    match tokio::time::timeout(STREAM_TIMEOUT, read).await {
        Ok(Ok(Some(data))) => Ok(data),
        Ok(Ok(None)) => {
            recv.stop(VarInt::from_u32(CODE_REFUSED));
            bail!("the stream exceeds {MAX_STREAM_BYTES} bytes")
        }
        Ok(Err(e)) => Err(e),
        Err(_) => {
            recv.stop(VarInt::from_u32(CODE_REFUSED));
            bail!("the stream did not finish within {STREAM_TIMEOUT:?}")
        }
    }
}

async fn write_and_finish(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    tokio::time::timeout(STREAM_TIMEOUT, async {
        send.write_all(bytes).await?;
        send.finish().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("the peer did not take the data in time")?
}

impl Session {
    pub fn new(
        conn: Connection,
        ask: AskFn,
        traffic: TrafficFn,
        status_tx: mpsc::UnboundedSender<String>,
        label: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            conn,
            ask,
            traffic,
            status_tx,
            label,
            turn: tokio::sync::Mutex::new(()),
        })
    }

    fn log(&self) -> Log<'_> {
        Log::new(Some(&self.status_tx))
    }

    /// One handler turn; turns on a session never overlap.
    pub async fn ask(&self, event: Event) -> Result<Vec<Value>> {
        let _turn = self.turn.lock().await;
        (self.ask)(event).await
    }

    /// Serve the session until it closes; returns why it closed.
    pub async fn run(
        self: Arc<Self>,
        initial: Vec<Value>,
        first: Option<Event>,
        mut commands: mpsc::Receiver<ClientCommand>,
    ) -> String {
        let mut work: FuturesUnordered<BoxFuture<'static, ()>> = FuturesUnordered::new();
        if !initial.is_empty() {
            work.push(self.clone().execute(initial, None, 0));
        }
        if let Some(event) = first {
            let s = self.clone();
            work.push(Box::pin(async move {
                match s.ask(event).await {
                    Ok(actions) => s.clone().execute(actions, None, 0).await,
                    Err(e) => s.log().error(format!(
                        "{} decision=fail_closed_llm_error; nothing sent: {e}",
                        s.label
                    )),
                }
            }));
        }
        let mut commands_open = true;
        loop {
            let room = work.len() < MAX_IN_FLIGHT;
            tokio::select! {
                r = self.conn.accept_bi(), if room => match r {
                    Ok((send, recv)) => work.push(Box::pin(self.clone().incoming(Some(send), recv))),
                    Err(e) => return e.to_string(),
                },
                r = self.conn.accept_uni(), if room => match r {
                    Ok(recv) => work.push(Box::pin(self.clone().incoming(None, recv))),
                    Err(e) => return e.to_string(),
                },
                r = self.conn.receive_datagram(), if room => match r {
                    Ok(d) => work.push(Box::pin(self.clone().datagram(d.payload().to_vec()))),
                    Err(e) => return e.to_string(),
                },
                c = commands.recv(), if commands_open && room => match c {
                    Some(c) => {
                        let checked = match c.action["type"].as_str() {
                            Some("webtransport_send_datagram" | "webtransport_open_uni" | "webtransport_open_bi" | "webtransport_close") => validate(&c.action),
                            _ => Err(anyhow::anyhow!("a live session takes webtransport_send_datagram, webtransport_open_uni, webtransport_open_bi or webtransport_close")),
                        };
                        match checked {
                            Ok(()) => {
                                let bytes_sent = payload(&c.action).map(|p| p.len()).unwrap_or(0);
                                work.push(self.clone().execute(vec![c.action.clone()], None, 0));
                                command_support::reply(c, Ok(ClientSendOutcome::Sent { bytes_sent }));
                            }
                            Err(e) => command_support::reply(c, Ok(ClientSendOutcome::Rejected { error: e.to_string() })),
                        }
                    }
                    None => commands_open = false,
                },
                _ = work.next(), if !work.is_empty() => {}
                e = self.conn.closed() => return e.to_string(),
            }
        }
    }

    async fn incoming(self: Arc<Self>, send: Option<SendStream>, recv: RecvStream) {
        let stream_id = recv.id().into_u64();
        let data = match read_all(recv).await {
            Ok(data) => data,
            Err(e) => {
                self.log().warn(format!(
                    "{} decision=protocol_refusal stream={stream_id}: {e}",
                    self.label
                ));
                if let Some(mut send) = send {
                    let _ = send.reset(VarInt::from_u32(CODE_REFUSED));
                }
                return;
            }
        };
        (self.traffic)(data.len() as u64, 0).await;
        let (text, encoding) = describe(&data);
        let direction = if send.is_some() {
            "bidirectional"
        } else {
            "unidirectional"
        };
        self.log().debug(format!(
            "{} {direction} stream {stream_id}: {} bytes",
            self.label,
            data.len()
        ));
        let event = Event::new(
            &STREAM_EVENT,
            json!({"stream_id": stream_id, "direction": direction, "data": text, "encoding": encoding}),
        );
        match self.ask(event).await {
            Ok(actions) => self.clone().execute(actions, send, 0).await,
            Err(e) => {
                self.log().error(format!(
                    "{} decision=fail_closed_llm_error stream={stream_id}; resetting it: {e}",
                    self.label
                ));
                if let Some(mut send) = send {
                    let _ = send.reset(VarInt::from_u32(CODE_FAILED));
                }
            }
        }
    }

    async fn datagram(self: Arc<Self>, data: Vec<u8>) {
        (self.traffic)(data.len() as u64, 0).await;
        let (text, encoding) = describe(&data);
        let event = Event::new(&DATAGRAM_EVENT, json!({"data": text, "encoding": encoding}));
        match self.ask(event).await {
            Ok(actions) => self.clone().execute(actions, None, 0).await,
            Err(e) => self.log().error(format!(
                "{} decision=fail_closed_llm_error datagram; nothing sent: {e}",
                self.label
            )),
        }
    }

    /// Carry out the handler's actions in order. `reply` is the stream a bidirectional
    /// event arrived on; it is finished empty when nothing answers on it.
    pub fn execute(
        self: Arc<Self>,
        actions: Vec<Value>,
        mut reply: Option<SendStream>,
        depth: usize,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for a in actions {
                let kind = a["type"].as_str().unwrap_or_default().to_owned();
                if !kind.starts_with("webtransport_") {
                    continue;
                }
                if let Err(e) = validate(&a) {
                    self.log().warn(format!(
                        "{} decision=fail_closed_invalid_reply {kind}: {e}",
                        self.label
                    ));
                    continue;
                }
                if let Err(e) = self.clone().act(&kind, &a, &mut reply, depth).await {
                    self.log()
                        .warn(format!("{} {kind} failed: {e:#}", self.label));
                }
                if kind == "webtransport_close" {
                    return;
                }
            }
            if let Some(mut send) = reply {
                self.log().info(format!(
                    "{} decision=model_silent; finishing the stream empty",
                    self.label
                ));
                let _ = send.finish().await;
            }
        })
    }

    async fn act(
        self: Arc<Self>,
        kind: &str,
        a: &Value,
        reply: &mut Option<SendStream>,
        depth: usize,
    ) -> Result<()> {
        match kind {
            "webtransport_reply" => {
                let mut send = reply
                    .take()
                    .context("webtransport_reply answers a bidirectional stream, and there is none to answer")?;
                let bytes = payload(a)?;
                self.log().info(format!(
                    "{} decision=model_answer; replying {} bytes",
                    self.label,
                    bytes.len()
                ));
                write_and_finish(&mut send, &bytes).await?;
                (self.traffic)(0, bytes.len() as u64).await;
            }
            "webtransport_send_datagram" => {
                let bytes = payload(a)?;
                self.conn.send_datagram(&bytes)?;
                (self.traffic)(0, bytes.len() as u64).await;
            }
            "webtransport_open_uni" => {
                let bytes = payload(a)?;
                let mut send = self.conn.open_uni().await?.await?;
                write_and_finish(&mut send, &bytes).await?;
                (self.traffic)(0, bytes.len() as u64).await;
            }
            "webtransport_open_bi" => {
                let bytes = payload(a)?;
                let (mut send, recv) = self.conn.open_bi().await?.await?;
                let stream_id = send.id().into_u64();
                write_and_finish(&mut send, &bytes).await?;
                (self.traffic)(0, bytes.len() as u64).await;
                let answer = read_all(recv).await?;
                (self.traffic)(answer.len() as u64, 0).await;
                if depth + 1 >= MAX_FOLLOWUP_DEPTH {
                    self.log().warn(format!(
                        "{} follow-up depth {MAX_FOLLOWUP_DEPTH} reached; the reply on stream {stream_id} is not raised",
                        self.label
                    ));
                    return Ok(());
                }
                let (text, encoding) = describe(&answer);
                let event = Event::new(
                    &STREAM_REPLY_EVENT,
                    json!({"stream_id": stream_id, "request": a["data"], "data": text, "encoding": encoding}),
                );
                let actions = self.ask(event).await?;
                self.clone().execute(actions, None, depth + 1).await;
            }
            "webtransport_close" => {
                let code = a["code"].as_u64().unwrap_or(0) as u32;
                let reason = a["reason"].as_str().unwrap_or_default();
                self.log().info(format!(
                    "{} closing the session: code={code} reason={reason:?}",
                    self.label
                ));
                self.conn.close(VarInt::from_u32(code), reason.as_bytes());
            }
            // Admission answers are taken before the session exists.
            "webtransport_accept" | "webtransport_reject" => {}
            other => bail!("unknown action {other}"),
        }
        Ok(())
    }
}
