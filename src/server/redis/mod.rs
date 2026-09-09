//! Redis server implementation with RESP protocol
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::{encode_error, RedisProtocol, REDIS_COMMAND_EVENT};
use anyhow::Result;
use redis_protocol::resp2::decode::decode;
use redis_protocol::resp2::types::OwnedFrame as Frame;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// Maximum bytes we will buffer for a single, still-incomplete RESP frame.
///
/// `decode()` returns `Ok(None)` while a frame is incomplete, so without this cap a client
/// that announces a huge bulk string (`$2000000000\r\n`) and then stalls would make the
/// connection buffer grow without bound. Real Redis caps a bulk string at 512 MB; 64 MB is
/// far more than any LLM-authored command needs.
const MAX_PENDING_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Redis server implementation
pub struct RedisServer {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    #[allow(dead_code)]
    status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
}

impl RedisServer {
    /// Create a new Redis server
    pub fn new(
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
    ) -> Self {
        Self {
            llm_client,
            app_state,
            status_tx,
            server_id,
        }
    }

    /// Spawn Redis server with LLM integration
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        _send_first: bool,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Redis server listening on {}", actual_addr));

        let server = Arc::new(RedisServer::new(
            llm_client,
            app_state.clone(),
            status_tx.clone(),
            Some(server_id),
        ));

        let status_tx_clone = status_tx.clone();
        let task_registrar = app_state.clone();

        // Spawn the accept loop
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        Log::new(Some(&status_tx)).info(format!("Redis connection from {}", addr));

                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(actual_addr);

                        // Track the connection
                        if let Some(server_id) = server.server_id {
                            use crate::state::server::{
                                ConnectionState as ServerConnectionState, ConnectionStatus,
                                ProtocolConnectionInfo,
                            };
                            let now = std::time::Instant::now();
                            let conn_state = ServerConnectionState {
                                id: connection_id,
                                remote_addr: addr,
                                local_addr: local_addr_conn,
                                bytes_sent: 0,
                                bytes_received: 0,
                                packets_sent: 0,
                                packets_received: 0,
                                last_activity: now,
                                status: ConnectionStatus::Active,
                                status_changed_at: now,
                                protocol_info: ProtocolConnectionInfo::empty(),
                            };
                            server
                                .app_state
                                .add_connection_to_server(server_id, conn_state)
                                .await;
                        }

                        let handler = RedisHandler {
                            connection_id,
                            llm_client: server.llm_client.clone(),
                            app_state: server.app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id: server.server_id,
                        };

                        let conn_handle = tokio::spawn(async move {
                            if let Err(e) = handler.handle_connection(stream).await {
                                error!("Redis connection error: {:?}", e);
                            }
                        });

                        // Register the per-connection task too, not just the accept loop:
                        // aborting the accept loop releases the port but leaves every
                        // in-flight session running, so `stop_server` did not actually stop
                        // the server. `register_server_task` prunes finished handles on each
                        // call, so this cannot grow without bound.
                        if let Some(server_id) = server.server_id {
                            server
                                .app_state
                                .register_server_task(server_id, conn_handle)
                                .await;
                        }
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Redis accept error: {}", e));
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
        Ok(actual_addr)
    }
}

/// Redis connection handler
struct RedisHandler {
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
}

impl RedisHandler {
    /// Run the connection, then always mark it closed in `AppState`.
    ///
    /// Without this the connection stays `Active` forever in the server's connection map,
    /// which is what `src/server/redis/CLAUDE.md` already claimed happened but did not.
    async fn handle_connection(self, stream: TcpStream) -> Result<()> {
        let server_id = self.server_id;
        let connection_id = self.connection_id;
        let app_state = self.app_state.clone();
        let status_tx = self.status_tx.clone();

        let result = self.run(stream).await;

        // Every exit path - EOF, read error, decode error, close_this_connection, frame
        // cap - lands here, so the dashboard's peer rows go away with the connection.
        if let Some(server_id) = server_id {
            app_state
                .remove_peer_handle(server_id, connection_id.as_u32())
                .await;
            app_state
                .close_connection_on_server(server_id, connection_id)
                .await;
            let _ = status_tx.send("__UPDATE_UI__".to_string());
        }

        result
    }

    async fn run(self, stream: TcpStream) -> Result<()> {
        let protocol = Arc::new(RedisProtocol::new(
            self.connection_id,
            self.app_state.clone(),
            self.status_tx.clone(),
        ));

        // The write half is shared with the peer command task below, so the dashboard can
        // write into this connection while the read loop is parked in `read()`.
        let (mut read_half, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

        // Peer messaging: "message this peer" / "disconnect this peer" inject actions into
        // THIS connection through the same executor the LLM path uses. Registered before the
        // first read, so a manual `*` rule parking the first command still leaves the
        // operator able to reach the connection. Every reply verb returns
        // `ActionResult::Output` with the RESP bytes pre-encoded (see actions.rs), so the
        // generic peer task writes injected replies to the wire; `close_connection`
        // half-closes.
        if let Some(server_id) = self.server_id {
            let peer_rx = crate::server::peer_support::register_peer_channel(
                &self.app_state,
                server_id,
                self.connection_id.as_u32(),
            )
            .await;
            crate::server::peer_support::spawn_peer_command_task(
                peer_rx,
                protocol.clone(),
                self.app_state.clone(),
                server_id,
                self.connection_id.as_u32(),
                write_half.clone(),
                self.status_tx.clone(),
            );
            let _ = self.status_tx.send("__UPDATE_UI__".to_string());
        }

        let mut buffer = Vec::new();
        let log = Log::new(Some(&self.status_tx));

        loop {
            // Read data from the stream
            let mut chunk = vec![0u8; 4096];
            let n = match read_half.read(&mut chunk).await {
                Ok(0) => {
                    debug!("Redis client disconnected");
                    return Ok(());
                }
                Ok(n) => n,
                Err(e) => {
                    error!("Redis read error: {}", e);
                    return Err(e.into());
                }
            };

            buffer.extend_from_slice(&chunk[..n]);

            if let Some(server_id) = self.server_id {
                self.app_state
                    .update_connection_stats(
                        server_id,
                        self.connection_id,
                        Some(n as u64),
                        None,
                        Some(1),
                        None,
                    )
                    .await;
            }

            if buffer.len() > MAX_PENDING_FRAME_BYTES {
                log.error(format!(
                    "Redis client {} exceeded the {} byte frame buffer limit without completing a frame; closing",
                    self.connection_id, MAX_PENDING_FRAME_BYTES
                ));
                let reply = encode_error("ERR Protocol error: invalid multibulk length");
                {
                    let mut writer = write_half.lock().await;
                    let _ = writer.write_all(&reply).await;
                    let _ = writer.flush().await;
                }
                if let Some(server_id) = self.server_id {
                    self.app_state
                        .update_connection_stats(
                            server_id,
                            self.connection_id,
                            None,
                            Some(reply.len() as u64),
                            None,
                            Some(1),
                        )
                        .await;
                }
                return Ok(());
            }

            // Try to decode RESP frames
            let mut offset = 0;
            while offset < buffer.len() {
                match decode(&buffer[offset..]) {
                    Ok(Some((frame, consumed))) => {
                        trace!("Redis frame: {:?}", frame);

                        // Extract command from frame. FileOnly: the redis_command
                        // event template surfaces the command to the TUI.
                        let command_str = frame_to_command_string(&frame);
                        // Truncated for the log and the status channel only - the event below
                        // still carries the whole command, because that is what the model has
                        // to answer. A single 64 MB bulk string would otherwise be copied
                        // verbatim into the *unbounded* status channel on every frame.
                        let command_for_log = crate::utils::truncate_for_log(&command_str, 512);
                        log.debug(format!("Redis command: {}", command_for_log));

                        // Create command event
                        let event = Event::new(
                            &REDIS_COMMAND_EVENT,
                            serde_json::json!({
                                "command": command_str.clone(),
                            }),
                        );

                        let server_id = self
                            .server_id
                            .unwrap_or_else(|| crate::state::ServerId::new(0));

                        let llm_result = call_llm(
                            &self.llm_client,
                            &self.app_state,
                            server_id,
                            Some(self.connection_id),
                            &event,
                            protocol.as_ref(),
                        )
                        .await;

                        // Collect the RESP bytes the actions produced, then write once so
                        // connection stats stay accurate and a close request still flushes
                        // whatever the LLM asked us to say first. The actions arrive already
                        // RESP-encoded as `ActionResult::Output` — the encoding lives once,
                        // in `actions.rs::execute_action`, shared with peer injection.
                        let mut response = Vec::new();
                        let mut close_after_write = false;

                        match llm_result {
                            Ok(execution_result) => {
                                let mut stack: Vec<ActionResult> =
                                    execution_result.protocol_results;
                                stack.reverse();
                                while let Some(result) = stack.pop() {
                                    match result {
                                        ActionResult::Output(bytes) => {
                                            response.extend_from_slice(&bytes);
                                        }
                                        ActionResult::Multiple(items) => {
                                            for inner in items.into_iter().rev() {
                                                stack.push(inner);
                                            }
                                        }
                                        ActionResult::CloseConnection => {
                                            debug!("Redis closing connection");
                                            close_after_write = true;
                                        }
                                        ActionResult::Custom { name, .. } => {
                                            // No Redis action returns Custom any more; one
                                            // reappearing would silently write nothing, so
                                            // make that loud.
                                            warn!(
                                                "Redis: action result '{}' is Custom, not Output; client will receive nothing for it",
                                                name
                                            );
                                        }
                                        _ => {
                                            // Other action results are informational
                                        }
                                    }
                                }

                                if response.is_empty() && !close_after_write {
                                    // Redis is strictly request/response: a command with no
                                    // reply hangs the client until its own timeout.
                                    // `decision=` tags mirror `src/server/radius/`: on the
                                    // wire a refusal, an outage and a model that answered
                                    // with nothing usable all look like "-ERR something", so
                                    // only the log can tell them apart.
                                    warn!(
                                        "Redis connection {} decision=fail_closed_no_action command='{}': \
                                         no response action produced, replying with an error",
                                        self.connection_id, command_for_log
                                    );
                                    response.extend_from_slice(&encode_error(
                                        "ERR no response produced for this command",
                                    ));
                                }
                            }
                            Err(e) => {
                                // A RESP error is the only thing a client can do anything
                                // with here, and `redis-rs`, `jedis` and `redis-py` all
                                // surface it as an exception rather than a value - so it
                                // cannot be mistaken for a reply to the command.
                                //
                                // `-LOADING` when the backend is merely saturated: clients
                                // already know that prefix means "not ready yet, retry", and
                                // several retry it automatically. Anything else is `-ERR`.
                                let overloaded = crate::llm::is_overload_error(&e);
                                let message = redis_error_message(&e, overloaded);
                                let decision = if overloaded {
                                    "fail_closed_llm_overloaded"
                                } else {
                                    "fail_closed_llm_error"
                                };
                                log.warn(format!(
                                    "Redis connection {} decision={} command='{}' replying -{}: {}",
                                    self.connection_id, decision, command_for_log, message, e
                                ));
                                response.extend_from_slice(&encode_error(&message));
                            }
                        }

                        if !response.is_empty() {
                            {
                                // Guard dropped before the next `call_llm` await.
                                let mut writer = write_half.lock().await;
                                writer.write_all(&response).await?;
                                writer.flush().await?;
                            }
                            if let Some(server_id) = self.server_id {
                                self.app_state
                                    .update_connection_stats(
                                        server_id,
                                        self.connection_id,
                                        None,
                                        Some(response.len() as u64),
                                        None,
                                        Some(1),
                                    )
                                    .await;
                            }
                        }

                        if close_after_write {
                            // The peer command task holds a clone of the write half until its
                            // channel closes; half-close now so the client sees EOF at once.
                            let _ = write_half.lock().await.shutdown().await;
                            return Ok(());
                        }

                        offset += consumed;
                    }
                    Ok(None) => {
                        // Need more data
                        break;
                    }
                    Err(e) => {
                        // Invalid frame
                        error!("Redis decode error: {:?}", e);
                        return Err(e.into());
                    }
                }
            }

            // Remove processed bytes from buffer
            buffer.drain(..offset);
        }
    }
}

/// Convert RESP frame to command string for display
fn frame_to_command_string(frame: &Frame) -> String {
    match frame {
        Frame::Array(frames) => {
            let parts: Vec<String> = frames
                .iter()
                .map(|f| match f {
                    Frame::BulkString(bytes) => String::from_utf8_lossy(bytes).to_string(),
                    Frame::SimpleString(bytes) => String::from_utf8_lossy(bytes).to_string(),
                    Frame::Integer(i) => i.to_string(),
                    _ => format!("{:?}", f),
                })
                .collect();
            parts.join(" ")
        }
        _ => format!("{:?}", frame),
    }
}

/// The RESP simple-error payload to send when the LLM backend fails.
///
/// The text is a category, never the error itself (`crate::utils::wire_failure`). That also
/// settles a framing hazard: a RESP simple error is CRLF-terminated with no length prefix, so
/// a newline anywhere in the text ends the frame early and the remainder is parsed as the
/// *next* reply - and backend error strings are routinely multi-line (a timeout with a URL, a
/// serde error with a snippet), so it would have desynchronised the connection for good.
fn redis_error_message(err: &anyhow::Error, overloaded: bool) -> String {
    let text = crate::utils::WireFailure::classify(err).prefixed_text();
    if overloaded {
        format!("LOADING {text}")
    } else {
        format!("ERR {text}")
    }
}
