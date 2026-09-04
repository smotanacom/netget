//! Finger (RFC 1288) client.
//!
//! One TCP connection carries exactly one query line and one free-text answer, and the
//! **server** closes when it is done. A finger client is therefore defined by reading to EOF:
//! `run_query` does not stop at the first `read()`, and everything else in this file follows
//! from that.
//!
//! Nothing here parses the answer. RFC 1288 specifies no format for it, so the text goes to the
//! model and the model decides what it means — the `best_effort` block on the response event is
//! a guess and says so in its own description.
//!
//! Two things are deliberate and worth reading before changing them:
//!
//! * **`user@host` forwarding is refused before anything is written.** RFC 1288 §3.2.1 calls
//!   forwarding a security risk; as the *client* we would be the one asking a stranger's server
//!   to relay on our behalf. `allow_forwarding` (startup parameter, default false) is the only
//!   way to turn it on, and `send_finger_query` rejects an `@` inside `username`, so it can
//!   never happen implicitly.
//! * **A follow-up query opens a new connection, and raises the response event again.** RFC
//!   1288 is one query per connection, so a second query on the same socket would be ignored by
//!   every real server. The chain is bounded by [`MAX_FOLLOWUP_DEPTH`] and one query per turn,
//!   and the recursive call is boxed — the shape that has run away in this repo before.

pub mod actions;

pub use actions::FingerClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::actions::protocol_trait::Protocol;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};
use crate::utils::truncate_for_log;

use actions::{
    best_effort_fields, sanitize_response, FingerQuerySpec, DEFAULT_FINGER_PORT,
    FINGER_CLIENT_CONNECTED_EVENT, FINGER_CLIENT_RESPONSE_RECEIVED_EVENT, MAX_FOLLOWUP_DEPTH,
    MAX_RESPONSE_BYTES,
};

/// Settings read from the declared startup parameters.
#[derive(Clone, Copy, Debug)]
pub struct FingerClientConfig {
    /// Port used when `remote_addr` carries none of its own.
    pub default_port: u16,
    /// Whether `user@host` forwarding queries may reach the wire.
    pub allow_forwarding: bool,
    /// Seconds to wait for the server's answer; 0 means wait indefinitely.
    pub response_timeout_secs: u64,
}

impl Default for FingerClientConfig {
    fn default() -> Self {
        Self {
            default_port: DEFAULT_FINGER_PORT,
            allow_forwarding: actions::DEFAULT_ALLOW_FORWARDING,
            response_timeout_secs: actions::DEFAULT_RESPONSE_TIMEOUT_SECS,
        }
    }
}

impl FingerClientConfig {
    fn response_timeout(&self) -> Option<Duration> {
        if self.response_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(self.response_timeout_secs))
        }
    }
}

/// The query that actually went on the wire, whichever path put it there (the model's
/// `send_finger_query` or one injected from the dashboard). Read once the server closes, so the
/// response event names the query it answers.
type SentQuery = Arc<std::sync::Mutex<Option<FingerQuerySpec>>>;

/// What one executed action did.
enum Applied {
    /// Bytes written (0 when the action produced no wire output).
    Sent(usize),
    /// A forwarding query was refused by policy; nothing was written.
    Refused(String),
    /// The write side was shut down and the session should end.
    Disconnect,
}

/// What one read-to-EOF produced.
#[derive(Clone, Debug)]
struct Response {
    /// Sanitised text: control characters stripped, line endings normalised to LF.
    text: String,
    /// Bytes actually read off the socket.
    bytes: usize,
    /// The server closed the connection, so the answer is complete.
    eof: bool,
    /// [`MAX_RESPONSE_BYTES`] was reached and the rest discarded.
    truncated: bool,
}

/// Everything a follow-up query needs. Owned and `'static`, so the boxed recursive future is
/// `Send` and can live inside a `tokio::spawn`.
struct SessionCtx {
    protocol: Arc<FingerClientProtocol>,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    client_id: ClientId,
    target: String,
    instruction: String,
    config: FingerClientConfig,
}

/// Resolve `remote_addr` to a `host:port` this client can connect to.
///
/// An explicit port in `remote_addr` always wins; the `port` startup parameter (or 79) fills in
/// when there is none. Bare and bracketed IPv6 literals are handled, because `[::1]` and `::1`
/// both contain colons and a naive "does it contain ':'" test reads the second as having a port.
pub fn resolve_target(remote_addr: &str, default_port: u16) -> String {
    let addr = remote_addr.trim();

    if let Some(rest) = addr.strip_prefix('[') {
        // Bracketed IPv6: "[::1]" or "[::1]:7979".
        return match rest.find(']') {
            Some(close) if addr.len() > close + 2 && addr.as_bytes()[close + 2] == b':' => {
                addr.to_string()
            }
            Some(_) => format!("{addr}:{default_port}"),
            None => format!("{addr}:{default_port}"),
        };
    }

    match addr.matches(':').count() {
        0 => format!("{addr}:{default_port}"),
        1 => addr.to_string(),
        // Bare IPv6 literal with no port: brackets are required before one can be appended.
        _ => format!("[{addr}]:{default_port}"),
    }
}

pub struct FingerClient;

impl FingerClient {
    /// Connect to a finger server and drive it with the LLM.
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        config: FingerClientConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        let target = resolve_target(&remote_addr, config.default_port);

        let stream = TcpStream::connect(&target)
            .await
            .with_context(|| format!("Failed to connect to Finger server at {target}"))?;

        let local_addr = stream.local_addr()?;
        let remote_sock_addr = stream.peer_addr()?;

        info!(
            "Finger client {} connected to {} (local: {})",
            client_id, remote_sock_addr, local_addr
        );
        if config.allow_forwarding {
            warn!(
                "Finger client {} allow_forwarding=true: 'user@host' queries will be put on the \
                 wire, asking {} to relay on our behalf (RFC 1288 3.2.1 calls this a security \
                 risk)",
                client_id, target
            );
            let _ = status_tx.send(format!(
                "[CLIENT] ⚠ Finger client {} will send forwarding queries (allow_forwarding=true)",
                client_id
            ));
        }

        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] Finger client {} connected", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        let (read_half, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(Mutex::new(write_half));
        let protocol = Arc::new(FingerClientProtocol::new());
        let sent_query: SentQuery = Arc::new(std::sync::Mutex::new(None));

        // Registered BEFORE the connected-event LLM call, which a manual `*` rule can park for
        // as long as the operator takes. Until registration the dashboard reports "no command
        // channel", which reads as a protocol limitation when it is only a queue.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            protocol.clone(),
            write_half.clone(),
            sent_query.clone(),
            config,
            client_id,
            app_state.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        let task_registrar = app_state.clone();
        let session_state = app_state.clone();
        let session_status = status_tx.clone();
        let task_handle = tokio::spawn(async move {
            Self::session(
                read_half,
                write_half,
                protocol,
                sent_query,
                target,
                config,
                llm_client,
                session_state.clone(),
                session_status.clone(),
                client_id,
            )
            .await;

            // The session is over: drop the handle so the dashboard stops offering [ send ]
            // and the command task ends with its channel.
            session_state.remove_client_handle(client_id).await;
            session_state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
            let _ =
                session_status.send(format!("[CLIENT] Finger client {} disconnected", client_id));
            let _ = session_status.send("__UPDATE_UI__".to_string());
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(local_addr)
    }

    /// connected event -> one query -> read until the server closes -> response event.
    #[allow(clippy::too_many_arguments)]
    async fn session<R, W>(
        read_half: R,
        write_half: Arc<Mutex<W>>,
        protocol: Arc<FingerClientProtocol>,
        sent_query: SentQuery,
        target: String,
        config: FingerClientConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            warn!(
                "Finger client {} has no instruction; nothing will be asked",
                client_id
            );
            return;
        };

        let ctx = Arc::new(SessionCtx {
            protocol: protocol.clone(),
            llm_client,
            app_state: app_state.clone(),
            status_tx: status_tx.clone(),
            client_id,
            target: target.clone(),
            instruction,
            config,
        });

        let event = Event::new(
            &FINGER_CLIENT_CONNECTED_EVENT,
            serde_json::json!({
                "remote_addr": target,
                "allow_forwarding": config.allow_forwarding,
            }),
        );

        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        let mut hung_up = false;
        match call_llm_for_client(
            &ctx.llm_client,
            &app_state,
            client_id.to_string(),
            &ctx.instruction,
            &memory,
            Some(&event),
            protocol.as_ref(),
            &status_tx,
        )
        .await
        {
            Ok(ClientLlmResult {
                actions,
                memory_updates,
            }) => {
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }

                // The model's answer is executed, never counted and logged. A finger client
                // that connects and asks nothing is the whole failure mode this guards.
                for action in actions {
                    let result = match protocol.execute_action(action.clone()) {
                        Ok(result) => result,
                        Err(e) => {
                            error!("Finger client {} rejected action: {}", client_id, e);
                            let _ = status_tx.send(format!(
                                "[CLIENT] ✖ Finger client {} rejected an action: {}",
                                client_id, e
                            ));
                            continue;
                        }
                    };
                    match Self::apply_action(result, &write_half, &sent_query, config, client_id)
                        .await
                    {
                        Ok(Applied::Sent(_)) => {}
                        Ok(Applied::Refused(reason)) => {
                            warn!("Finger client {} {}", client_id, reason);
                            let _ = status_tx
                                .send(format!("[CLIENT] ⚠ Finger client {} {}", client_id, reason));
                        }
                        Ok(Applied::Disconnect) => {
                            info!(
                                "Finger client {} disconnected before sending a query",
                                client_id
                            );
                            hung_up = true;
                            break;
                        }
                        Err(e) => {
                            error!("Finger client {} failed to send query: {}", client_id, e);
                            app_state
                                .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                                .await;
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                // Stay connected: the operator can still inject a query from the dashboard,
                // and a finger server waits for one rather than closing on its own.
                error!("LLM error for Finger client {}: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[CLIENT] ⚠ Finger client {} LLM error: {} (still connected; a query can be \
                     injected)",
                    client_id, e
                ));
            }
        }

        if hung_up {
            let _ = write_half.lock().await.shutdown().await;
            return;
        }

        if sent_query.lock().map(|q| q.is_none()).unwrap_or(true) {
            info!(
                "Finger client {} has no query on the wire yet; waiting for an injected one or \
                 for {} to close",
                client_id, target
            );
        }

        let response = Self::read_response(read_half, config.response_timeout(), client_id).await;
        debug!(
            "Finger client {} read {} bytes (eof={}, truncated={})",
            client_id, response.bytes, response.eof, response.truncated
        );
        trace!(
            "Finger client {} response: {}",
            client_id,
            truncate_for_log(&response.text, 4096)
        );

        // The socket is finished with, so the dashboard must stop offering [ send ] on it now
        // rather than when the follow-up chain below runs out.
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        let spec = sent_query.lock().ok().and_then(|q| q.clone());
        match spec {
            Some(spec) => {
                let _ = status_tx.send(format!(
                    "[CLIENT] Finger client {} received {} bytes for {:?}",
                    client_id,
                    response.bytes,
                    spec.query_text()
                ));
                Self::handle_response(ctx, spec, response, 0).await;
            }
            None => {
                info!(
                    "Finger client {} closed with no query ever sent ({} bytes read)",
                    client_id, response.bytes
                );
            }
        }
    }

    /// Raise `finger_response_received`, then act on the answer.
    ///
    /// The model may reply with another `send_finger_query`. RFC 1288 is one query per
    /// connection — a second one on the same socket is ignored by every real server — so a
    /// follow-up opens a **fresh** connection and raises this event again.
    ///
    /// That is self-referential, which is why the recursion is boxed (an `async fn` awaiting
    /// itself has an infinitely-sized future) and bounded twice: [`MAX_FOLLOWUP_DEPTH`] limits
    /// the chain, and at most one query per turn is followed so the chain cannot fan out.
    fn handle_response(
        ctx: Arc<SessionCtx>,
        spec: FingerQuerySpec,
        response: Response,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            let client_id = ctx.client_id;

            let event = Event::new(
                &FINGER_CLIENT_RESPONSE_RECEIVED_EVENT,
                serde_json::json!({
                    "response": response.text,
                    "query": spec.query_text(),
                    "username": spec.username,
                    "verbose": spec.verbose,
                    "forward_host": spec.forward_host,
                    "bytes": response.bytes,
                    "eof": response.eof,
                    "truncated": response.truncated,
                    "best_effort": best_effort_fields(&response.text),
                }),
            );

            let memory = ctx
                .app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            let result = call_llm_for_client(
                &ctx.llm_client,
                &ctx.app_state,
                client_id.to_string(),
                &ctx.instruction,
                &memory,
                Some(&event),
                ctx.protocol.as_ref(),
                &ctx.status_tx,
            )
            .await;

            let ClientLlmResult {
                actions,
                memory_updates,
            } = match result {
                Ok(result) => result,
                Err(e) => {
                    error!(
                        "LLM error for Finger client {} on response event: {}",
                        client_id, e
                    );
                    return;
                }
            };

            if let Some(mem) = memory_updates {
                ctx.app_state.set_memory_for_client(client_id, mem).await;
            }

            // Execute the answer. A follow-up is a whole new connection, so only the first
            // query in one answer is followed; the rest are refused out loud rather than
            // quietly dropped, because k queries per turn at depth d is k^d connections.
            let mut followed = false;
            for action in actions {
                let result = match ctx.protocol.execute_action(action.clone()) {
                    Ok(result) => result,
                    Err(e) => {
                        error!("Finger client {} rejected action: {}", client_id, e);
                        continue;
                    }
                };

                match result {
                    ClientActionResult::Custom { name, data } if name == "finger_query" => {
                        let next = match FingerQuerySpec::from_json(&data) {
                            Ok(next) => next,
                            Err(e) => {
                                error!("Finger client {} built an invalid query: {}", client_id, e);
                                continue;
                            }
                        };

                        if next.is_forwarding() && !ctx.config.allow_forwarding {
                            warn!(
                                "Finger client {} decision=forward_refused query={:?} \
                                 forward_host={:?}: RFC 1288 3.2.1 calls forwarding a security \
                                 risk; start the client with allow_forwarding=true to permit it",
                                client_id,
                                next.query_text(),
                                next.forward_host
                            );
                            let _ = ctx.status_tx.send(format!(
                                "[CLIENT] ⚠ Finger client {} refused a forwarding follow-up",
                                client_id
                            ));
                            continue;
                        }

                        if followed {
                            warn!(
                                "Finger client {} ignoring extra follow-up query {:?}: one query \
                                 per turn, because each needs its own connection",
                                client_id,
                                next.query_text()
                            );
                            continue;
                        }
                        if depth + 1 >= MAX_FOLLOWUP_DEPTH {
                            warn!(
                                "Finger client {} reached MAX_FOLLOWUP_DEPTH ({}); not following \
                                 up with {:?}",
                                client_id,
                                MAX_FOLLOWUP_DEPTH,
                                next.query_text()
                            );
                            let _ = ctx.status_tx.send(format!(
                                "[CLIENT] ⚠ Finger client {} hit the follow-up depth limit",
                                client_id
                            ));
                            continue;
                        }
                        followed = true;

                        info!(
                            "Finger client {} following up with {:?} on a new connection to {} \
                             (depth {})",
                            client_id,
                            next.query_text(),
                            ctx.target,
                            depth + 1
                        );

                        match Self::run_query(&ctx.target, &next, ctx.config.response_timeout())
                            .await
                        {
                            Ok(next_response) => {
                                let _ = ctx.status_tx.send(format!(
                                    "[CLIENT] Finger client {} received {} bytes for {:?}",
                                    client_id,
                                    next_response.bytes,
                                    next.query_text()
                                ));
                                Self::handle_response(ctx.clone(), next, next_response, depth + 1)
                                    .await;
                            }
                            Err(e) => {
                                error!(
                                    "Finger client {} follow-up query {:?} failed: {}",
                                    client_id,
                                    next.query_text(),
                                    e
                                );
                                let _ = ctx.status_tx.send(format!(
                                    "[CLIENT] ✖ Finger client {} follow-up failed: {}",
                                    client_id, e
                                ));
                            }
                        }
                    }
                    ClientActionResult::Disconnect => {
                        debug!(
                            "Finger client {} asked to disconnect; the server already closed",
                            client_id
                        );
                    }
                    ClientActionResult::WaitForMore => {
                        debug!(
                            "Finger client {} waiting; the server already closed this connection",
                            client_id
                        );
                    }
                    other => {
                        trace!(
                            "Finger client {} produced an inert result: {:?}",
                            client_id,
                            other
                        );
                    }
                }
            }
        })
    }

    /// Open a fresh connection, ask one question, read the answer to EOF, close.
    ///
    /// The new connection is not incidental: RFC 1288 gives the server one query and then has
    /// it close, so a follow-up genuinely needs its own.
    async fn run_query(
        target: &str,
        spec: &FingerQuerySpec,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let stream = TcpStream::connect(target)
            .await
            .with_context(|| format!("Finger follow-up could not reach {target}"))?;
        let (read_half, mut write_half) = tokio::io::split(stream);

        write_half
            .write_all(spec.to_wire_line().as_bytes())
            .await
            .context("Finger follow-up failed to write the query")?;
        write_half
            .flush()
            .await
            .context("Finger follow-up failed to flush the query")?;

        Ok(Self::read_response_inner(read_half, timeout).await)
    }

    /// Read until the server closes, bounded by size and by time.
    ///
    /// A finger client is *defined* by reading to EOF — the server closes when its answer is
    /// finished and there is no length anywhere in the protocol. Both bounds exist because a
    /// server that never closes would otherwise park this task forever and a server that
    /// streams would fill memory.
    async fn read_response<R>(
        read_half: R,
        timeout: Option<Duration>,
        client_id: ClientId,
    ) -> Response
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let response = Self::read_response_inner(read_half, timeout).await;
        if !response.eof && !response.truncated {
            warn!(
                "Finger client {} stopped reading after {} bytes without seeing EOF; the server \
                 did not close",
                client_id, response.bytes
            );
        }
        if response.truncated {
            warn!(
                "Finger client {} truncated the response at {} bytes",
                client_id, MAX_RESPONSE_BYTES
            );
        }
        response
    }

    async fn read_response_inner<R>(mut read_half: R, timeout: Option<Duration>) -> Response
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut raw: Vec<u8> = Vec::new();
        let mut truncated = false;

        let read_loop = async {
            let mut buf = vec![0u8; 4096];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => return true,
                    Ok(n) => {
                        let room = MAX_RESPONSE_BYTES.saturating_sub(raw.len());
                        if n >= room {
                            raw.extend_from_slice(&buf[..room]);
                            truncated = true;
                            return false;
                        }
                        raw.extend_from_slice(&buf[..n]);
                    }
                    Err(e) => {
                        debug!("Finger client read error: {}", e);
                        return false;
                    }
                }
            }
        };

        let eof = match timeout {
            Some(duration) => tokio::time::timeout(duration, read_loop)
                .await
                .unwrap_or(false),
            None => read_loop.await,
        };

        let bytes = raw.len();
        Response {
            text: sanitize_response(&String::from_utf8_lossy(&raw)),
            bytes,
            eof,
            truncated,
        }
    }

    /// Put one executed action on the wire.
    ///
    /// Shared by the LLM path and the dashboard's injected commands, so the query encoding —
    /// and the forwarding refusal — exist in exactly one place. The refusal lives here rather
    /// than in `execute_action` because this is where the startup parameter is: the executor
    /// decides what a *legal* query is, the loop decides what this client is *allowed* to send.
    async fn apply_action<W>(
        result: ClientActionResult,
        write_half: &Arc<Mutex<W>>,
        sent_query: &SentQuery,
        config: FingerClientConfig,
        client_id: ClientId,
    ) -> Result<Applied>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        match result {
            ClientActionResult::Custom { name, data } if name == "finger_query" => {
                let spec = FingerQuerySpec::from_json(&data)?;

                if spec.is_forwarding() && !config.allow_forwarding {
                    let reason = format!(
                        "decision=forward_refused query={:?} forward_host={:?}: RFC 1288 3.2.1 \
                         calls 'user@host' forwarding a security risk, and this client was not \
                         started with allow_forwarding=true. Nothing was sent.",
                        spec.query_text(),
                        spec.forward_host
                    );
                    return Ok(Applied::Refused(reason));
                }

                let line = spec.to_wire_line();
                debug!(
                    "Finger client {} querying: {:?}",
                    client_id,
                    spec.query_text()
                );
                {
                    let mut writer = write_half.lock().await;
                    writer.write_all(line.as_bytes()).await?;
                    writer.flush().await?;
                }
                if let Ok(mut slot) = sent_query.lock() {
                    // RFC 1288 is one query per connection: the first is the one the response
                    // answers, and a second on the same socket is ignored by real servers.
                    slot.get_or_insert(spec);
                }
                Ok(Applied::Sent(line.len()))
            }
            ClientActionResult::Disconnect => {
                debug!("Finger client {} disconnecting", client_id);
                // Half-close: the server reads EOF and closes, and the read loop then sees 0
                // and runs its normal path.
                let _ = write_half.lock().await.shutdown().await;
                Ok(Applied::Disconnect)
            }
            // WaitForMore, NoAction, SendData, an unknown Custom, a nested Multiple.
            _ => Ok(Applied::Sent(0)),
        }
    }

    /// Drain injected commands until the channel closes (session over, or client removed) or
    /// an injected `disconnect` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot run this client's
    /// vocabulary, because `send_finger_query` yields `ClientActionResult::Custom` and the
    /// generic arm has no way to encode one. So the action goes through
    /// [`Self::apply_action`] — the same function the LLM path uses — and the outcome is
    /// recorded and replied exactly the way the generic arm does it.
    #[allow(clippy::too_many_arguments)]
    async fn command_loop<W>(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        protocol: Arc<FingerClientProtocol>,
        write_half: Arc<Mutex<W>>,
        sent_query: SentQuery,
        config: FingerClientConfig,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) where
        W: tokio::io::AsyncWrite + Unpin,
    {
        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => {
                    Self::apply_action(result, &write_half, &sent_query, config, client_id)
                        .await
                        .map(|applied| match applied {
                            Applied::Disconnect => ClientSendOutcome::Disconnected,
                            Applied::Refused(reason) => {
                                ClientSendOutcome::Rejected { error: reason }
                            }
                            Applied::Sent(0) => ClientSendOutcome::Executed {
                                detail: "executed (nothing to write)".to_string(),
                            },
                            Applied::Sent(bytes_sent) => ClientSendOutcome::Sent { bytes_sent },
                        })
                }
            };

            let outcome_json = match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            };
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![outcome_json],
                )
                .await;

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            match &outcome {
                Ok(ClientSendOutcome::Rejected { error }) => {
                    warn!(
                        "Finger client {} refused an injected action: {}",
                        client_id, error
                    );
                    let _ = status_tx.send(format!(
                        "[CLIENT] ⚠ Finger client {} refused an injected action: {}",
                        client_id, error
                    ));
                }
                Err(e) => {
                    error!("Finger client {} injected action failed: {}", client_id, e);
                    let _ = status_tx.send(format!(
                        "[CLIENT] ✖ Finger client {} injected action failed: {}",
                        client_id, e
                    ));
                }
                _ => {}
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                // Do not wait for the server to answer the half-close with its own FIN: the
                // rail must stop offering [ send ] on this client now.
                app_state.remove_client_handle(client_id).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                break;
            }
        }
    }
}
