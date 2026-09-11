//! STUN server implementation
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::{ActionResult, Server};
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::StunProtocol;
use crate::state::app_state::AppState;
use crate::{console_debug, console_trace};
use actions::STUN_BINDING_REQUEST_EVENT;

/// STUN server that handles binding requests
pub struct StunServer;

impl StunServer {
    /// Spawn STUN server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        let log = Log::new(Some(&status_tx));
        log.info(format!(
            "STUN server (action-based) listening on {}",
            local_addr
        ));
        log.debug(format!(
            "STUN socket bound - requested: {}, actual: {}",
            listen_addr, local_addr
        ));

        let protocol = Arc::new(StunProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 2048]; // STUN messages are typically < 2KB

            Log::new(Some(&status_tx)).debug("STUN receive loop started, waiting for packets...");

            loop {
                debug!("STUN calling recv_from...");
                Log::new(Some(&status_tx)).trace("STUN about to call recv_from (iteration)");
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        Log::new(Some(&status_tx)).trace(format!(
                            "STUN recv_from returned OK: {} bytes from {}",
                            n, peer_addr
                        ));
                        let data = buffer[..n].to_vec();
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Add connection to ServerInstance (STUN "connection" = transaction)
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
                            bytes_received: n as u64,
                            packets_sent: 0,
                            packets_received: 1,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        // DEBUG: Log summary
                        console_debug!(status_tx, "STUN received {} bytes from {}", n, peer_addr);

                        // TRACE: Log full payload
                        let hex_str = hex::encode(&data);
                        console_trace!(status_tx, "STUN data (hex): {}", hex_str);

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            // Parse STUN message to extract transaction ID and message type
                            let (transaction_id, message_type, is_valid) =
                                Self::parse_stun_header(&data);

                            if !is_valid {
                                Log::new(Some(&status_clone)).debug(format!(
                                    "STUN dropped an unservable {} message from {} — only \
                                     Binding requests are answered (see parse_stun_header)",
                                    message_type, peer_addr
                                ));
                                return;
                            }

                            let transaction_id_hex =
                                transaction_id.as_ref().map(hex::encode).unwrap_or_default();

                            // Create STUN binding request event
                            let event_data = serde_json::json!({
                                "peer_addr": peer_addr.to_string(),
                                "local_addr": local_addr.to_string(),
                                "transaction_id": transaction_id_hex,
                                "message_type": message_type,
                                "bytes_received": data.len()
                            });

                            let event = Event::new(&STUN_BINDING_REQUEST_EVENT, event_data);

                            // A STUN Binding response is 100% mechanical: reflect the source
                            // address into XOR-MAPPED-ADDRESS and echo the 96-bit transaction ID.
                            // There is nothing to decide, so by default we answer statically with
                            // NO LLM round-trip. The model is consulted only when the operator
                            // opts in — a server instruction or a per-event handler — which is
                            // how one asks for non-standard behaviour (e.g. lying about the
                            // mapped address for NAT testing).
                            let wants_dynamic = operator_wants_dynamic(
                                &state_clone,
                                server_id,
                                &event.event_type.id,
                            )
                            .await;

                            if !wants_dynamic {
                                send_static_binding_response(
                                    protocol_clone.as_ref(),
                                    socket_clone.as_ref(),
                                    peer_addr,
                                    &transaction_id_hex,
                                    &status_clone,
                                )
                                .await;
                                return;
                            }

                            Log::new(Some(&status_clone)).debug(format!(
                                "STUN calling LLM for binding request from {}",
                                peer_addr
                            ));

                            match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None, // STUN uses UDP, no persistent connection
                                &event,
                                protocol_clone.as_ref(),
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    let log = Log::new(Some(&status_clone));

                                    // Display messages from LLM
                                    for message in &execution_result.messages {
                                        log.info(message);
                                    }

                                    log.debug(format!(
                                        "STUN parsed {} actions",
                                        execution_result.raw_actions.len()
                                    ));

                                    // Process protocol results
                                    log.debug(format!(
                                        "STUN got {} protocol results",
                                        execution_result.protocol_results.len()
                                    ));

                                    for protocol_result in execution_result.protocol_results {
                                        if let Some(output_data) =
                                            protocol_result.get_all_output().first()
                                        {
                                            let _ =
                                                socket_clone.send_to(output_data, peer_addr).await;

                                            // DEBUG: Log summary
                                            log.debug(format!(
                                                "STUN sent {} bytes to {}",
                                                output_data.len(),
                                                peer_addr
                                            ));

                                            // TRACE: Log full payload
                                            let hex_str = hex::encode(output_data);
                                            log.trace(format!("STUN sent (hex): {}", hex_str));

                                            let _ = status_clone.send(format!(
                                                "→ STUN response to {} ({} bytes)",
                                                peer_addr,
                                                output_data.len()
                                            ));
                                        } else {
                                            log.debug("STUN protocol result has no output data");
                                        }
                                    }
                                }
                                Err(e) => {
                                    // Fail closed to the correct static default, not to a
                                    // permissive or misleading answer. The operator opted into
                                    // LLM control and the LLM failed; the mechanical response
                                    // (the client's real reflected address) is the safe fallback
                                    // here — a STUN Binding response is never a credential or an
                                    // approval — so send it and say so, rather than dropping the
                                    // request or inventing an address.
                                    error!(
                                        "STUN LLM call failed for request from {} ({}): {} — falling back to static binding response",
                                        peer_addr, connection_id, e
                                    );
                                    let _ = status_clone.send(format!(
                                        "✗ STUN LLM error: {} — sending static binding response",
                                        e
                                    ));
                                    send_static_binding_response(
                                        protocol_clone.as_ref(),
                                        socket_clone.as_ref(),
                                        peer_addr,
                                        &transaction_id_hex,
                                        &status_clone,
                                    )
                                    .await;
                                }
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("STUN receive error: {}", e));
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

    /// Parse STUN message header to extract transaction ID and message type.
    ///
    /// Returns `(transaction_id, message_type_string, servable)`. `servable` is
    /// **not** "this parsed" — it is "this server may answer it". Everything
    /// else is dropped without a byte leaving the socket, because on UDP a reply
    /// goes to whatever source address the datagram claimed.
    fn parse_stun_header(data: &[u8]) -> (Option<Vec<u8>>, String, bool) {
        // STUN message header is 20 bytes minimum
        if data.len() < 20 {
            return (None, "invalid".to_string(), false);
        }

        // RFC 8489 section 5: the two most significant bits of a STUN message
        // type are always zero. Anything else is not STUN — 0b01 there is a TURN
        // ChannelData frame — and must not be answered.
        if u16::from_be_bytes([data[0], data[1]]) & 0xC000 != 0 {
            return (None, "invalid".to_string(), false);
        }

        // Check magic cookie (bytes 4-7 should be 0x2112A442)
        let magic_cookie = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if magic_cookie != 0x2112A442 {
            return (None, "invalid".to_string(), false);
        }

        // RFC 8489 section 5: the message length counts the attributes only and
        // is always a multiple of 4. A header whose declared length overruns the
        // datagram is malformed, and answering it would mean answering a source
        // address on the strength of a header we already know is lying.
        let declared_length = u16::from_be_bytes([data[2], data[3]]) as usize;
        if declared_length % 4 != 0 || 20usize.saturating_add(declared_length) > data.len() {
            return (None, "invalid".to_string(), false);
        }

        // Extract message type (first 2 bytes)
        let message_type_raw = u16::from_be_bytes([data[0], data[1]]);

        // RFC 8489 section 5: the 14-bit type interleaves method and class bits as
        //   0b00 M11 M10 M9 M8 M7 C1 M6 M5 M4 C0 M3 M2 M1 M0
        // so C0 is bit 4 and C1 is bit 8, and class == C1<<1 | C0.
        //
        // The previous expression, ((raw & 0x0110) >> 4) | ((raw & 0x0100) >> 7),
        // works out to C0 + 18*C1, which happens to be right only when C1 is 0
        // (requests and indications). Success and error responses decoded to 18
        // and 19, so their arms below were unreachable and every response was
        // labelled "Unknown".
        let c0 = (message_type_raw >> 4) & 0x1;
        let c1 = (message_type_raw >> 8) & 0x1;
        let class = (c1 << 1) | c0;
        let method = (message_type_raw & 0x000F)
            | ((message_type_raw & 0x00E0) >> 1)
            | ((message_type_raw & 0x3E00) >> 2);

        // Class values: 0 = request, 1 = indication, 2 = success response,
        // 3 = error response.
        let message_type = match (class, method) {
            (0, 1) => "BindingRequest",
            (1, 1) => "BindingIndication",
            (2, 1) => "BindingResponse",
            (3, 1) => "BindingError",
            _ => "Unknown",
        };

        // Extract transaction ID (12 bytes, from byte 8 to 19)
        let transaction_id = data[8..20].to_vec();

        // Only a Binding *Request* is served. That is RFC 8489 — section 6.3.2
        // discards an indication without replying, and a server never receives a
        // response for a transaction it did not start — and it is also what keeps
        // this server from being a reflector.
        //
        // Until now any message carrying the magic cookie was answered, a Binding
        // *Response* included. One spoofed datagram naming a second NetGet STUN
        // server as its source therefore set the two answering each other's
        // answers, with nothing in either to stop it. Aimed at any other UDP
        // listener the same spoof makes this a ~2.4x amplifier (a 20-byte request
        // produces a 48-byte reply) for as long as the attacker keeps sending.
        // Dropping every class but Request costs nothing a real client does.
        let servable = class == 0 && method == 1;

        (Some(transaction_id), message_type.to_string(), servable)
    }
}

/// Returns true if the operator opted into dynamic (LLM- or handler-driven) responses for
/// this server: either a non-empty server instruction was given, or an event handler is
/// configured for `event_id`. When false the protocol answers with a correct static default
/// and never consults the model — a STUN Binding response is fully determined by the request.
async fn operator_wants_dynamic(
    state: &AppState,
    server_id: crate::state::ServerId,
    event_id: &str,
) -> bool {
    state
        .with_server_mut(server_id, |server| {
            let has_instruction = !server.instruction.trim().is_empty();
            let has_handler = server
                .event_handler_config
                .as_ref()
                .map(|c| c.find_handler(event_id).is_some())
                .unwrap_or(false);
            has_instruction || has_handler
        })
        .await
        .unwrap_or(false)
}

/// Build and send the mechanical STUN Binding Success Response with no LLM involvement:
/// XOR-MAPPED-ADDRESS = the client's own source address, transaction ID echoed verbatim.
async fn send_static_binding_response(
    protocol: &StunProtocol,
    socket: &UdpSocket,
    peer_addr: SocketAddr,
    transaction_id_hex: &str,
    status: &mpsc::UnboundedSender<String>,
) {
    let action = serde_json::json!({
        "type": "send_stun_binding_response",
        "transaction_id": transaction_id_hex,
        "mapped_address": peer_addr.to_string(),
        "xor_mapped_address": true
    });
    match protocol.execute_action(action) {
        Ok(ActionResult::Output(bytes)) => {
            let _ = socket.send_to(&bytes, peer_addr).await;
            debug!(
                "STUN sent static binding response ({} bytes) to {}",
                bytes.len(),
                peer_addr
            );
            let _ = status.send(format!(
                "→ STUN static binding response to {} ({} bytes)",
                peer_addr,
                bytes.len()
            ));
        }
        Ok(_) => {
            warn!(
                "STUN static binding response produced no output for {}",
                peer_addr
            );
        }
        Err(e) => {
            error!(
                "STUN failed to build static binding response for {}: {}",
                peer_addr, e
            );
            let _ = status.send(format!("✗ STUN static binding response failed: {e}"));
        }
    }
}
