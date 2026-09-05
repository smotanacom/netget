//! IMAP server implementation
//!
//! This module implements an IMAP (Internet Message Access Protocol) server
//! that allows LLM control over email retrieval and mailbox management.
//!
//! Key points:
//! - IMAP4rev1 commands parsed by hand into (tag, command, args). There is no `imap-codec`
//!   and no grammar: anything the split does not model (literals, continuations, quoting) is
//!   passed to the model as raw text.
//! - Session state management (NotAuthenticated -> Authenticated -> Selected -> Logout)
//! - Plain TCP only. An `ImapServer::spawn_with_tls` used to sit here advertising IMAPS on
//!   993; nothing called it and it could not have worked - it fed a concatenated PEM string
//!   to `Identity::from_pkcs12`, which only accepts DER PKCS#12 - so it was removed rather
//!   than left as an implemented-looking feature.
//! - No mailbox storage: the model answers LIST/FETCH/SEARCH from its instruction and memory.

pub mod actions;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

#[cfg(feature = "imap")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "imap")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "imap")]
use crate::llm::ActionResult;
#[cfg(feature = "imap")]
use crate::logging::emit::Log;
#[cfg(feature = "imap")]
use crate::protocol::Event;
#[cfg(feature = "imap")]
use crate::server::connection::ConnectionId;
#[cfg(feature = "imap")]
use crate::server::ImapProtocol;
#[cfg(feature = "imap")]
use crate::state::app_state::AppState;
#[cfg(feature = "imap")]
use crate::state::server::{
    ConnectionStatus, ImapSessionState, ProtocolConnectionInfo, ProtocolState, ServerId,
};
#[cfg(feature = "imap")]
use actions::{IMAP_AUTH_EVENT, IMAP_COMMAND_EVENT, IMAP_CONNECTION_EVENT};
#[cfg(feature = "imap")]
use serde_json::json;

/// IMAP server that handles mail retrieval with LLM
pub struct ImapServer;

#[cfg(feature = "imap")]
impl ImapServer {
    /// Spawn IMAP server with integrated LLM actions (plain TCP on port 143)
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("IMAP server (action-based) listening on {}", local_addr);
        Log::new(Some(&status_tx)).info(format!("IMAP server listening on {}", local_addr));

        let protocol = Arc::new(ImapProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        debug!("IMAP connection {} from {}", connection_id, remote_addr);
                        let _ = status_tx.send(format!(
                            "→ IMAP connection {} from {}",
                            connection_id, remote_addr
                        ));

                        // Track connection in server state
                        let local_addr = stream.local_addr().unwrap_or(listen_addr);
                        let (read_half, write_half) = tokio::io::split(stream);
                        let write_half_arc = Arc::new(tokio::sync::Mutex::new(write_half));

                        // Add connection to app_state
                        app_state
                            .add_connection_to_server(
                                server_id,
                                crate::state::ConnectionState {
                                    id: connection_id,
                                    remote_addr,
                                    local_addr,
                                    bytes_sent: 0,
                                    bytes_received: 0,
                                    packets_sent: 0,
                                    packets_received: 0,
                                    last_activity: std::time::Instant::now(),
                                    status: ConnectionStatus::Active,
                                    status_changed_at: std::time::Instant::now(),
                                    protocol_info: ProtocolConnectionInfo::empty(),
                                },
                            )
                            .await;

                        // Peer messaging: the dashboard's "message this peer" /
                        // "disconnect this peer" inject actions into THIS connection through
                        // the same executor the LLM path uses. Registered before the greeting
                        // event, because a manual `*` rule can park that greeting for minutes
                        // and the operator must still be able to reach the connection while it
                        // waits. Every IMAP wire verb returns `ActionResult::Output` (and
                        // `close_connection` half-closes), so the generic task covers the whole
                        // vocabulary with no `Custom` gap.
                        let peer_rx = crate::server::peer_support::register_peer_channel(
                            &app_state,
                            server_id,
                            connection_id.as_u32(),
                        )
                        .await;
                        crate::server::peer_support::spawn_peer_command_task(
                            peer_rx,
                            protocol.clone(),
                            app_state.clone(),
                            server_id,
                            connection_id.as_u32(),
                            write_half_arc.clone(),
                            status_tx.clone(),
                        );
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let write_half_for_session = write_half_arc.clone();

                        tokio::spawn(async move {
                            let mut session = ImapSession {
                                reader: BufReader::new(read_half),
                                writer: write_half_for_session,
                                connection_id,
                                server_id,
                                remote_addr,
                                llm_client: llm_clone,
                                app_state: state_clone.clone(),
                                status_tx: status_clone.clone(),
                                protocol: protocol_clone,
                            };

                            // Handle IMAP session
                            if let Err(e) = session.handle().await {
                                error!("IMAP session error for {}: {}", connection_id, e);
                                Log::new(Some(&status_clone))
                                    .error(format!("IMAP session {} error: {}", connection_id, e));
                            }

                            // Every exit path of `handle()` - EOF, read error, refused
                            // greeting, LOGOUT - lands here, so this single cleanup removes the
                            // peer handle no matter how the session ended (idempotent with the
                            // peer task's own removal on an injected close).
                            state_clone
                                .remove_peer_handle(server_id, connection_id.as_u32())
                                .await;

                            // Mark connection as closed
                            state_clone
                                .update_connection_status(
                                    server_id,
                                    connection_id,
                                    ConnectionStatus::Closed,
                                )
                                .await;

                            info!("IMAP connection {} closed", connection_id);
                            let _ = status_clone
                                .send(format!("✗ IMAP connection {} closed", connection_id));
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept IMAP connection: {}", e));
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

#[cfg(feature = "imap")]
struct ImapSession<R, W> {
    reader: BufReader<R>,
    writer: Arc<tokio::sync::Mutex<W>>,
    connection_id: ConnectionId,
    server_id: ServerId,
    #[allow(dead_code)]
    remote_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<ImapProtocol>,
}

#[cfg(feature = "imap")]
impl<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin> ImapSession<R, W> {
    async fn handle(&mut self) -> Result<()> {
        // Send greeting via LLM. A failure here used to propagate and drop the socket without
        // a single byte written, so the client sat waiting for a banner that never arrived.
        // RFC 3501 lets a server refuse a connection with an untagged BYE; RFC 5530 gives it
        // a machine-readable reason.
        if let Err(e) = self.send_greeting().await {
            let (code, detail) = imap_failure_code(&e);
            error!(
                "IMAP greeting handler failed on connection {}: {}",
                self.connection_id, e
            );
            Log::new(Some(&self.status_tx)).error(format!(
                "IMAP connection {} refused with BYE [{}]: {}",
                self.connection_id, code, e
            ));
            let bye = format!("* BYE [{}] {}\r\n", code, detail);
            let _ = self.send_response(bye.as_bytes()).await;
            // `send_response` flushes; returning drops the session and closes the socket.
            return Ok(());
        }

        // Main command loop
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF - client disconnected
                    debug!("IMAP client {} disconnected", self.connection_id);
                    break;
                }
                Ok(n) => {
                    trace!(
                        "IMAP received {} bytes from {}: {}",
                        n,
                        self.connection_id,
                        line.trim()
                    );
                    Log::new(Some(&self.status_tx)).trace(format!("IMAP command: {}", line.trim()));

                    // Update bytes received
                    self.app_state
                        .update_connection_stats(
                            self.server_id,
                            self.connection_id,
                            Some(n as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;

                    // Parse and handle IMAP command
                    if let Err(e) = self.handle_command(&line).await {
                        // NO, not BAD. BAD means "I did not understand the command", which
                        // invites the client to give up on the command permanently; a backend
                        // failure is a refusal to execute a command we understood fine. The
                        // tag is echoed so the client can correlate, and the RFC 5530 code
                        // tells it whether retrying is worth anything.
                        let (code, detail) = imap_failure_code(&e);
                        error!(
                            "Error handling IMAP command on connection {}: {}",
                            self.connection_id, e
                        );
                        Log::new(Some(&self.status_tx)).error(format!(
                            "IMAP connection {} refusing command with NO [{}]: {}",
                            self.connection_id, code, e
                        ));

                        let (tag, _, _) = parse_imap_command(&line);
                        let error_response = format!("{} NO [{}] {}\r\n", tag, code, detail);
                        let _ = self.send_response(error_response.as_bytes()).await;
                    }

                    // Check if session should logout
                    if let Some((session_state, _, _)) = self
                        .app_state
                        .get_imap_connection_state(self.server_id, self.connection_id)
                        .await
                    {
                        if session_state == ImapSessionState::Logout {
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading IMAP command: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    async fn send_greeting(&mut self) -> Result<()> {
        let event = Event::new(&IMAP_CONNECTION_EVENT, json!({}));

        let result = call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await?;

        for action_result in result.protocol_results {
            match action_result {
                ActionResult::Output(data) => {
                    self.send_response(&data).await?;
                }
                ActionResult::CloseConnection => {
                    // Don't close on greeting
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_command(&mut self, line: &str) -> Result<()> {
        let (tag, command, args) = parse_imap_command(line);

        debug!(
            "IMAP command from {}: tag={}, command={}, args={}",
            self.connection_id, tag, command, args
        );

        // Get current session state
        let (session_state, authenticated_user, selected_mailbox) = self
            .app_state
            .get_imap_connection_state(self.server_id, self.connection_id)
            .await
            .unwrap_or((ImapSessionState::NotAuthenticated, None, None));

        // Handle LOGIN specially for authentication event
        if command.to_uppercase() == "LOGIN" {
            return self.handle_login(&tag, &args).await;
        }

        // Create event for LLM
        let event = Event::new(
            &IMAP_COMMAND_EVENT,
            json!({
                "tag": tag,
                "command": command,
                "args": args,
                "session_state": format!("{:?}", session_state),
                "authenticated_user": authenticated_user,
                "selected_mailbox": selected_mailbox,
            }),
        );

        let result = call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await?;

        // A command completes only when a line carrying the client's own tag
        // goes out: async-imap, Thunderbird and every other client read until
        // they see it. Untagged data alone leaves the client blocked forever,
        // so track what was actually written and guarantee the completion
        // below rather than trusting the model to have produced it.
        let mut sent_tagged_completion = false;
        let mut sent_any_output = false;
        let mut deferred = false;
        // Whether the tagged completion the model itself sent said OK. A SELECT the model
        // refused must not still move the session into `Selected` (see `update_session_state`).
        let mut tagged_completion_ok = false;

        // Execute actions returned by LLM
        for action_result in result.protocol_results {
            match action_result {
                ActionResult::Output(data) => {
                    sent_any_output = true;
                    let text = String::from_utf8_lossy(&data);
                    if text.lines().any(|line| {
                        line.trim_start()
                            .to_uppercase()
                            .starts_with(&format!("{} ", tag.to_uppercase()))
                    }) {
                        sent_tagged_completion = true;
                        tagged_completion_ok = tagged_ok_for(&text, &tag);
                    }
                    self.send_response(&data).await?;
                }
                ActionResult::CloseConnection => {
                    deferred = true;
                    // Update session state to Logout
                    self.app_state
                        .update_imap_session_state(
                            self.server_id,
                            self.connection_id,
                            ImapSessionState::Logout,
                        )
                        .await;
                }
                ActionResult::WaitForMore => {
                    deferred = true;
                    // Mark as accumulating (for multi-line commands like APPEND)
                    self.app_state
                        .update_imap_protocol_state(
                            self.server_id,
                            self.connection_id,
                            ProtocolState::Accumulating,
                        )
                        .await;
                }
                _ => {}
            }
        }

        // Guarantee the tagged completion. Without it the client waits out its
        // own timeout on a command the server considers finished — the single
        // most common way an IMAP answer goes wrong, because untagged data
        // reads like a complete reply to everything except the client.
        if !sent_tagged_completion && !deferred {
            let completion = if sent_any_output {
                // Untagged data went out, so the command did produce its answer;
                // only the terminator is missing.
                format!("{} OK {} completed\r\n", tag, command.to_uppercase())
            } else {
                // Nothing at all was written: fail closed with a refusal the
                // client can act on, distinguishable from a real success.
                warn!(
                    "IMAP command {} on connection {} produced no response; answering NO",
                    command, self.connection_id
                );
                format!(
                    "{} NO netget: no response was produced for {}\r\n",
                    tag,
                    command.to_uppercase()
                )
            };
            debug!(
                "IMAP appending missing tagged completion for {}: {}",
                command,
                completion.trim_end()
            );
            self.send_response(completion.as_bytes()).await?;
        }

        // Handle state transitions based on command.
        //
        // Gated on the command having actually succeeded. This used to run unconditionally, so
        // a SELECT the model refused with `tag NO` still moved the session to `Selected` and
        // recorded the mailbox: the client was told no, the server believed yes, and every
        // later FETCH/STORE operated against a mailbox the model had just declined. A command
        // still awaiting more data (`deferred`) has not completed at all, so it transitions
        // nothing either.
        let command_ok = if deferred {
            false
        } else if sent_tagged_completion {
            tagged_completion_ok
        } else {
            // The completion synthesised above: OK when untagged data went out, NO otherwise.
            sent_any_output
        };
        self.update_session_state(&command, &args, command_ok)
            .await?;

        Ok(())
    }

    async fn handle_login(&mut self, tag: &str, args: &str) -> Result<()> {
        // Parse LOGIN username password
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 2 {
            let response = format!("{} BAD LOGIN requires username and password\r\n", tag);
            return self.send_response(response.as_bytes()).await;
        }

        let username = parts[0].trim_matches('"');
        let password = parts[1].trim_matches('"');

        debug!("IMAP LOGIN attempt: username={}", username);

        let event = Event::new(
            &IMAP_AUTH_EVENT,
            json!({
                "tag": tag,
                "username": username,
                "password": password,
            }),
        );

        let result = call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await?;

        // Check if authentication was successful by looking for OK response
        let mut auth_success = false;
        for action_result in &result.protocol_results {
            if let ActionResult::Output(data) = action_result {
                let response = String::from_utf8_lossy(&data);
                if tagged_ok_for(&response, tag) {
                    auth_success = true;
                }
                self.send_response(&data).await?;
            } else if let ActionResult::CloseConnection = action_result {
                // Authentication failed, close connection
                self.app_state
                    .update_imap_session_state(
                        self.server_id,
                        self.connection_id,
                        ImapSessionState::Logout,
                    )
                    .await;
            }
        }

        // If authentication successful, update session state
        if auth_success {
            self.app_state
                .update_imap_connection_state(
                    self.server_id,
                    self.connection_id,
                    Some(ImapSessionState::Authenticated),
                    Some(Some(username.to_string())),
                    None,
                    None,
                )
                .await;
            debug!("IMAP user {} authenticated", username);
        }

        Ok(())
    }

    /// Apply the session-state transition a completed command implies.
    ///
    /// `command_ok` is whether the tagged completion the client received said OK. Only LOGOUT
    /// transitions regardless: the client is leaving either way, and refusing to record that
    /// would leave a dead session marked live. Everything else transitions only on success —
    /// server-side state must never claim more than the client was told.
    async fn update_session_state(
        &mut self,
        command: &str,
        args: &str,
        command_ok: bool,
    ) -> Result<()> {
        let cmd_upper = command.to_uppercase();

        if !command_ok && cmd_upper != "LOGOUT" {
            debug!(
                "IMAP {} on connection {} did not succeed (decision=no_state_change); \
                 session state unchanged",
                cmd_upper, self.connection_id
            );
            return Ok(());
        }

        match cmd_upper.as_str() {
            "SELECT" => {
                // Extract mailbox name
                let mailbox = args.split_whitespace().next().unwrap_or("INBOX");
                self.app_state
                    .update_imap_connection_state(
                        self.server_id,
                        self.connection_id,
                        Some(ImapSessionState::Selected),
                        None,
                        Some(Some(mailbox.trim_matches('"').to_string())),
                        Some(false),
                    )
                    .await;
                debug!("IMAP mailbox selected: {}", mailbox);
            }
            "EXAMINE" => {
                // Like SELECT but read-only
                let mailbox = args.split_whitespace().next().unwrap_or("INBOX");
                self.app_state
                    .update_imap_connection_state(
                        self.server_id,
                        self.connection_id,
                        Some(ImapSessionState::Selected),
                        None,
                        Some(Some(mailbox.trim_matches('"').to_string())),
                        Some(true),
                    )
                    .await;
                debug!("IMAP mailbox examined (read-only): {}", mailbox);
            }
            "CLOSE" => {
                // Close selected mailbox, back to Authenticated
                self.app_state
                    .update_imap_connection_state(
                        self.server_id,
                        self.connection_id,
                        Some(ImapSessionState::Authenticated),
                        None,
                        Some(None),
                        Some(false),
                    )
                    .await;
                debug!("IMAP mailbox closed");
            }
            "LOGOUT" => {
                self.app_state
                    .update_imap_session_state(
                        self.server_id,
                        self.connection_id,
                        ImapSessionState::Logout,
                    )
                    .await;
                debug!("IMAP session logout");
            }
            _ => {}
        }

        Ok(())
    }

    async fn send_response(&mut self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(data).await?;
        writer.flush().await?;
        drop(writer);

        // Update stats
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

        trace!("IMAP sent {} bytes to {}", data.len(), self.connection_id);
        Log::new(Some(&self.status_tx)).trace(format!("IMAP sent {} bytes", data.len()));

        Ok(())
    }
}

/// The RFC 5530 response code and human text to use when the LLM backend fails us.
///
/// `UNAVAILABLE` is defined as "temporary failure because a subsystem is down" and is the
/// signal a client should read as "retry later"; it is what capacity exhaustion deserves.
/// Anything else - the backend refusing, timing out, or answering with something we cannot
/// parse - is reported as `SERVERBUG`, which does not invite an immediate retry loop.
///
/// Both are refusals. Neither can be confused with success, which is the point: an `OK` here
/// would tell the client the command was carried out.
#[cfg(feature = "imap")]
fn imap_failure_code(err: &anyhow::Error) -> (&'static str, String) {
    // The text is a category, never the error itself (`crate::utils::wire_failure`), which
    // also makes it structurally impossible for a newline in an error to forge a second
    // response line.
    let failure = crate::utils::WireFailure::classify(err);
    let code = if failure.is_overloaded() {
        "UNAVAILABLE"
    } else {
        "SERVERBUG"
    };
    (code, failure.prefixed_text().to_string())
}

/// Does this payload contain the tagged `OK` completion for `tag`?
///
/// Used to decide whether a LOGIN succeeded, which makes it an authentication check, so it
/// parses rather than pattern-matches. The previous `response.contains("{tag} OK")` searched
/// the whole payload for that text anywhere: a refusal whose human-readable message quoted
/// the phrase - `A001 NO LOGIN failed, expected A001 OK` - authenticated the session, and so
/// did an untagged line that happened to contain it. RFC 3501 §7.1 puts the condition in the
/// second field of a line whose first field is the tag, and nowhere else.
#[cfg(feature = "imap")]
fn tagged_ok_for(payload: &str, tag: &str) -> bool {
    payload.lines().any(|line| {
        let mut fields = line.trim().split_whitespace();
        fields.next() == Some(tag) && fields.next().is_some_and(|status| status == "OK")
    })
}

/// Parse IMAP command line into (tag, command, args)
#[cfg(feature = "imap")]
fn parse_imap_command(line: &str) -> (String, String, String) {
    let trimmed = line.trim();
    let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();

    match parts.len() {
        0 => ("*".to_string(), "".to_string(), "".to_string()),
        1 => (parts[0].to_string(), "".to_string(), "".to_string()),
        2 => (parts[0].to_string(), parts[1].to_string(), "".to_string()),
        _ => (
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        ),
    }
}
