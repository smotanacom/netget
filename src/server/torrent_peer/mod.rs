//! BitTorrent Peer Wire Protocol server implementation
//!
//! TCP-based protocol for peer-to-peer data transfer between BitTorrent clients

pub mod actions;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::error;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use actions::TorrentPeerProtocol;

/// What the connection loop should do after one event has been dispatched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Disposition {
    /// Keep reading from the peer.
    Continue,
    /// The peer has been refused and its write half half-closed; stop reading.
    Close,
}

/// `<len=0001><id=0>` — the peer wire protocol's choke message.
///
/// The only refusal the base protocol has. There is no error frame and no free-text field
/// anywhere in BEP 3, so a failure category can be expressed by *which* refusal is sent and
/// whether the connection survives it — never by text, which has nowhere to go.
const CHOKE_FRAME: [u8; 5] = [0x00, 0x00, 0x00, 0x01, 0x00];

/// BitTorrent Peer Wire Protocol server
pub struct TorrentPeerServer;

impl TorrentPeerServer {
    /// Spawn BitTorrent Peer server with LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "BitTorrent Peer server (action-based) listening on {}",
            local_addr
        ));

        let protocol = Arc::new(TorrentPeerProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        Log::new(Some(&status_clone)).debug(format!(
                            "BitTorrent Peer accepted connection from {}",
                            peer_addr
                        ));

                        // Split stream for read/write
                        let (read_half, write_half) = tokio::io::split(stream);
                        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
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
                        };
                        state_clone
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_clone.send("__UPDATE_UI__".to_string());

                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                read_half,
                                write_half,
                                peer_addr,
                                local_addr,
                                connection_id,
                                llm_clone,
                                state_clone,
                                status_clone,
                                server_id,
                                protocol_clone,
                            )
                            .await
                            {
                                error!("BitTorrent Peer connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("BitTorrent Peer accept error: {}", e));
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    async fn handle_connection(
        mut read_half: tokio::io::ReadHalf<tokio::net::TcpStream>,
        write_half: Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
        peer_addr: SocketAddr,
        _local_addr: SocketAddr,
        connection_id: ConnectionId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        protocol: Arc<TorrentPeerProtocol>,
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;

        // Register the peer handle so the dashboard can "message this peer" /
        // "disconnect this peer" through the same write half the reader uses. All
        // wire verbs return `ActionResult::Output`, so the generic peer command
        // task covers the full vocabulary with nothing protocol-specific. The
        // handle is removed on every exit path via the single cleanup below.
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
            write_half.clone(),
            status_tx.clone(),
        );

        let result: Result<()> = async {
            let mut chunk = vec![0u8; 16384];
            // The peer wire protocol is length-prefixed over a byte stream, so a `read()` is
            // not a message. Previously each read was parsed as exactly one frame, which meant
            // the bitfield a client sends in the same segment as its handshake was discarded,
            // two coalesced messages became one, and a message split across two reads was
            // dropped and left the stream misaligned for everything after it. Accumulate and
            // drain complete frames instead.
            let mut pending: Vec<u8> = Vec::new();
            let mut handshake_complete = false;

            'read: loop {
                let n = read_half.read(&mut chunk).await?;
                if n == 0 {
                    Log::new(Some(&status_tx)).debug("BitTorrent Peer connection closed by peer");
                    break;
                }

                // Refresh inbound counters (and last_activity) so the rail shows ↓ move.
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        Some(n as u64),
                        None,
                        Some(1),
                        None,
                    )
                    .await;

                Log::new(Some(&status_tx)).debug(format!(
                    "BitTorrent Peer received {} bytes from {}",
                    n, peer_addr
                ));

                // TRACE: Log full payload
                Log::new(Some(&status_tx)).trace(format!(
                    "BitTorrent Peer data (hex): {}",
                    hex::encode(&chunk[..n])
                ));

                pending.extend_from_slice(&chunk[..n]);

                // A peer that never completes a frame would otherwise grow this buffer without
                // bound. The largest legitimate frame is a piece message, capped well under
                // this by every client in use.
                const MAX_PENDING: usize = 2 * 1024 * 1024;
                if pending.len() > MAX_PENDING {
                    Log::new(Some(&status_tx)).warn(format!(
                        "BitTorrent Peer {} buffered {} bytes without a complete message, closing",
                        peer_addr,
                        pending.len()
                    ));
                    break;
                }

                loop {
                    if !handshake_complete {
                        if pending.len() < 68 {
                            break;
                        }
                        let handshake: Vec<u8> = pending.drain(..68).collect();
                        match Self::parse_handshake(&handshake) {
                            Ok((info_hash, peer_id, peer_id_hex)) => {
                                Log::new(Some(&status_tx)).debug(format!(
                                    "BitTorrent Peer handshake: info_hash={}, peer_id={}",
                                    info_hash, peer_id
                                ));

                                handshake_complete = true;

                                let event = Event::new(
                                    &actions::PEER_HANDSHAKE_EVENT,
                                    serde_json::json!({
                                        "info_hash": info_hash,
                                        "peer_id": peer_id,
                                        "peer_id_hex": peer_id_hex,
                                    }),
                                );

                                let disposition = Self::dispatch_event(
                                    event,
                                    &write_half,
                                    peer_addr,
                                    connection_id,
                                    &llm_client,
                                    &app_state,
                                    &status_tx,
                                    server_id,
                                    &protocol,
                                )
                                .await?;
                                if disposition == Disposition::Close {
                                    break 'read;
                                }
                            }
                            Err(e) => {
                                Log::new(Some(&status_tx))
                                    .error(format!("Failed to parse handshake: {}", e));
                                break 'read;
                            }
                        }
                        continue;
                    }

                    // Keep-alive and regular messages are both `<4-byte length><body>`.
                    if pending.len() < 4 {
                        break;
                    }
                    let length =
                        u32::from_be_bytes([pending[0], pending[1], pending[2], pending[3]])
                            as usize;
                    if pending.len() < 4 + length {
                        break;
                    }
                    let frame: Vec<u8> = pending.drain(..4 + length).collect();

                    match Self::parse_message(&frame) {
                        Ok((message_type, message_data)) => {
                            Log::new(Some(&status_tx))
                                .debug(format!("BitTorrent Peer message type: {}", message_type));

                            let event_type = match message_type.as_str() {
                                // One event covers the four payload-free state messages; they
                                // share a reply vocabulary and carry `message_type` so a
                                // handler can still tell them apart.
                                "choke" | "unchoke" | "interested" | "not_interested" => {
                                    &actions::PEER_CHOKE_MESSAGE_EVENT
                                }
                                "request" => &actions::PEER_REQUEST_MESSAGE_EVENT,
                                "bitfield" => &actions::PEER_BITFIELD_MESSAGE_EVENT,
                                // have / piece / cancel / keepalive / unrecognised ids used to
                                // be announced to the model as "peer_choke_message", which is
                                // simply false.
                                _ => &actions::PEER_MESSAGE_EVENT,
                            };
                            let event = Event::new(event_type, message_data);

                            Log::new(Some(&status_tx)).debug(format!(
                                "BitTorrent Peer calling LLM for {} message",
                                message_type
                            ));

                            let disposition = Self::dispatch_event(
                                event,
                                &write_half,
                                peer_addr,
                                connection_id,
                                &llm_client,
                                &app_state,
                                &status_tx,
                                server_id,
                                &protocol,
                            )
                            .await?;
                            if disposition == Disposition::Close {
                                break 'read;
                            }
                        }
                        Err(e) => {
                            Log::new(Some(&status_tx))
                                .warn(format!("Failed to parse peer message: {}", e));
                        }
                    }
                }
            }

            Ok(())
        }
        .await;

        // Every exit path (EOF, read error, handshake/parse abort, buffer cap) lands
        // here: drop the peer handle so the dashboard stops offering to message or
        // disconnect a dead connection.
        app_state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        result
    }

    /// Call the LLM for one event and write whatever it produced back to the peer.
    ///
    /// On a backend failure the peer is *answered*, not left hanging. BitTorrent has no error
    /// message, so the answer is a `choke` — the protocol's own "I will not serve you right
    /// now" — and the two failure categories are told apart by what follows it:
    ///
    /// * [`WireFailure::Overloaded`] — choke alone. The connection stays up, so a peer that
    ///   honours choke simply stops requesting and waits for an `unchoke`; nothing is
    ///   recorded as a permanent fault and a retry costs it no new handshake.
    /// * [`WireFailure::Unavailable`] — choke, then half-close the write half so the peer's
    ///   next read returns EOF and it moves on to another peer.
    ///
    /// Nothing derived from the error reaches the socket; a choke frame is five fixed bytes
    /// and has nowhere to put text even if it were wanted. The error goes to the log and the
    /// status stream, tagged `decision=fail_closed_llm_error`.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_event(
        event: Event,
        write_half: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        protocol: &Arc<TorrentPeerProtocol>,
    ) -> Result<Disposition> {
        match call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                Self::process_llm_response(
                    execution_result,
                    write_half,
                    peer_addr,
                    connection_id,
                    app_state,
                    server_id,
                    status_tx,
                )
                .await?;
                Ok(Disposition::Continue)
            }
            Err(e) => {
                use tokio::io::AsyncWriteExt;
                let failure = WireFailure::classify(&e);
                let log = Log::new(Some(&status_tx));
                // The full error belongs here and only here.
                log.error(format!(
                    "BitTorrent Peer {} conn={} decision=fail_closed_llm_error category={} LLM error: {}",
                    peer_addr,
                    connection_id.as_u32(),
                    if failure.is_overloaded() { "overloaded" } else { "unavailable" },
                    e
                ));

                let write_result = {
                    let mut write = write_half.lock().await;
                    let sent = write.write_all(&CHOKE_FRAME).await;
                    if sent.is_ok() && !failure.is_overloaded() {
                        // No text to send and nothing more to say: FIN is the honest signal.
                        let _ = write.shutdown().await;
                    }
                    sent
                };

                match write_result {
                    Ok(()) => {
                        app_state
                            .update_connection_stats(
                                server_id,
                                connection_id,
                                None,
                                Some(CHOKE_FRAME.len() as u64),
                                None,
                                Some(1),
                            )
                            .await;
                    }
                    Err(write_err) => {
                        log.warn(format!(
                            "BitTorrent Peer could not send choke to {}: {}",
                            peer_addr, write_err
                        ));
                        return Ok(Disposition::Close);
                    }
                }

                if failure.is_overloaded() {
                    log.warn(format!(
                        "BitTorrent Peer choked {} (transient); connection kept open",
                        peer_addr
                    ));
                    Ok(Disposition::Continue)
                } else {
                    log.info(format!(
                        "BitTorrent Peer choked and half-closed {} after LLM error",
                        peer_addr
                    ));
                    Ok(Disposition::Close)
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_llm_response(
        execution_result: crate::llm::actions::executor::ExecutionResult,
        write_half: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        app_state: &Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        // Display messages from LLM
        for message in &execution_result.messages {
            Log::new(Some(&status_tx)).info(format!("{}", message));
        }

        Log::new(Some(&status_tx)).debug(format!(
            "BitTorrent Peer got {} protocol results",
            execution_result.protocol_results.len()
        ));

        // Three outcomes must stay apart in the log: the model refused, the model said
        // nothing, and the backend failed (tagged `decision=fail_closed_llm_error` in
        // `dispatch_event`). Silence here is not a refusal and is not a grant — the peer
        // simply gets no frame — so it must not be recorded as either.
        let model_refused = execution_result
            .raw_actions
            .iter()
            .any(|a| a.get("type").and_then(|t| t.as_str()) == Some("send_choke"));
        let decision = if model_refused {
            "model_reject"
        } else if execution_result.protocol_results.is_empty() {
            "model_no_action"
        } else {
            "model_answer"
        };
        Log::new(Some(&status_tx)).debug(format!(
            "BitTorrent Peer {} conn={} decision={}",
            peer_addr,
            connection_id.as_u32(),
            decision
        ));

        // Send responses. Every output is written, in order: a handshake reply is
        // routinely followed by a bitfield and an unchoke, and dropping all but the first
        // would leave the peer waiting.
        for protocol_result in execution_result.protocol_results {
            for output_data in protocol_result.get_all_output() {
                let mut write = write_half.lock().await;
                write.write_all(&output_data).await?;
                drop(write);

                // Refresh outbound counters (and last_activity) so the rail shows ↑ move.
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        None,
                        Some(output_data.len() as u64),
                        None,
                        Some(1),
                    )
                    .await;

                Log::new(Some(&status_tx)).debug(format!(
                    "BitTorrent Peer sent {} bytes to {}",
                    output_data.len(),
                    peer_addr
                ));

                // TRACE: Log full response
                let hex_str = hex::encode(&output_data);
                Log::new(Some(&status_tx))
                    .trace(format!("BitTorrent Peer sent (hex): {}", hex_str));
            }
        }

        Ok(())
    }

    /// Parse the fixed 68-byte handshake.
    ///
    /// Returns `(info_hash_hex, peer_id_text, peer_id_hex)`. The peer ID is 20 arbitrary
    /// bytes: most clients use printable ASCII but the trailing random section frequently
    /// is not, so the lossy text form can contain replacement characters and must not be
    /// echoed back. `peer_id_hex` is the faithful form.
    fn parse_handshake(data: &[u8]) -> Result<(String, String, String)> {
        // Handshake format: <pstrlen><pstr><reserved><info_hash><peer_id>
        // pstrlen = 19, pstr = "BitTorrent protocol"

        if data.len() < 68 {
            return Err(anyhow::anyhow!("Handshake too short"));
        }

        let pstrlen = data[0] as usize;
        if pstrlen != 19 {
            return Err(anyhow::anyhow!("Invalid pstrlen"));
        }

        let pstr = &data[1..20];
        if pstr != b"BitTorrent protocol" {
            return Err(anyhow::anyhow!("Invalid protocol string"));
        }

        // reserved = 8 bytes (bytes 20-27)
        let info_hash = hex::encode(&data[28..48]);
        let peer_id = String::from_utf8_lossy(&data[48..68]).to_string();
        let peer_id_hex = hex::encode(&data[48..68]);

        Ok((info_hash, peer_id, peer_id_hex))
    }

    fn parse_message(data: &[u8]) -> Result<(String, serde_json::Value)> {
        if data.is_empty() {
            return Ok((
                "keepalive".to_string(),
                serde_json::json!({"message_type": "keepalive"}),
            ));
        }

        if data.len() < 4 {
            return Err(anyhow::anyhow!("Message too short"));
        }

        let length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;

        if length == 0 {
            return Ok((
                "keepalive".to_string(),
                serde_json::json!({"message_type": "keepalive"}),
            ));
        }

        if data.len() < 4 + length {
            return Err(anyhow::anyhow!("Incomplete message"));
        }

        let message_id = data[4];
        let payload = &data[5..4 + length];

        let (message_type, message_data) = match message_id {
            0 => ("choke", serde_json::json!({})),
            1 => ("unchoke", serde_json::json!({})),
            2 => ("interested", serde_json::json!({})),
            3 => ("not_interested", serde_json::json!({})),
            4 => {
                if payload.len() >= 4 {
                    let piece_index =
                        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    ("have", serde_json::json!({"piece_index": piece_index}))
                } else {
                    ("have", serde_json::json!({}))
                }
            }
            5 => {
                // Bitfield message
                (
                    "bitfield",
                    serde_json::json!({"bitfield": hex::encode(payload)}),
                )
            }
            6 => {
                if payload.len() >= 12 {
                    let index =
                        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let begin =
                        u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    let length =
                        u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
                    (
                        "request",
                        serde_json::json!({"index": index, "begin": begin, "length": length}),
                    )
                } else {
                    ("request", serde_json::json!({}))
                }
            }
            7 => {
                if payload.len() >= 8 {
                    let index =
                        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let begin =
                        u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    let block = hex::encode(&payload[8..]);
                    (
                        "piece",
                        serde_json::json!({"index": index, "begin": begin, "block_hex": block}),
                    )
                } else {
                    ("piece", serde_json::json!({}))
                }
            }
            8 => {
                if payload.len() >= 12 {
                    let index =
                        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let begin =
                        u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    let length =
                        u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
                    (
                        "cancel",
                        serde_json::json!({"index": index, "begin": begin, "length": length}),
                    )
                } else {
                    ("cancel", serde_json::json!({}))
                }
            }
            _ => (
                "unknown",
                serde_json::json!({"id": message_id, "payload_hex": hex::encode(payload)}),
            ),
        };

        // Several message types share one event, so the event payload has to say which
        // one actually arrived; without it a handler on peer_choke_message cannot tell a
        // choke from an "interested".
        let mut message_data = message_data;
        if let Some(obj) = message_data.as_object_mut() {
            obj.insert("message_type".to_string(), serde_json::json!(message_type));
        }

        Ok((message_type.to_string(), message_data))
    }
}
