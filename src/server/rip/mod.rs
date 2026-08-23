//! RIP (Routing Information Protocol) server implementation
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
use crate::server::RipProtocol;
use crate::state::app_state::AppState;
use actions::RIP_REQUEST_EVENT;

/// RIP server that forwards routing requests to LLM
pub struct RipServer;

impl RipServer {
    /// Spawn RIP server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("RIP server listening on {}", local_addr));

        let protocol = Arc::new(RipProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            // Maximum RIP packet size: 4-byte header + up to 25 route entries (20 bytes each) = 504 bytes
            let mut buffer = vec![0u8; 512];
            let log = Log::new(Some(&status_tx));

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Add connection to ServerInstance (RIP "connection" = recent peer)
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

                        // Parse RIP packet to determine message type
                        if n < 4 {
                            log.debug(format!(
                                "RIP received invalid packet (too short: {} bytes) from {}",
                                n, peer_addr
                            ));
                            continue;
                        }

                        let command = data[0];
                        let version = data[1];
                        let num_entries = (n - 4) / 20;

                        // Summary + full payload FileOnly: the rip_request event template
                        // renders the equivalent line to the TUI.
                        log.debug(format!(
                            "RIP received {} bytes from {} (cmd={}, ver={}, entries={})",
                            n, peer_addr, command, version, num_entries
                        ));
                        log.trace(format!("RIP data (hex): {}", hex::encode(&data)));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let log = Log::new(Some(&status_clone));
                            // Parse RIP message type
                            let message_type = match command {
                                1 => "request",
                                2 => "response",
                                _ => "unknown",
                            };

                            // Parse route entries
                            let mut routes = Vec::new();
                            for i in 0..num_entries {
                                let offset = 4 + (i * 20);
                                if offset + 20 <= data.len() {
                                    let afi = u16::from_be_bytes([data[offset], data[offset + 1]]);
                                    let route_tag =
                                        u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
                                    let ip = format!(
                                        "{}.{}.{}.{}",
                                        data[offset + 4],
                                        data[offset + 5],
                                        data[offset + 6],
                                        data[offset + 7]
                                    );
                                    let subnet_mask = format!(
                                        "{}.{}.{}.{}",
                                        data[offset + 8],
                                        data[offset + 9],
                                        data[offset + 10],
                                        data[offset + 11]
                                    );
                                    let next_hop = format!(
                                        "{}.{}.{}.{}",
                                        data[offset + 12],
                                        data[offset + 13],
                                        data[offset + 14],
                                        data[offset + 15]
                                    );
                                    let metric = u32::from_be_bytes([
                                        data[offset + 16],
                                        data[offset + 17],
                                        data[offset + 18],
                                        data[offset + 19],
                                    ]);

                                    routes.push(serde_json::json!({
                                        "afi": afi,
                                        "route_tag": route_tag,
                                        "ip_address": ip,
                                        "subnet_mask": subnet_mask,
                                        "next_hop": next_hop,
                                        "metric": metric
                                    }));
                                }
                            }

                            // Create RIP request event
                            let event_data = serde_json::json!({
                                "command": command,
                                "version": version,
                                "message_type": message_type,
                                "routes": routes,
                                "peer_address": peer_addr.to_string(),
                                "bytes_received": data.len()
                            });

                            let event = Event::new(&RIP_REQUEST_EVENT, event_data);

                            // A RIP response is NOT wire-determined: which routes to advertise
                            // (and their metrics, including 16 = withdraw) is a routing-policy
                            // decision, exactly like DNS resolution or DHCP leasing. There is no
                            // mechanical reply to synthesise. So with no operator policy — no
                            // server instruction and no per-event handler — the spec-safe default
                            // is to advertise nothing and stay silent, WITHOUT burning an LLM
                            // round-trip (which would otherwise be asked to invent routes). The
                            // model is consulted only when the operator opts in with the routing
                            // policy it should apply.
                            if !operator_wants_dynamic(
                                &state_clone,
                                server_id,
                                &event.event_type.id,
                            )
                            .await
                            {
                                log.info(format!(
                                    "RIP {} from {} decision=static_default_silent: no routing policy configured (no LLM call)",
                                    message_type, peer_addr
                                ));
                                return;
                            }

                            log.debug(format!(
                                "RIP calling LLM for {} from {}",
                                message_type, peer_addr
                            ));

                            match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None, // RIP uses UDP, no persistent connection
                                &event,
                                protocol_clone.as_ref(),
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    // Display messages from LLM
                                    for message in &execution_result.messages {
                                        log.info(message);
                                    }

                                    // Three outcomes that all end in the same silence on the
                                    // wire must stay apart in the log, because only one of
                                    // them is a fault: an explicit `ignore_request` is a
                                    // routing decision, an empty answer is the model
                                    // declining to decide, and an error is netget failing.
                                    // Grep `decision=` to tell them apart.
                                    let named_ignore =
                                        execution_result.raw_actions.iter().any(|a| {
                                            a.get("type").and_then(|v| v.as_str())
                                                == Some("ignore_request")
                                        });
                                    let decision = if named_ignore {
                                        "model_ignore"
                                    } else if execution_result.raw_actions.is_empty() {
                                        "no_answer_silent"
                                    } else {
                                        "model_advertise"
                                    };
                                    log.debug(format!(
                                        "RIP {} from {} decision={} ({} actions)",
                                        message_type,
                                        peer_addr,
                                        decision,
                                        execution_result.raw_actions.len()
                                    ));

                                    // Process protocol results
                                    log.debug(format!(
                                        "RIP got {} protocol results",
                                        execution_result.protocol_results.len()
                                    ));

                                    for protocol_result in execution_result.protocol_results {
                                        if let Some(output_data) =
                                            protocol_result.get_all_output().first()
                                        {
                                            let _ =
                                                socket_clone.send_to(output_data, peer_addr).await;

                                            // Summary + full payload FileOnly: the
                                            // send_rip_* action template already reports the
                                            // send to the TUI.
                                            log.debug(format!(
                                                "RIP sent {} bytes to {}",
                                                output_data.len(),
                                                peer_addr
                                            ));
                                            log.trace(format!(
                                                "RIP sent (hex): {}",
                                                hex::encode(output_data)
                                            ));
                                        } else {
                                            log.debug("RIP protocol result has no output data");
                                        }
                                    }
                                }
                                Err(e) => {
                                    // RIP has no error message: RFC 2453 defines Request (1)
                                    // and Response (2) and nothing else, and a Response is a
                                    // routing assertion. Inventing one here would put a
                                    // fabricated routing statement into a peer's table, so
                                    // the spec-safe answer to an internal failure is the same
                                    // silence this server already applies when no routing
                                    // policy is configured. The peer is not blocked by it:
                                    // RIP is connectionless and re-requests on its own timer.
                                    //
                                    // The failure is therefore reported to the operator only.
                                    // The category is logged separately from the error so a
                                    // saturated backend (retry worthwhile) is distinguishable
                                    // from a broken one, matching what a protocol with error
                                    // codes would put on the wire. The full error goes to the
                                    // log and the status stream and never anywhere else.
                                    let category = if crate::utils::WireFailure::classify(&e)
                                        .is_overloaded()
                                    {
                                        "overloaded"
                                    } else {
                                        "unavailable"
                                    };
                                    log.warn(format!(
                                        "RIP {} from {} decision=fail_closed_llm_error category={}, no response sent: {}",
                                        message_type, peer_addr, category, e
                                    ));
                                }
                            }
                        });
                    }
                    Err(e) => {
                        log.error(format!("RIP receive error: {}", e));
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

/// Returns true if the operator opted into dynamic (LLM- or handler-driven) responses for this
/// server: either a non-empty server instruction was given, or an event handler is configured
/// for `event_id`. When false the protocol applies its static default and never consults the
/// model — for a policy protocol like RIP that default is to stay silent, because with no
/// configured policy there is nothing correct to advertise.
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
