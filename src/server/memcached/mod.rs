//! Memcached server (text protocol) — the model is the cache.
//!
//! **Nothing is stored here.** Grep this file for a `HashMap`, a `BTreeMap` or a file handle
//! and you will not find one. Each command becomes an event, the model answers it, and the
//! answer goes on the wire. Two `get`s of the same key are two independent questions.
//!
//! See `src/server/memcached/CLAUDE.md`.

pub mod actions;
pub mod protocol;

pub use actions::MemcachedProtocol;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use actions::{
    MEMCACHED_ARITHMETIC_EVENT, MEMCACHED_DELETE_EVENT, MEMCACHED_FLUSH_ALL_EVENT,
    MEMCACHED_GET_EVENT, MEMCACHED_STATS_EVENT, MEMCACHED_STORE_EVENT, MEMCACHED_TOUCH_EVENT,
    MEMCACHED_UNKNOWN_COMMAND_EVENT, MEMCACHED_VERSION_EVENT,
};
use protocol::{Command, Parsed};

/// Ceiling on the read buffer for one still-incomplete command.
///
/// A storage command declares its own length, capped at `MAX_VALUE_LEN`; this leaves room
/// for that plus the header and terminator, and refuses anything that would let a stalled
/// client grow the buffer without bound.
const MAX_BUFFERED: usize = protocol::MAX_VALUE_LEN + protocol::MAX_COMMAND_LINE + 16;

pub struct MemcachedServer;

impl MemcachedServer {
    /// Bind and serve. Returns `Err` if the listener cannot be bound, so `server_startup`
    /// records `ServerStatus::Error` instead of a server that is not listening.
    pub async fn spawn_with_llm_actions(ctx: SpawnContext) -> Result<SocketAddr> {
        let listen_addr = ctx.legacy_listen_addr();
        let SpawnContext {
            llm_client,
            state,
            status_tx,
            server_id,
            ..
        } = ctx;

        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("Memcached failed to bind {}", listen_addr))?;
        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Memcached server listening on {}", actual_addr));

        let task_registrar = state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                let (stream, peer_addr) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Memcached accept error: {}", e));
                        continue;
                    }
                };

                let connection_id = ConnectionId::new(state.get_next_unified_id().await);
                let local_addr = stream.local_addr().unwrap_or(actual_addr);
                Self::track_connection(&state, server_id, connection_id, peer_addr, local_addr)
                    .await;

                Log::new(Some(&status_tx)).info(format!(
                    "Memcached connection {} from {}",
                    connection_id, peer_addr
                ));
                let _ = status_tx.send("__UPDATE_UI__".to_string());

                let llm = llm_client.clone();
                let st = state.clone();
                let tx = status_tx.clone();
                let registrar = state.clone();
                let conn_handle = tokio::spawn(async move {
                    if let Err(e) = Self::handle_connection(
                        stream,
                        peer_addr,
                        connection_id,
                        server_id,
                        llm,
                        st.clone(),
                        tx,
                    )
                    .await
                    {
                        debug!("Memcached connection {} ended: {}", connection_id, e);
                    }
                    st.close_connection_on_server(server_id, connection_id)
                        .await;
                });

                // Register the per-connection task too, not just the accept loop: aborting
                // the accept loop releases the port but leaves every in-flight session
                // running, so `stop_server` did not actually stop the server.
                // `register_server_task` prunes finished handles on each call, so this cannot
                // grow without bound.
                registrar.register_server_task(server_id, conn_handle).await;
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(actual_addr)
    }

    async fn track_connection(
        state: &Arc<AppState>,
        server_id: ServerId,
        connection_id: ConnectionId,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
    ) {
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let now = std::time::Instant::now();
        state
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
    }

    /// One command at a time, strictly sequentially.
    ///
    /// This is deliberately *not* the Idle/Processing/Accumulating state machine the
    /// connection-oriented protocols hand-roll. That machine exists to stop two LLM calls
    /// running on one connection; reading and answering in a single task achieves the same
    /// thing by construction, and data arriving mid-call simply waits in the socket buffer.
    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        stream: TcpStream,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: ServerId,
        llm_client: OllamaClient,
        state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // Split, never clone — the project rule for TcpStream. The write half is shared
        // under one mutex between this reader task and the dashboard's peer-injection task,
        // so their writes cannot interleave.
        let (reader, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));
        let handler = Arc::new(actions::MemcachedProtocol::new());

        // Peer messaging: the dashboard's "message this peer" / "disconnect this peer"
        // injects actions into THIS connection through the same executor the LLM path uses.
        // Every memcached wire verb returns `ActionResult::Output` and
        // `close_memcached_connection` returns `CloseConnection`, so the generic peer task
        // covers the whole vocabulary — there is no `Custom` gap.
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &state,
            server_id,
            connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            handler.clone(),
            state.clone(),
            server_id,
            connection_id.as_u32(),
            write_half.clone(),
            status_tx.clone(),
        );

        let result = Self::run_connection(
            reader,
            &write_half,
            &handler,
            peer_addr,
            connection_id,
            server_id,
            &llm_client,
            &state,
            &status_tx,
        )
        .await;

        // Every exit path — EOF, read/write error, client quit, model-requested close,
        // oversize buffer — lands here, so the handle never outlives the connection.
        state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        result
    }

    /// The read/parse/answer/write loop, generic over the split halves so the peer task can
    /// hold a clone of the write half. Setup and peer-handle teardown live in the caller.
    #[allow(clippy::too_many_arguments)]
    async fn run_connection<R, W>(
        mut reader: R,
        write_half: &Arc<tokio::sync::Mutex<W>>,
        handler: &Arc<actions::MemcachedProtocol>,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: ServerId,
        llm_client: &OllamaClient,
        state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let log = Log::new(Some(status_tx));

        let mut buffer: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = vec![0u8; 8192];

        loop {
            // Drain every complete command already buffered before reading more.
            loop {
                match protocol::parse_command(&buffer) {
                    Parsed::Incomplete => break,
                    Parsed::Invalid { message, consumed } => {
                        let dropped = consumed.min(buffer.len());
                        buffer.drain(..dropped);
                        warn!(
                            "Memcached {} sent an unusable command: {}",
                            peer_addr, message
                        );
                        // Bytes the peer sent are bytes received, malformed or not. This arm
                        // counted only what it wrote back, so a peer sending nothing but bad
                        // commands showed `bytes_received == 0` on the dashboard.
                        Self::count_received(state, server_id, connection_id, dropped).await;
                        let line = format!("CLIENT_ERROR {}\r\n", message);
                        {
                            let mut writer = write_half.lock().await;
                            writer.write_all(line.as_bytes()).await?;
                            writer.flush().await?;
                        }
                        Self::count_sent(state, server_id, connection_id, line.len()).await;
                    }
                    Parsed::Fatal { message } => {
                        // A storage command whose `<bytes>` is missing, unparseable or larger
                        // than the cap. There is no boundary to skip to, so continuing would
                        // mean parsing the peer's payload as commands. Reply and close.
                        let received = buffer.len();
                        buffer.clear();
                        warn!(
                            "Memcached {} decision=connection_closed_unframable: {}; closing, \
                             because the data block's extent is unknown and skipping it would \
                             put the peer's payload on the command path",
                            peer_addr, message
                        );
                        Self::count_received(state, server_id, connection_id, received).await;
                        let line = format!("CLIENT_ERROR {}\r\n", message);
                        {
                            let mut writer = write_half.lock().await;
                            writer.write_all(line.as_bytes()).await?;
                            writer.flush().await?;
                            let _ = writer.shutdown().await;
                        }
                        Self::count_sent(state, server_id, connection_id, line.len()).await;
                        return Ok(());
                    }
                    Parsed::Complete { command, consumed } => {
                        buffer.drain(..consumed);
                        Self::count_received(state, server_id, connection_id, consumed).await;

                        if matches!(command, Command::Quit) {
                            debug!("Memcached {} sent quit", peer_addr);
                            return Ok(());
                        }

                        let suppress = protocol::is_noreply(&command);
                        let event = Self::event_for(&command);

                        let outcome = call_llm(
                            llm_client,
                            state,
                            server_id,
                            Some(connection_id),
                            &event,
                            handler.as_ref(),
                        )
                        .await;

                        let (reply, close) = match outcome {
                            Ok(execution) => {
                                for message in &execution.messages {
                                    log.info(message);
                                }
                                let mut bytes = Vec::new();
                                let mut close = false;
                                for result in &execution.protocol_results {
                                    for out in result.get_all_output() {
                                        bytes.extend_from_slice(&out);
                                    }
                                    close |= result.closes_connection();
                                }
                                if bytes.is_empty() && !close {
                                    // Nothing usable came back. Memcached clients block on a
                                    // reply, so silence would hang them until their own
                                    // timeout. SERVER_ERROR is the protocol's way of saying
                                    // "this server failed" — never a fabricated cache hit.
                                    // `decision=` tags mirror `src/server/radius/`: the text
                                    // protocol has one failure reply, so on the wire a
                                    // refusal, an outage and a model that answered with
                                    // nothing usable are indistinguishable. Only the log can
                                    // tell them apart.
                                    warn!(
                                        "Memcached {} decision=fail_closed_no_action command={}: \
                                         no action produced",
                                        peer_addr,
                                        Self::command_name(&command)
                                    );
                                    bytes.extend_from_slice(
                                        b"SERVER_ERROR no response was produced\r\n",
                                    );
                                }
                                (bytes, close)
                            }
                            Err(e) => {
                                // `SERVER_ERROR <text>` is the protocol's own way to say the
                                // server failed on an otherwise valid command, and no client
                                // treats it as data - so it can never be read as a cache hit,
                                // a stored value or a successful delete.
                                //
                                // The reason is stripped of CR/LF: memcached replies are
                                // CRLF-terminated with no length prefix, so a newline inside
                                // the text would end the reply early and the rest would be
                                // parsed as the next one.
                                //
                                // The overload/unavailable split that `redis` maps onto
                                // `-LOADING` vs `-ERR` has nowhere to go here: the text
                                // protocol defines exactly one server-failure reply, and
                                // inventing a second would be a protocol extension no client
                                // understands. It is carried in the log instead, so an
                                // operator can still tell a saturated backend from a broken
                                // one.
                                let overloaded = crate::llm::is_overload_error(&e);
                                let decision = if overloaded {
                                    "fail_closed_llm_overloaded"
                                } else {
                                    "fail_closed_llm_error"
                                };
                                let text = crate::utils::WireFailure::classify(&e).prefixed_text();
                                let reply = format!("SERVER_ERROR {text}\r\n");
                                log.warn(format!(
                                    "Memcached {} decision={} command={} replying {}: {}",
                                    peer_addr,
                                    decision,
                                    Self::command_name(&command),
                                    reply.trim(),
                                    e
                                ));
                                (reply.into_bytes(), false)
                            }
                        };

                        // `noreply` suppresses the reply for storage/delete/arithmetic
                        // commands, per protocol.txt. The LLM call still happened; only the
                        // write is skipped.
                        if !suppress && !reply.is_empty() {
                            log.trace(format!(
                                "Memcached -> {}: {}",
                                peer_addr,
                                String::from_utf8_lossy(&reply)
                            ));
                            {
                                let mut writer = write_half.lock().await;
                                writer.write_all(&reply).await?;
                                writer.flush().await?;
                            }
                            Self::count_sent(state, server_id, connection_id, reply.len()).await;
                        }

                        if close {
                            debug!("Memcached closing {} on model request", peer_addr);
                            return Ok(());
                        }
                    }
                }
            }

            if buffer.len() > MAX_BUFFERED {
                warn!(
                    "Memcached {} buffered {} bytes without a complete command; closing",
                    peer_addr,
                    buffer.len()
                );
                let _ = write_half
                    .lock()
                    .await
                    .write_all(b"SERVER_ERROR command too large\r\n")
                    .await;
                return Ok(());
            }

            let n = reader.read(&mut chunk).await?;
            if n == 0 {
                debug!("Memcached {} closed the connection", peer_addr);
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..n]);
        }
    }

    async fn count_received(
        state: &Arc<AppState>,
        server_id: ServerId,
        connection_id: ConnectionId,
        bytes: usize,
    ) {
        state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(bytes as u64),
                None,
                Some(1),
                None,
            )
            .await;
    }

    async fn count_sent(
        state: &Arc<AppState>,
        server_id: ServerId,
        connection_id: ConnectionId,
        bytes: usize,
    ) {
        state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(bytes as u64),
                None,
                Some(1),
            )
            .await;
    }

    fn command_name(command: &Command) -> &'static str {
        match command {
            Command::Retrieval { command, .. } => command,
            Command::Storage { command, .. } => command,
            Command::Delete { .. } => "delete",
            Command::Arithmetic { command, .. } => command,
            Command::Touch { .. } => "touch",
            Command::Stats { .. } => "stats",
            Command::Version => "version",
            Command::FlushAll { .. } => "flush_all",
            Command::Quit => "quit",
            Command::Unknown { .. } => "unknown",
        }
    }

    /// Turn a parsed command into the event the model sees. Structured fields only; the one
    /// place bytes appear is a storage command's data block, which carries an explicit
    /// `value_encoding` saying whether it is text or hex.
    fn event_for(command: &Command) -> Event {
        match command {
            Command::Retrieval { command, keys } => Event::new(
                &MEMCACHED_GET_EVENT,
                serde_json::json!({ "command": command, "keys": keys }),
            ),
            Command::Storage {
                command,
                key,
                flags,
                exptime,
                bytes,
                cas_unique,
                data,
                ..
            } => {
                let (value, encoding) = match std::str::from_utf8(data) {
                    Ok(s) => (s.to_string(), "utf8"),
                    Err(_) => (hex::encode(data), "hex"),
                };
                Event::new(
                    &MEMCACHED_STORE_EVENT,
                    serde_json::json!({
                        "command": command,
                        "key": key,
                        "flags": flags,
                        "exptime": exptime,
                        "bytes": bytes,
                        "cas_unique": cas_unique,
                        "value": value,
                        "value_encoding": encoding,
                    }),
                )
            }
            Command::Delete { key, .. } => {
                Event::new(&MEMCACHED_DELETE_EVENT, serde_json::json!({ "key": key }))
            }
            Command::Arithmetic {
                command,
                key,
                delta,
                ..
            } => Event::new(
                &MEMCACHED_ARITHMETIC_EVENT,
                serde_json::json!({ "command": command, "key": key, "delta": delta }),
            ),
            Command::Touch { key, exptime, .. } => Event::new(
                &MEMCACHED_TOUCH_EVENT,
                serde_json::json!({ "key": key, "exptime": exptime }),
            ),
            Command::Stats { argument } => Event::new(
                &MEMCACHED_STATS_EVENT,
                serde_json::json!({ "argument": argument }),
            ),
            Command::Version => Event::new(&MEMCACHED_VERSION_EVENT, serde_json::json!({})),
            Command::FlushAll { delay, .. } => Event::new(
                &MEMCACHED_FLUSH_ALL_EVENT,
                serde_json::json!({ "delay": delay }),
            ),
            Command::Unknown { line } => Event::new(
                &MEMCACHED_UNKNOWN_COMMAND_EVENT,
                serde_json::json!({ "line": line }),
            ),
            Command::Quit => {
                // Handled before this function is reached; quit is answered by closing.
                Event::new(
                    &MEMCACHED_UNKNOWN_COMMAND_EVENT,
                    serde_json::json!({ "line": "quit" }),
                )
            }
        }
    }
}
