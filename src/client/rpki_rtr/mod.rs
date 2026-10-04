//! RPKI-to-Router client in the router role. Rust sends the Reset Query on connect, polls with
//! Serial Queries at the refresh interval and on Serial Notify, and answers Cache Reset with a
//! new Reset Query; the handler hears about each completed exchange.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::rpki_rtr::codec::{self, Intervals, Packet, Pdu, Record};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::RpkiRtrClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EXCHANGE_PDUS: u64 = 1_000_000;
const SAMPLE: usize = 256;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let u64_param = |k: &str| {
        p.map(|p| p.get_optional_u64(k))
            .transpose()
            .map(Option::flatten)
    };
    let version = match u64_param("version")? {
        None | Some(1) => 1u8,
        Some(0) => 0,
        Some(v) => bail!("version must be 0 or 1, not {v}"),
    };
    let refresh = codec::seconds(
        u64_param("refresh_interval_secs")?,
        u64::from(codec::DEFAULT_REFRESH_SECONDS),
        86400,
        "refresh_interval_secs",
    )?;
    let exchange_timeout = codec::seconds(
        u64_param("exchange_timeout_secs")?,
        EXCHANGE_TIMEOUT.as_secs(),
        600,
        "exchange_timeout_secs",
    )?;
    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(&ctx.remote_addr),
    )
    .await
    .context("RPKI-RTR connect deadline")??;
    let local = stream.local_addr()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = RpkiRtrClientProtocol;
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
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("RPKI-RTR client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let (reader, writer) = tokio::io::split(stream);
    // A dedicated reader keeps PDU reads cancel-safe against timers and commands.
    let (frame_tx, frame_rx) = mpsc::channel::<Result<Vec<u8>>>(64);
    let reader_task = tokio::spawn(read_frames(reader, frame_tx, exchange_timeout));
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let mut router = Router::new(version, refresh, exchange_timeout, writer);
        let result = router
            .run(&session_ctx, external, internal_rx, frame_rx, event_tx)
            .await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("RPKI-RTR client ended: {e}"));
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

/// Header with no deadline (a cache may be silent until the next poll); body bounded.
async fn read_frames(
    mut reader: tokio::io::ReadHalf<tokio::net::TcpStream>,
    tx: mpsc::Sender<Result<Vec<u8>>>,
    body_deadline: Duration,
) {
    loop {
        let mut header = [0u8; 8];
        let frame = async {
            reader.read_exact(&mut header).await?;
            let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
            ensure!(
                (8..=codec::MAX_PDU_BYTES).contains(&len),
                "RPKI-RTR inbound PDU bound"
            );
            let mut bytes = vec![0u8; len];
            bytes[..8].copy_from_slice(&header);
            tokio::time::timeout(body_deadline, reader.read_exact(&mut bytes[8..]))
                .await
                .context("RPKI-RTR PDU body deadline")??;
            Ok(bytes)
        }
        .await;
        let end = frame.is_err();
        if tx.send(frame).await.is_err() || end {
            return;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Reset,
    Incremental,
}

struct Exchange {
    kind: Kind,
    started: tokio::time::Instant,
    session: Option<u16>,
    announced: u64,
    withdrawn: u64,
    pdus: u64,
    router_keys: u64,
    sample: Vec<Value>,
}

struct Router {
    version: u8,
    refresh: Duration,
    exchange_timeout: Duration,
    writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    session: Option<u16>,
    serial: Option<u32>,
    intervals: Option<Intervals>,
    exchange: Option<Exchange>,
    next_poll: tokio::time::Instant,
}

impl Router {
    fn new(
        version: u8,
        refresh: Duration,
        exchange_timeout: Duration,
        writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    ) -> Self {
        Self {
            version,
            refresh,
            exchange_timeout,
            writer,
            session: None,
            serial: None,
            intervals: None,
            exchange: None,
            next_poll: tokio::time::Instant::now() + refresh,
        }
    }

    async fn write(&mut self, pdu: Pdu) -> Result<usize> {
        codec::write_packet(
            &mut self.writer,
            &Packet {
                version: self.version,
                pdu,
            },
        )
        .await
    }

    async fn reset_query(&mut self) -> Result<usize> {
        let n = self.write(Pdu::ResetQuery).await?;
        self.begin(Kind::Reset);
        Ok(n)
    }

    async fn serial_query(&mut self) -> Result<usize> {
        let (Some(session), Some(serial)) = (self.session, self.serial) else {
            bail!("no synchronized session yet; send a Reset Query first");
        };
        let n = self.write(Pdu::SerialQuery { session, serial }).await?;
        self.begin(Kind::Incremental);
        Ok(n)
    }

    fn begin(&mut self, kind: Kind) {
        self.exchange = Some(Exchange {
            kind,
            started: tokio::time::Instant::now(),
            session: None,
            announced: 0,
            withdrawn: 0,
            pdus: 0,
            router_keys: 0,
            sample: Vec::new(),
        });
    }

    async fn fail(&mut self, code: u16, offending: &[u8], why: &str) -> anyhow::Error {
        let _ = codec::write_packet(
            &mut self.writer,
            &codec::error_report(self.version, code, offending, why),
        )
        .await;
        anyhow::anyhow!("RPKI-RTR {}: {why}", codec::error_name(code))
    }

    async fn run(
        &mut self,
        ctx: &ConnectContext,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<Value>,
        mut frames: mpsc::Receiver<Result<Vec<u8>>>,
        events: mpsc::Sender<Event>,
    ) -> Result<()> {
        self.reset_query().await?;
        loop {
            let deadline = match &self.exchange {
                Some(e) => e.started + self.exchange_timeout,
                None => self.next_poll,
            };
            tokio::select! {
                frame = frames.recv() => {
                    let Some(frame) = frame else { return Ok(()) };
                    let bytes = match frame {
                        Ok(b) => b,
                        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof) => return Ok(()),
                        Err(e) => return Err(e),
                    };
                    if self.on_pdu(&bytes, &events).await? {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if self.exchange.is_some() {
                        bail!("RPKI-RTR exchange did not reach End of Data within {}s", self.exchange_timeout.as_secs());
                    }
                    if self.serial.is_some() {
                        self.serial_query().await?;
                    } else {
                        self.reset_query().await?;
                    }
                }
                command = external.recv() => {
                    let Some(command) = command else { return Ok(()) };
                    let action = command.action.clone();
                    let (reply, end) = self.on_action(action.clone()).await;
                    ctx.state.record_access_log(AccessLogOwner::Client(ctx.client_id.as_u32()), "RPKI-RTR", None, "injected_action", action, vec![json!({"ok": reply.is_ok()})]).await;
                    crate::client::command_support::reply(command, reply);
                    if end { return Ok(()); }
                }
                action = internal.recv() => {
                    let Some(action) = action else { return Ok(()) };
                    let (reply, end) = self.on_action(action).await;
                    if let Err(e) = reply {
                        Log::new(Some(&ctx.status_tx)).warn(format!("RPKI-RTR client action refused: {e}"));
                    }
                    if end { return Ok(()); }
                }
            }
        }
    }

    async fn on_action(&mut self, action: Value) -> (Result<ClientSendOutcome>, bool) {
        let result = match RpkiRtrClientProtocol.execute_action(action) {
            Ok(r) => r,
            Err(e) => {
                return (
                    Ok(ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    }),
                    false,
                )
            }
        };
        match result {
            ClientActionResult::Disconnect => {
                let _ = self.writer.shutdown().await;
                (Ok(ClientSendOutcome::Disconnected), true)
            }
            ClientActionResult::Custom { name, .. } => {
                if self.exchange.is_some() {
                    return (
                        Ok(ClientSendOutcome::Rejected {
                            error: "an RTR exchange is in progress; retry after End of Data".into(),
                        }),
                        false,
                    );
                }
                let sent = if name == "rpki_rtr_reset_query" {
                    self.reset_query().await
                } else {
                    self.serial_query().await
                };
                match sent {
                    Ok(n) => (Ok(ClientSendOutcome::Sent { bytes_sent: n }), false),
                    Err(e) if self.session.is_none() && name == "rpki_rtr_serial_query" => (
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                        false,
                    ),
                    Err(e) => (Err(e), true),
                }
            }
            _ => (
                Ok(ClientSendOutcome::Rejected {
                    error: "unsupported action".into(),
                }),
                false,
            ),
        }
    }

    /// Handle one PDU; `Ok(true)` ends the session cleanly.
    async fn on_pdu(&mut self, bytes: &[u8], events: &mpsc::Sender<Event>) -> Result<bool> {
        if bytes[0] != self.version {
            // An Error Report saying the cache does not speak our version arrives in its own.
            if bytes[1] == 10 {
                if let Ok(Packet {
                    pdu:
                        Pdu::ErrorReport {
                            code, diagnostic, ..
                        },
                    ..
                }) = Packet::decode(bytes)
                {
                    return self.error_event(code, &diagnostic, events).map(|_| true);
                }
            }
            return Err(self
                .fail(8, bytes, "PDU version differs from the session's")
                .await);
        }
        if bytes[1] == 9 && self.version == 1 {
            // Router Key (BGPsec): outside this client's scope, counted and ignored.
            if let Some(e) = self.exchange.as_mut() {
                e.router_keys += 1;
                e.pdus += 1;
            }
            return Ok(false);
        }
        let packet = match Packet::decode(bytes) {
            Ok(p) => p,
            Err(e) => {
                let code = if matches!(bytes[1], 0..=4 | 6..=8 | 10) {
                    0
                } else {
                    5
                };
                return Err(self.fail(code, bytes, &e.to_string()).await);
            }
        };
        match packet.pdu {
            Pdu::CacheResponse { session } => {
                let Some(exchange) = self.exchange.as_mut() else {
                    return Err(self.fail(0, bytes, "Cache Response without a query").await);
                };
                if exchange.session.is_some() {
                    return Err(self
                        .fail(0, bytes, "second Cache Response in one exchange")
                        .await);
                }
                if exchange.kind == Kind::Incremental && Some(session) != self.session {
                    return Err(self
                        .fail(0, bytes, "Cache Response changed the session id")
                        .await);
                }
                exchange.session = Some(session);
            }
            Pdu::Prefix(record) => {
                let Some(exchange) = self.exchange.as_mut().filter(|e| e.session.is_some()) else {
                    return Err(self.fail(0, bytes, "Prefix PDU outside an exchange").await);
                };
                if exchange.kind == Kind::Reset && !record.announcement {
                    return Err(self.fail(6, bytes, "withdrawal in a reset exchange").await);
                }
                exchange.pdus += 1;
                if exchange.pdus > MAX_EXCHANGE_PDUS {
                    return Err(self.fail(1, bytes, "exchange exceeds 1,000,000 PDUs").await);
                }
                if record.announcement {
                    exchange.announced += 1;
                } else {
                    exchange.withdrawn += 1;
                }
                if exchange.sample.len() < SAMPLE {
                    exchange.sample.push(record_json(&record));
                }
            }
            Pdu::EndOfData {
                session,
                serial,
                intervals,
            } => {
                let Some(exchange) = self.exchange.take().filter(|e| e.session == Some(session))
                else {
                    return Err(self
                        .fail(
                            0,
                            bytes,
                            "End of Data does not close an exchange of this session",
                        )
                        .await);
                };
                if exchange.kind == Kind::Incremental {
                    let old = self.serial.unwrap_or(serial);
                    if serial != old && !codec::serial_newer(serial, old) {
                        return Err(self
                            .fail(0, bytes, "End of Data serial went backwards")
                            .await);
                    }
                }
                self.session = Some(session);
                self.serial = Some(serial);
                if self.version == 1 {
                    self.intervals = Some(intervals);
                }
                let refresh = self
                    .intervals
                    .map(|i| Duration::from_secs(u64::from(i.refresh)))
                    .unwrap_or(self.refresh);
                self.next_poll = tokio::time::Instant::now() + refresh;
                let shown = self.intervals.unwrap_or(Intervals {
                    refresh: refresh.as_secs() as u32,
                    ..Intervals::default()
                });
                let truncated =
                    exchange.announced + exchange.withdrawn > exchange.sample.len() as u64;
                events
                    .try_send(Event::new(
                        &actions::SYNCHRONIZED_EVENT,
                        json!({
                            "kind": if exchange.kind == Kind::Reset { "reset" } else { "incremental" },
                            "session_id": session,
                            "serial": serial,
                            "announced": exchange.announced,
                            "withdrawn": exchange.withdrawn,
                            "records": exchange.sample,
                            "truncated": truncated,
                            "router_keys_ignored": exchange.router_keys,
                            "intervals": {"refresh": shown.refresh, "retry": shown.retry, "expire": shown.expire},
                        }),
                    ))
                    .context("RPKI-RTR event queue full; consumer stalled")?;
            }
            Pdu::CacheReset => {
                if !matches!(
                    self.exchange.as_ref().map(|e| (e.kind, e.session)),
                    Some((Kind::Incremental, None))
                ) {
                    return Err(self
                        .fail(0, bytes, "Cache Reset outside a pending Serial Query")
                        .await);
                }
                let previous = self.serial;
                self.session = None;
                self.serial = None;
                self.reset_query().await?;
                events
                    .try_send(Event::new(
                        &actions::CACHE_RESET_EVENT,
                        json!({"previous_serial": previous}),
                    ))
                    .context("RPKI-RTR event queue full")?;
            }
            Pdu::SerialNotify { session, serial } => {
                // Poll now unless an exchange is running or the notice is about another session.
                if self.exchange.is_none()
                    && Some(session) == self.session
                    && Some(serial) != self.serial
                {
                    self.serial_query().await?;
                }
            }
            Pdu::ErrorReport {
                code, diagnostic, ..
            } => {
                let end = self.error_event(code, &diagnostic, events)?;
                if code == 2 {
                    self.exchange = None;
                    let retry = self
                        .intervals
                        .map(|i| u64::from(i.retry))
                        .unwrap_or(u64::from(codec::DEFAULT_RETRY_SECONDS));
                    self.next_poll = tokio::time::Instant::now() + Duration::from_secs(retry);
                }
                return Ok(end);
            }
            Pdu::ResetQuery | Pdu::SerialQuery { .. } => {
                return Err(self.fail(3, bytes, "a cache does not send queries").await);
            }
        }
        Ok(false)
    }

    fn error_event(
        &mut self,
        code: u16,
        diagnostic: &str,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool> {
        events
            .try_send(Event::new(
                &actions::ERROR_EVENT,
                json!({"code": code, "name": codec::error_name(code), "diagnostic": crate::utils::sanitize::strip_controls(diagnostic)}),
            ))
            .context("RPKI-RTR event queue full")?;
        Ok(code != 2)
    }
}

fn record_json(r: &Record) -> Value {
    json!({"prefix": r.prefix, "max_length": r.max_length, "asn": r.asn, "announcement": r.announcement})
}
