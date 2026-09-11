//! Telnet server implementation
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::TelnetProtocol;
use crate::state::app_state::AppState;
use actions::{TELNET_CONNECTION_OPENED_EVENT, TELNET_MESSAGE_RECEIVED_EVENT};

/// Truncate `text` to at most `max` bytes without splitting a UTF-8 character.
///
/// Slicing `&text[..max]` directly panics when the boundary lands inside a multi-byte
/// character, and both the received line and the handler's response are attacker- or
/// model-controlled, so that panic is reachable from the network.
#[cfg(feature = "telnet")]
fn preview(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// A telnet line longer than this is not a line anyone typed.
///
/// `BufReader::read_line` grows its `String` until a newline arrives, so before this cap
/// existed an unauthenticated peer could hold the connection open and stream gigabytes with
/// no `\n` among them, and the server buffered every byte. Nothing authenticates before this
/// loop, and telnet is the protocol an operator is most likely to point at a real device.
#[cfg(feature = "telnet")]
const MAX_LINE_BYTES: usize = 8192;

#[cfg(feature = "telnet")]
mod iac {
    pub const IAC: u8 = 255;
    pub const SE: u8 = 240;
    pub const SB: u8 = 250;
    pub const WILL: u8 = 251;
    pub const DONT: u8 = 254;
}

/// Where the IAC state machine is between reads.
///
/// The whole machine is this one enum plus a buffer capped at [`MAX_LINE_BYTES`], which is
/// what makes "can option negotiation be driven into unbounded state?" answerable: it cannot.
/// A subnegotiation that never sends `IAC SE` discards bytes as they arrive rather than
/// accumulating them, and no arm can panic.
#[cfg(feature = "telnet")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum IacState {
    /// Ordinary data.
    Data,
    /// `IAC` seen; the next byte is the command.
    Command,
    /// `IAC WILL/WONT/DO/DONT` seen; the next byte is the option, and is consumed.
    Option,
    /// Inside `IAC SB …`; everything is consumed until `IAC SE`.
    Subnegotiation,
    /// `IAC` inside a subnegotiation: `SE` ends it, `IAC` is an escaped 0xFF.
    SubnegotiationIac,
}

/// What one attempt to read a line produced.
#[cfg(feature = "telnet")]
enum LineRead {
    /// A line with its terminator stripped, and the wire bytes it consumed.
    Line(Vec<u8>, usize),
    /// The peer hung up with nothing pending.
    Eof,
    /// [`MAX_LINE_BYTES`] of data arrived with no newline in it.
    TooLong,
    /// The socket errored.
    Failed(std::io::Error),
}

/// Reads `\n`-terminated lines with IAC sequences removed and a hard size cap.
///
/// Two defects this replaced, both reachable from any peer:
///
/// * **Unbounded buffering.** See [`MAX_LINE_BYTES`].
/// * **A real `telnet(1)` client was dropped on its first line.** `read_line` validates UTF-8
///   across the whole line and returns `InvalidData` when that fails. A real client opens by
///   sending `IAC DO/WILL …` negotiation, whose bytes are not valid UTF-8, so the first line
///   the user typed arrived with that junk in front of it, failed to decode, and the read loop
///   treated the error as a reason to close. `actions.rs` told the model those bytes "arrive
///   as part of the first message", which was never what happened — the connection died
///   instead. Sequences are now removed from the stream and what is left is decoded lossily,
///   so the client survives and the model sees what was typed.
///
/// Stripping has to happen on the byte stream *before* lines are split out, not on a line
/// already cut at a newline: an option byte can itself be `0x0A` (`IAC DO NAOCRD` is
/// `FF FD 0A`), and a sequence may straddle two reads.
///
/// Negotiation is still **not answered**: options are recognised only well enough to be
/// skipped, so a real client gets no reply to its offers and stays in its default line mode,
/// which is the mode this server can serve.
#[cfg(feature = "telnet")]
struct TelnetLineReader<R> {
    reader: R,
    /// Data bytes received and not yet returned as a line.
    pending: Vec<u8>,
    chunk: Vec<u8>,
    state: IacState,
}

#[cfg(feature = "telnet")]
impl<R: tokio::io::AsyncRead + Unpin> TelnetLineReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            chunk: vec![0u8; 4096],
            state: IacState::Data,
        }
    }

    /// Feed raw bytes through the IAC machine, appending only data bytes to `pending`.
    fn absorb(&mut self, raw: &[u8]) {
        for &byte in raw {
            self.state = match self.state {
                IacState::Data => {
                    if byte == iac::IAC {
                        IacState::Command
                    } else {
                        self.pending.push(byte);
                        IacState::Data
                    }
                }
                IacState::Command => match byte {
                    // `IAC IAC` is a literal 0xFF in the data stream.
                    iac::IAC => {
                        self.pending.push(iac::IAC);
                        IacState::Data
                    }
                    iac::SB => IacState::Subnegotiation,
                    iac::WILL..=iac::DONT => IacState::Option,
                    // Every other command (NOP, DM, BRK, AYT, GA …) is two bytes in total.
                    _ => IacState::Data,
                },
                IacState::Option => IacState::Data,
                IacState::Subnegotiation => {
                    if byte == iac::IAC {
                        IacState::SubnegotiationIac
                    } else {
                        IacState::Subnegotiation
                    }
                }
                IacState::SubnegotiationIac => match byte {
                    iac::SE => IacState::Data,
                    // Anything else, including an escaped 0xFF, is still payload we discard.
                    _ => IacState::Subnegotiation,
                },
            };
        }
    }

    async fn next_line(&mut self) -> LineRead {
        use tokio::io::AsyncReadExt;
        loop {
            if let Some(idx) = self.pending.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=idx).collect();
                let consumed = line.len();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return LineRead::Line(line, consumed);
            }
            if self.pending.len() > MAX_LINE_BYTES {
                return LineRead::TooLong;
            }

            let n = match self.reader.read(&mut self.chunk).await {
                Ok(0) => {
                    if self.pending.is_empty() {
                        return LineRead::Eof;
                    }
                    // A final line with no terminator: answer it rather than discarding what
                    // the peer said on its way out.
                    let mut line = std::mem::take(&mut self.pending);
                    let consumed = line.len();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return LineRead::Line(line, consumed);
                }
                Ok(n) => n,
                Err(e) => return LineRead::Failed(e),
            };
            let raw = self.chunk[..n].to_vec();
            self.absorb(&raw);
        }
    }
}

/// Telnet server that forwards messages to LLM
pub struct TelnetServer;

#[cfg(feature = "telnet")]
impl TelnetServer {
    /// Spawn Telnet server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        send_first: bool,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("Telnet server listening on {}", local_addr));

        let protocol = Arc::new(TelnetProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let (read_half, write_half) = tokio::io::split(stream);
                            let write_half_arc = Arc::new(tokio::sync::Mutex::new(write_half));

                            // Add connection to ServerInstance
                            use crate::state::server::{
                                ConnectionState as ServerConnectionState, ConnectionStatus,
                                ProtocolConnectionInfo,
                            };
                            let now = std::time::Instant::now();
                            let conn_state = ServerConnectionState {
                                id: connection_id,
                                remote_addr,
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
                            state_clone
                                .add_connection_to_server(server_id, conn_state)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                            let log = Log::new(Some(&status_clone));

                            // Peer messaging: the dashboard's "message this peer" and
                            // "disconnect" inject actions into THIS connection through
                            // the same executor the handlers use. The handle is removed
                            // on every exit path below; the command task ends with it.
                            let peer_rx = crate::server::peer_support::register_peer_channel(
                                &state_clone,
                                server_id,
                                connection_id.as_u32(),
                            )
                            .await;
                            crate::server::peer_support::spawn_peer_command_task(
                                peer_rx,
                                protocol_clone.clone(),
                                state_clone.clone(),
                                server_id,
                                connection_id.as_u32(),
                                write_half_arc.clone(),
                                status_clone.clone(),
                            );

                            // If the server was started with send_first, give the handler a
                            // chance to greet before the client says anything. Without this
                            // Telnet has no connect-time event at all and cannot show a
                            // login banner or prompt.
                            if send_first {
                                let event = Event::new(
                                    &TELNET_CONNECTION_OPENED_EVENT,
                                    serde_json::json!({}),
                                );
                                match call_llm(
                                    &llm_clone,
                                    &state_clone,
                                    server_id,
                                    Some(connection_id),
                                    &event,
                                    protocol_clone.as_ref(),
                                )
                                .await
                                {
                                    Ok(execution_result) => {
                                        for protocol_result in execution_result.protocol_results {
                                            if let ActionResult::Output(data) = protocol_result {
                                                use tokio::io::AsyncWriteExt;
                                                let mut write = write_half_arc.lock().await;
                                                let _ = write.write_all(&data).await;
                                                let _ = write.flush().await;
                                                drop(write);
                                                state_clone
                                                    .update_connection_stats(
                                                        server_id,
                                                        connection_id,
                                                        None,
                                                        Some(data.len() as u64),
                                                        None,
                                                        Some(1),
                                                    )
                                                    .await;
                                                log.debug(format!(
                                                    "Telnet sent greeting ({} bytes) on connection {}",
                                                    data.len(),
                                                    connection_id
                                                ));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // The client asked for a banner and is sitting at a
                                        // blank screen. Telnet has no error frame - it is a
                                        // byte stream with a human on the other end - so the
                                        // protocol-appropriate answer is a plain notice line.
                                        // Non-fatal: a wire notice is still delivered
                                        // (fallback), so this is WARN not ERROR.
                                        log.warn(format!(
                                            "Telnet greeting handler failed on connection {}: {}",
                                            connection_id, e
                                        ));
                                        let notice = telnet_failure_notice(&e);
                                        use tokio::io::AsyncWriteExt;
                                        let mut write = write_half_arc.lock().await;
                                        let _ = write.write_all(notice.as_bytes()).await;
                                        let _ = write.flush().await;
                                    }
                                }
                            }

                            // Bounded, IAC-stripping line reading. Option negotiation is still
                            // not *answered* — sequences are recognised only well enough to be
                            // removed, so a real telnet client survives instead of being
                            // dropped when its negotiation fails to decode as UTF-8.
                            let mut reader = TelnetLineReader::new(read_half);
                            let mut close_requested = false;

                            loop {
                                let (line_bytes, n) = match reader.next_line().await {
                                    LineRead::Line(bytes, n) => (bytes, n),
                                    LineRead::Eof => break,
                                    LineRead::TooLong => {
                                        // No error frame exists in telnet, so say it in words
                                        // on their own line and hang up. A category, never an
                                        // error string: see `telnet_failure_notice`.
                                        log.warn(format!(
                                            "Telnet connection {} sent more than {} bytes with \
                                             no newline; closing",
                                            connection_id, MAX_LINE_BYTES
                                        ));
                                        use tokio::io::AsyncWriteExt;
                                        let mut write = write_half_arc.lock().await;
                                        let _ = write
                                            .write_all(b"\r\n[netget] line too long\r\n")
                                            .await;
                                        let _ = write.flush().await;
                                        let _ = write.shutdown().await;
                                        break;
                                    }
                                    LineRead::Failed(e) => {
                                        log.debug(format!(
                                            "Telnet read error on connection {}: {}",
                                            connection_id, e
                                        ));
                                        break;
                                    }
                                };
                                // Decoded lossily: after IAC stripping whatever is left is
                                // what the peer typed, and a stray non-UTF-8 byte must not
                                // cost them the connection.
                                let line = String::from_utf8_lossy(&line_bytes).into_owned();

                                // Counters and last_activity: the rail shows ↓/↑ per peer,
                                // and until this was added every telnet peer read 0/0.
                                state_clone
                                    .update_connection_stats(
                                        server_id,
                                        connection_id,
                                        Some(n as u64),
                                        None,
                                        Some(1),
                                        None,
                                    )
                                    .await;

                                // Summary + full payload are FileOnly: the
                                // telnet_message_received event template renders the
                                // equivalent line to the TUI, so streaming it here too
                                // would duplicate it and load the unbounded status channel.
                                let line_preview = preview(&line, 100);
                                log.debug(format!(
                                    "Telnet received {} bytes on connection {}: {}",
                                    n,
                                    connection_id,
                                    line_preview.trim()
                                ));
                                log.trace(format!("Telnet data (text): {:?}", line.trim()));

                                let event = Event::new(
                                    &TELNET_MESSAGE_RECEIVED_EVENT,
                                    serde_json::json!({
                                        "message": line.trim()
                                    }),
                                );

                                log.debug(format!(
                                    "Telnet calling LLM for connection {}",
                                    connection_id
                                ));

                                match call_llm(
                                    &llm_clone,
                                    &state_clone,
                                    server_id,
                                    Some(connection_id),
                                    &event,
                                    protocol_clone.as_ref(),
                                )
                                .await
                                {
                                    Ok(execution_result) => {
                                        for message in &execution_result.messages {
                                            log.info(message);
                                        }

                                        log.debug(format!(
                                            "Telnet got {} protocol results",
                                            execution_result.protocol_results.len()
                                        ));

                                        for protocol_result in execution_result.protocol_results {
                                            match protocol_result {
                                                ActionResult::Output(data) => {
                                                    let mut write = write_half_arc.lock().await;

                                                    // Write the action's bytes verbatim. Going
                                                    // via String::from_utf8_lossy would replace
                                                    // any non-UTF-8 byte with U+FFFD before it
                                                    // reached the wire.
                                                    use tokio::io::AsyncWriteExt;
                                                    let _ = write.write_all(&data).await;
                                                    let _ = write.flush().await;
                                                    drop(write);
                                                    state_clone
                                                        .update_connection_stats(
                                                            server_id,
                                                            connection_id,
                                                            None,
                                                            Some(data.len() as u64),
                                                            None,
                                                            Some(1),
                                                        )
                                                        .await;

                                                    let response = String::from_utf8_lossy(&data);

                                                    // Summary + full payload FileOnly: the
                                                    // send_telnet_* action template already
                                                    // reports the send to the TUI.
                                                    let response_preview = preview(&response, 100);
                                                    log.debug(format!(
                                                        "Telnet sent {} bytes on connection {}: {}",
                                                        data.len(),
                                                        connection_id,
                                                        response_preview.trim()
                                                    ));
                                                    log.trace(format!(
                                                        "Telnet sent (text): {:?}",
                                                        response.trim()
                                                    ));
                                                }
                                                // Only flags the intent: `break` here would
                                                // leave the read loop running and the socket
                                                // open, so close_connection did nothing.
                                                ActionResult::CloseConnection => {
                                                    close_requested = true;
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // Same reasoning as the greeting: a Telnet client that
                                        // sent a line and gets nothing back cannot tell a
                                        // broken server from a slow one. A notice is not a
                                        // prompt and not a shell result, so nothing downstream
                                        // can read it as the command having run.
                                        // Non-fatal: a wire notice is still delivered
                                        // (fallback), so this is WARN not ERROR.
                                        log.warn(format!(
                                            "Telnet LLM call failed on connection {}: {}",
                                            connection_id, e
                                        ));
                                        let notice = telnet_failure_notice(&e);
                                        use tokio::io::AsyncWriteExt;
                                        let mut write = write_half_arc.lock().await;
                                        let _ = write.write_all(notice.as_bytes()).await;
                                        let _ = write.flush().await;
                                    }
                                }

                                if close_requested {
                                    log.info(format!(
                                        "Telnet connection {} closed by handler",
                                        connection_id
                                    ));
                                    use tokio::io::AsyncWriteExt;
                                    let mut write = write_half_arc.lock().await;
                                    let _ = write.shutdown().await;
                                    break;
                                }
                            }

                            // Connection closed - mark as closed
                            state_clone
                                .remove_peer_handle(server_id, connection_id.as_u32())
                                .await;
                            state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept Telnet connection: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

#[cfg(not(feature = "telnet"))]
impl TelnetServer {
    pub async fn spawn_with_llm_actions(
        _listen_addr: SocketAddr,
        _llm_client: OllamaClient,
        _app_state: Arc<AppState>,
        _status_tx: mpsc::UnboundedSender<String>,
        _server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        anyhow::bail!("Telnet feature not enabled")
    }
}

/// The line to print when the LLM backend fails.
///
/// Telnet is an unstructured byte stream: there is no status code to send and no framing a
/// client could key off, so the only useful thing to do is tell whoever is on the other end,
/// in words, that the server could not answer - and to say so on its own line so it cannot be
/// mistaken for the output of whatever they typed.
///
/// The text is a category, never the error: see `crate::utils::wire_failure`. This is the
/// path that put netget's own retry message on a stranger's terminal.
///
/// CRLF because a raw Telnet client is usually in a mode where a bare LF does not return the
/// carriage, which would leave the message stair-stepping across the terminal.
#[cfg(feature = "telnet")]
fn telnet_failure_notice(err: &anyhow::Error) -> String {
    format!(
        "\r\n[netget] {}\r\n",
        crate::utils::WireFailure::classify(err).text()
    )
}
