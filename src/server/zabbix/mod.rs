//! Zabbix trapper — the protocol `zabbix_sender` speaks to a Zabbix server on port 10051.
//!
//! One request per connection, as the Zabbix server does it: the sender connects, writes one
//! `ZBXD` packet carrying `{"request":"sender data","data":[…]}`, reads one response and the
//! server closes. The model decides how many of the reported values it accepts; NetGet renders
//! the response.
//!
//! Four properties worth knowing before changing the loop:
//!
//! 1. **The declared length is judged before anything is allocated for it.** The header is read
//!    whole (13 or 21 bytes) and [`wire::parse_header`] refuses a length past
//!    [`wire::MAX_DATA_BYTES`] (1 MiB) before a single body byte is read. Compressed packets are
//!    refused outright (see `wire.rs`).
//! 2. **The model writes two numbers.** `send_zabbix_result` is rendered by
//!    [`wire::render_result`]; the loop reads the counts back and requires
//!    `processed + failed` to equal the request's own value count before writing, with the real
//!    `seconds spent` filled in.
//! 3. **Failure is told as failure in the one form `zabbix_sender` acts on.** A backend
//!    failure, silence, or counts that do not add up are answered `processed: 0; failed: N;
//!    total: N` — `zabbix_sender` exits 2. A `{"response":"failed"}` would be the obvious
//!    choice and is the wrong one: zabbix_sender 7.4 prints a warning and exits **0** on it
//!    (measured), so a script checking `$?` would believe its values were stored.
//! 4. **The deadlines wrap the reads, not the answer.** A request parked for a human under a
//!    `manual` rule is not closed by either.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

pub use wire::MAX_DATA_BYTES;

/// How long a new connection may send nothing. `zabbix_sender` writes its request the moment
/// it connects (the Zabbix server's own `Timeout` defaults to 3 s); 30 s leaves room for a slow
/// network and for a hand-driven NetGet TCP client, which can raise it further through
/// `first_byte_timeout_secs`.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server waits for more of a request that has started arriving.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent connections admitted before new ones are refused — the house default.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// Fixed `info` texts for the requests NetGet refuses itself.
const INFO_TOO_LARGE: &str = "message is too large";
const INFO_COMPRESSED: &str = "compressed data is not supported";
const INFO_BAD_FLAGS: &str = "unsupported protocol flags";
const INFO_UNSUPPORTED: &str = "unsupported request";

pub struct ZabbixServer;

impl ZabbixServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        first_byte_timeout_secs: Option<u64>,
        idle_timeout_secs: Option<u64>,
    ) -> Result<SocketAddr> {
        let first_timeout = first_byte_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(FIRST_BYTE_TIMEOUT);
        let idle_timeout = idle_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(IDLE_TIMEOUT);
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Zabbix trapper listening on {}", local_addr));

        let protocol = Arc::new(actions::ZabbixProtocol::new());
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                // A peer over the cap gets no bytes: the response to a request it has not
                // sent yet would be read as the answer to it. zabbix_sender reports the
                // closed connection as a failed send, which is the truth.
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    b"",
                    "Zabbix",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((socket, peer_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = crate::utils::clock::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ServerConnectionState {
                                    id: connection_id,
                                    remote_addr: peer_addr,
                                    local_addr,
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
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        Log::new(Some(&status_tx))
                            .info(format!("Zabbix sender connected from {}", peer_addr));

                        let session = Session {
                            peer_addr,
                            llm_client: llm_client.clone(),
                            app_state: app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id,
                            protocol: protocol.clone(),
                            connection_id,
                            first_timeout,
                            idle_timeout,
                        };
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                let _permit = permit;
                                session.run(socket).await
                            })
                            .await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Zabbix accept error: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;
        Ok(local_addr)
    }
}

struct Session {
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::ZabbixProtocol>,
    connection_id: ConnectionId,
    first_timeout: Duration,
    idle_timeout: Duration,
}

type Writer = Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>;

impl Session {
    async fn run(self, socket: tokio::net::TcpStream) {
        let (mut reader, write_half) = tokio::io::split(socket);
        let writer: Writer = Arc::new(Mutex::new(write_half));

        // Registered first, so the operator can reach the connection while its request is
        // parked for them.
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &self.app_state,
            self.server_id,
            self.connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            self.protocol.clone(),
            self.app_state.clone(),
            self.server_id,
            self.connection_id.as_u32(),
            writer.clone(),
            self.status_tx.clone(),
        );

        self.session(&mut reader, &writer).await;

        self.app_state
            .remove_peer_handle(self.server_id, self.connection_id.as_u32())
            .await;
        let _ = writer.lock().await.shutdown().await;
        linger(&mut reader).await;
        self.app_state
            .update_connection_status(
                self.server_id,
                self.connection_id,
                crate::state::server::ConnectionStatus::Closed,
            )
            .await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());
    }

    async fn write(&self, writer: &Writer, data: &[u8]) {
        let ok = {
            let mut w = writer.lock().await;
            w.write_all(data).await.is_ok() && w.flush().await.is_ok()
        };
        if ok {
            self.app_state
                .update_connection_stats(
                    self.server_id,
                    self.connection_id,
                    None,
                    Some(data.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
    }

    /// Read until `buf` holds `want` bytes. `first` bounds the wait for the very first byte,
    /// `self.idle_timeout` every wait after it.
    async fn fill<R: AsyncRead + Unpin>(
        &self,
        reader: &mut R,
        buf: &mut Vec<u8>,
        want: usize,
        first: Option<Duration>,
    ) -> Result<(), &'static str> {
        let mut chunk = [0u8; 8192];
        while buf.len() < want {
            let timeout = match first {
                Some(t) if buf.is_empty() => t,
                _ => self.idle_timeout,
            };
            let room = (want - buf.len()).min(chunk.len());
            match tokio::time::timeout(timeout, reader.read(&mut chunk[..room])).await {
                Err(_) => return Err("timed out"),
                Ok(Ok(0)) => return Err("closed"),
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                Ok(Err(_)) => return Err("read error"),
            }
        }
        Ok(())
    }

    async fn session<R: AsyncRead + Unpin>(&self, reader: &mut R, writer: &Writer) {
        let log = Log::new(Some(&self.status_tx));
        let started = crate::utils::clock::Instant::now();
        let mut buf: Vec<u8> = Vec::with_capacity(wire::LARGE_HEADER_LEN);

        // The magic and the flags byte, which says how long the rest of the header is.
        if let Err(why) = self
            .fill(reader, &mut buf, 5, Some(self.first_timeout))
            .await
        {
            log.info(format!(
                "Zabbix peer {} sent no header ({why}); closing",
                self.peer_addr
            ));
            return;
        }
        if &buf[..4] != wire::MAGIC {
            // Not the Zabbix protocol at all: there is no response it would understand.
            log.warn(format!(
                "Zabbix peer {} did not send a ZBXD header decision=fail_closed_bad_header",
                self.peer_addr
            ));
            return;
        }
        let header_len = wire::header_len(buf[4]);
        if let Err(why) = self.fill(reader, &mut buf, header_len, None).await {
            log.info(format!(
                "Zabbix peer {}: header incomplete ({why}); closing",
                self.peer_addr
            ));
            return;
        }
        let header = match wire::parse_header(&buf[..header_len]) {
            Ok(header) => header,
            Err(e) => {
                let (decision, info) = match e {
                    wire::HeaderError::TooLarge(_) => ("fail_closed_too_large", INFO_TOO_LARGE),
                    wire::HeaderError::Compressed => ("fail_closed_compressed", INFO_COMPRESSED),
                    wire::HeaderError::BadFlags(_) | wire::HeaderError::BadMagic => {
                        ("fail_closed_bad_header", INFO_BAD_FLAGS)
                    }
                };
                log.warn(format!(
                    "Zabbix peer {}: {:?} decision={} (limit {} bytes)",
                    self.peer_addr,
                    e,
                    decision,
                    wire::MAX_DATA_BYTES
                ));
                self.write(writer, &wire::render_failed(info)).await;
                return;
            }
        };

        // Bounded by parse_header: at most MAX_DATA_BYTES.
        let total_len = header_len + header.data_len as usize;
        buf.reserve_exact(total_len - buf.len());
        if let Err(why) = self.fill(reader, &mut buf, total_len, None).await {
            log.info(format!(
                "Zabbix peer {}: request body incomplete ({why}); closing",
                self.peer_addr
            ));
            return;
        }
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                Some(buf.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let body = &buf[header_len..total_len];
        log.trace(format!(
            "Zabbix request from {}: {}",
            self.peer_addr,
            String::from_utf8_lossy(body)
        ));

        let (items, clock) = match wire::parse_request(body) {
            Ok(wire::Request::SenderData { items, clock }) => (items, clock),
            Ok(wire::Request::Other(name)) => {
                log.warn(format!(
                    "Zabbix peer {} sent request {:?} decision=fail_closed_unsupported_request",
                    self.peer_addr, name
                ));
                self.write(writer, &wire::render_failed(INFO_UNSUPPORTED))
                    .await;
                return;
            }
            Err(e) => {
                log.warn(format!(
                    "Zabbix peer {}: {:?} decision=fail_closed_bad_request",
                    self.peer_addr, e
                ));
                self.write(writer, &wire::render_failed(e.info())).await;
                return;
            }
        };
        let total = items.len() as u64;
        if total == 0 {
            // Nothing to decide: the Zabbix server answers an empty batch the same way.
            self.write(
                writer,
                &wire::render_result(0, 0, 0, started.elapsed().as_secs_f64()),
            )
            .await;
            return;
        }

        let reply = self.answer_with_model(&items, clock).await;
        if let Some((processed, failed)) = reply {
            self.write(
                writer,
                &wire::render_result(processed, failed, total, started.elapsed().as_secs_f64()),
            )
            .await;
        }
    }

    /// Ask the model; `None` means close without answering (the model's own choice).
    async fn answer_with_model(
        &self,
        items: &[wire::SenderItem],
        clock: Option<i64>,
    ) -> Option<(u64, u64)> {
        let log = Log::new(Some(&self.status_tx));
        let total = items.len() as u64;
        let items_json: Vec<serde_json::Value> = items
            .iter()
            .map(|i| {
                let mut v = serde_json::json!({"host": i.host, "key": i.key, "value": i.value});
                if let Some(c) = i.clock {
                    v["clock"] = c.into();
                }
                if let Some(ns) = i.ns {
                    v["ns"] = ns.into();
                }
                v
            })
            .collect();
        let mut data = serde_json::json!({"items": items_json, "item_count": total});
        if let Some(c) = clock {
            data["clock"] = c.into();
        }
        let event = Event::new(&actions::ZABBIX_SENDER_DATA_EVENT, data);

        let result = match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let category = match crate::utils::WireFailure::classify(&e) {
                    crate::utils::WireFailure::Overloaded => "overloaded",
                    crate::utils::WireFailure::Unavailable => "unavailable",
                };
                log.warn(format!(
                    "Zabbix {} value(s) from {} decision=fail_closed_llm_error category={}; \
                     answering processed: 0",
                    total, self.peer_addr, category
                ));
                log.debug(format!("Zabbix LLM call failed: {}", e));
                return Some((0, total));
            }
        };
        for message in &result.messages {
            log.info(message);
        }

        let mut replies: Vec<Vec<u8>> = Vec::new();
        let mut close = false;
        let mut stack = result.protocol_results;
        stack.reverse();
        while let Some(item) = stack.pop() {
            match item {
                ActionResult::Output(bytes) => replies.push(bytes),
                ActionResult::CloseConnection => close = true,
                ActionResult::Multiple(items) => stack.extend(items.into_iter().rev()),
                _ => {}
            }
        }

        let Some(reply) = replies.first() else {
            if close {
                log.info(format!(
                    "Zabbix {} value(s) from {} decision=model_close; closing without an answer",
                    total, self.peer_addr
                ));
                return None;
            }
            log.warn(format!(
                "Zabbix {} value(s) from {} decision=model_silent ({} failed action(s)); \
                 answering processed: 0",
                total,
                self.peer_addr,
                result.failures.len()
            ));
            return Some((0, total));
        };

        match wire::read_result(reply) {
            Some((processed, failed, t)) if t == total => {
                let decision = if processed == 0 {
                    "model_reject"
                } else {
                    "model_answer"
                };
                log.info(format!(
                    "Zabbix {} value(s) from {} decision={} processed={} failed={}",
                    total, self.peer_addr, decision, processed, failed
                ));
                Some((processed, failed))
            }
            other => {
                log.warn(format!(
                    "Zabbix {} value(s) from {} decision=fail_closed_mismatched_reply \
                     counts={:?}; answering processed: 0",
                    total, self.peer_addr, other
                ));
                Some((0, total))
            }
        }
    }
}

/// Keep reading briefly after the response and the half-close, so the close is a FIN rather
/// than an RST that could destroy the response before the sender reads it. Bounded both ways.
const LINGER_TIME: Duration = Duration::from_secs(2);
const LINGER_BYTES: usize = 64 * 1024;

async fn linger<R: AsyncRead + Unpin>(reader: &mut R) {
    let deadline = tokio::time::Instant::now() + LINGER_TIME;
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < LINGER_BYTES {
        match tokio::time::timeout_at(deadline, reader.read(&mut sink)).await {
            Ok(Ok(n)) if n > 0 => drained += n,
            _ => break,
        }
    }
}
