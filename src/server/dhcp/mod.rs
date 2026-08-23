//! DHCP server implementation
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::DhcpProtocol;
use crate::state::app_state::AppState;
use actions::DHCP_REQUEST_EVENT;

use actions::DhcpRequestContext;
use dhcproto::{v4, Decodable, Decoder};

/// Render a hardware address the way the `dhcp_request` event reports it: lower-case hex
/// with no separators, e.g. `001122334455`.
///
/// This matches the format the BOOTP sibling and the E2E suites expect. The reply actions
/// accept either this or the colon-separated form when `client_mac` is passed explicitly.
pub fn format_mac(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// DHCP server that forwards requests to LLM
pub struct DhcpServer;

impl DhcpServer {
    /// Spawn DHCP server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("DHCP server listening on {}", local_addr));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 1500];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // DEBUG: Log summary (FileOnly, hot per-packet path)
                        Log::new(Some(&status_tx))
                            .debug(format!("DHCP received {} bytes from {}", n, peer_addr));

                        // TRACE: Log full payload (always hex for DHCP, FileOnly)
                        let hex_str = hex::encode(&data);
                        Log::new(Some(&status_tx)).trace(format!("DHCP data (hex): {}", hex_str));

                        // A datagram that does not decode - or decodes without a DHCP message
                        // type option - is not a DHCP request. Raising `dhcp_request` for it
                        // spent an LLM round trip and handed the model an event reading
                        // "unknown" in every field, out of which no reply could be built:
                        // `base_reply` has no transaction id to echo and errors. Drop it.
                        let Some((_, Some(request_ctx))) = Self::parse_dhcp_message(&data) else {
                            Log::new(Some(&status_tx)).warn(format!(
                                "Dropping non-DHCP datagram ({} bytes) from {}",
                                n, peer_addr
                            ));
                            continue;
                        };

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

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();

                        tokio::spawn(async move {
                            // One protocol instance per request. The instance carries the
                            // request context (xid, chaddr, giaddr, broadcast flag) used to
                            // build the reply, so two clients whose LLM calls overlap can
                            // never echo each other's transaction ID.
                            let protocol = DhcpProtocol::new();
                            protocol.set_request_context(request_ctx.clone());

                            let message_type = format!("{:?}", request_ctx.message_type);
                            let message_type_for_log = message_type.clone();
                            let client_mac = crate::server::dhcp::format_mac(&request_ctx.chaddr);
                            let requested_ip = request_ctx.requested_ip.map(|ip| ip.to_string());
                            let xid = Some(request_ctx.xid);
                            let client_ip = request_ctx.ciaddr.to_string();
                            let gateway_ip = request_ctx.giaddr.to_string();

                            let mut event_data = serde_json::json!({
                                "message_type": message_type,
                                "client_mac": client_mac,
                                "xid": xid,
                                "client_ip": client_ip,
                                "gateway_ip": gateway_ip
                            });
                            if let Some(ip) = requested_ip {
                                event_data["requested_ip"] = serde_json::json!(ip);
                            }

                            let event = Event::new(&DHCP_REQUEST_EVENT, event_data);

                            Log::new(Some(&status_clone))
                                .debug(format!("DHCP calling LLM for request from {}", peer_addr));

                            // ── Why a failed request is answered with NOTHING ──────────────
                            //
                            // DHCP has no "I cannot answer right now" message. The only reply
                            // that could be synthesised here without the model is a DHCPNAK,
                            // and a NAK is not a failure code: RFC 2131 4.3.2 defines it as
                            // "your notion of the network address is incorrect". Sending it
                            // because *netget's* backend is down tells the client something
                            // false about its own lease, and a client in RENEWING/REBINDING
                            // that receives one MUST drop the address and restart from INIT
                            // (4.4.5) — a valid lease destroyed by an internal error. The same
                            // section requires a server with no knowledge of a binding to
                            // "remain silent", which is exactly netget's position when the
                            // oracle that would have decided is unavailable.
                            //
                            // Silence is also the retry path the protocol already specifies:
                            // clients retransmit DISCOVER/REQUEST with backoff (4.4.1), so an
                            // Overloaded backend recovers on the client's own retry, which is
                            // what a 503 buys in HTTP. Answering would be the worse behaviour
                            // here, not the better one.
                            //
                            // So the three no-reply cases are separated in the LOG instead,
                            // with a stable `decision=` token (the radius convention): an
                            // operator greps `decision=fail_closed_` and finds every request
                            // netget dropped on the floor, distinct from the ones the model
                            // deliberately chose not to answer. The error itself goes to the
                            // log and the status stream only — never to the wire, which for
                            // this protocol carries no bytes at all on these paths.
                            let log = Log::new(Some(&status_clone));

                            let decision = match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None,
                                &event,
                                &protocol,
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    for message in &execution_result.messages {
                                        log.info(format!("{}", message));
                                    }

                                    log.debug(format!(
                                        "DHCP got {} protocol results",
                                        execution_result.protocol_results.len()
                                    ));

                                    let had_actions = !execution_result.raw_actions.is_empty();
                                    let mut sent = 0usize;

                                    for protocol_result in execution_result.protocol_results {
                                        if let Some(output_data) =
                                            protocol_result.get_all_output().first()
                                        {
                                            let _ =
                                                socket_clone.send_to(output_data, peer_addr).await;
                                            sent += 1;

                                            // DEBUG: Log summary (FileOnly, hot per-packet path)
                                            log.debug(format!(
                                                "DHCP sent {} bytes to {}",
                                                output_data.len(),
                                                peer_addr
                                            ));

                                            // TRACE: Log full payload (FileOnly)
                                            let hex_str = hex::encode(output_data);
                                            log.trace(format!("DHCP sent (hex): {}", hex_str));

                                            log.info(format!(
                                                "DHCP response to {} ({} bytes)",
                                                peer_addr,
                                                output_data.len()
                                            ));
                                        } else {
                                            log.debug("DHCP protocol result has no output data");
                                        }
                                    }

                                    if sent > 0 {
                                        "model_reply"
                                    } else if had_actions {
                                        // The model answered, and its answer produced no
                                        // packet — `ignore_request`, or actions that write
                                        // nothing. Deliberate silence, not a failure.
                                        "model_no_reply"
                                    } else {
                                        // The model produced no action at all. Nothing on the
                                        // wire, same as an error, but a different cause.
                                        "fail_closed_no_action"
                                    }
                                }
                                Err(e) => {
                                    // DHCP cannot express "retry later" on the wire, so an
                                    // overloaded backend and a dead one look identical to the
                                    // client. Keep the distinction where it is actionable —
                                    // next to the decision token, in the log.
                                    let category =
                                        match crate::utils::wire_failure::WireFailure::classify(&e)
                                        {
                                            crate::utils::wire_failure::WireFailure::Overloaded => {
                                                "overloaded"
                                            }
                                            crate::utils::wire_failure::WireFailure::Unavailable => {
                                                "unavailable"
                                            }
                                        };
                                    log.error(format!(
                                        "DHCP LLM call failed for {} (category={}): {}",
                                        peer_addr, category, e
                                    ));
                                    "fail_closed_llm_error"
                                }
                            };

                            log.info(format!(
                                "DHCP {} from {} decision={}",
                                message_type_for_log, peer_addr, decision
                            ));

                            if decision.starts_with("fail_closed_") {
                                // RFC 2131 4.3.2: a server that cannot decide remains silent
                                // and "MAY output a warning to the network administrator".
                                // This is that warning — the client will retransmit.
                                log.warn(format!(
                                    "DHCP sent no reply to {} (no usable decision was produced; \
                                     the client will retransmit)",
                                    peer_addr
                                ));
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("DHCP receive error: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the recv loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Decode a datagram into `(description, request context)`.
    ///
    /// `None` means the datagram is not usable as a DHCP request: it did not decode, its
    /// `hlen` is out of range, or it carries no message-type option. The caller drops those.
    fn parse_dhcp_message(data: &[u8]) -> Option<(String, Option<DhcpRequestContext>)> {
        use std::net::Ipv4Addr;

        match v4::Message::decode(&mut Decoder::new(data)) {
            Ok(msg) => {
                // `hlen` is read straight off the wire without validation, and
                // `Message::chaddr()` slices a fixed [u8; 16] with it - a datagram
                // declaring hlen > 16 panics inside dhcproto. This runs in the socket
                // task, so that panic would take the whole server down. Reject instead.
                if msg.hlen() as usize > 16 {
                    tracing::warn!(
                        "Dropping DHCP datagram with invalid hlen {} (max 16)",
                        msg.hlen()
                    );
                    return None;
                }

                // Extract message type from options
                let message_type = msg.opts().get(v4::OptionCode::MessageType).and_then(|opt| {
                    if let v4::DhcpOption::MessageType(mt) = opt {
                        Some(*mt)
                    } else {
                        None
                    }
                });

                let message_type_str = message_type
                    .as_ref()
                    .map(|mt| format!("{:?}", mt))
                    .unwrap_or_else(|| "Unknown".to_string());

                // Extract requested IP from options if present
                let requested_ip =
                    msg.opts()
                        .get(v4::OptionCode::RequestedIpAddress)
                        .and_then(|opt| {
                            if let v4::DhcpOption::RequestedIpAddress(ip) = opt {
                                Some(*ip)
                            } else {
                                None
                            }
                        });

                // Build human-readable description
                let mac_str = hex::encode(msg.chaddr());
                let mut description = format!(
                    "DHCP {} from client MAC {} (transaction ID: 0x{:08x})",
                    message_type_str,
                    mac_str,
                    msg.xid()
                );

                if msg.ciaddr() != Ipv4Addr::UNSPECIFIED {
                    description.push_str(&format!(", client IP: {}", msg.ciaddr()));
                }

                if let Some(req_ip) = requested_ip {
                    description.push_str(&format!(", requested IP: {}", req_ip));
                }

                // Create context for action execution
                let context = message_type.map(|mt| DhcpRequestContext {
                    xid: msg.xid(),
                    chaddr: msg.chaddr().to_vec(),
                    message_type: mt,
                    ciaddr: msg.ciaddr(),
                    giaddr: msg.giaddr(),
                    broadcast: msg.flags().broadcast(),
                    requested_ip,
                });

                Some((description, context))
            }
            Err(e) => {
                tracing::warn!("Failed to parse DHCP message: {}", e);
                None
            }
        }
    }
}
