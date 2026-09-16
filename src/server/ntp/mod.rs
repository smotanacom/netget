//! NTP server implementation
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::{ActionResult, Server};
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::NtpProtocol;
use crate::state::app_state::AppState;
use actions::NTP_REQUEST_EVENT;

/// NTP server that forwards requests to LLM
pub struct NtpServer;

impl NtpServer {
    /// Spawn NTP server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("NTP server listening on {}", local_addr));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            // Large enough for extension fields / an authentication MAC. Only the first
            // 48 bytes are interpreted, but a short buffer would silently truncate the
            // datagram and misreport its size to the model.
            let mut buffer = vec![0u8; 1024];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Add connection to ServerInstance (NTP "connection" = recent client)
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = crate::utils::clock::Instant::now();
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

                        // DEBUG: Log summary (FileOnly, hot per-packet path)
                        Log::new(Some(&status_tx))
                            .debug(format!("NTP received {} bytes from {}", n, peer_addr));

                        // TRACE: Log full payload (always hex for NTP, FileOnly)
                        let hex_str = hex::encode(&data);
                        Log::new(Some(&status_tx)).trace(format!("NTP data (hex): {}", hex_str));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();

                        tokio::spawn(async move {
                            // Get current Unix timestamp
                            use crate::utils::clock::{SystemTime, UNIX_EPOCH};
                            let current_unix_time = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();

                            // Version (bits 5-3) and mode (bits 2-0) of the request. The
                            // reply must come back in the client's own version.
                            let (client_version, client_mode) = match data.first() {
                                Some(b) => ((b >> 3) & 0x07, b & 0x07),
                                None => (4, 3),
                            };

                            // Parse client's transmit timestamp from request (bytes 40-47).
                            // RFC 5905 requires this to come back verbatim as the reply's
                            // origin timestamp; the client uses it to match the response to
                            // its request and rejects anything else.
                            let (client_transmit_unix, client_transmit_ntp) = if data.len() >= 48 {
                                let seconds =
                                    u32::from_be_bytes([data[40], data[41], data[42], data[43]])
                                        as u64;
                                let fraction =
                                    u32::from_be_bytes([data[44], data[45], data[46], data[47]])
                                        as u64;
                                let ntp_timestamp = (seconds << 32) | fraction; // Full 64-bit NTP timestamp

                                // Convert seconds part to Unix timestamp for the LLM prompt
                                let unix_ts = if seconds > 2_208_988_800 {
                                    Some(seconds - 2_208_988_800)
                                } else {
                                    None
                                };

                                (unix_ts, Some(ntp_timestamp))
                            } else {
                                (None, None)
                            };

                            // One protocol instance per request, carrying that request's
                            // origin timestamp and version, so overlapping requests cannot
                            // pick up each other's values.
                            let protocol =
                                NtpProtocol::for_request(client_transmit_ntp, client_version);

                            // Create NTP request event
                            let mut event_data = serde_json::json!({
                                "current_time": current_unix_time,
                                "client_version": client_version,
                                "client_mode": client_mode,
                                "bytes_received": data.len()
                            });

                            // Expose the raw 64-bit NTP value, which is what
                            // origin_timestamp needs; the Unix form is lossy and is
                            // provided only for readability.
                            if let Some(ntp_ts) = client_transmit_ntp {
                                event_data["client_transmit_timestamp"] = serde_json::json!(ntp_ts);
                            }
                            if let Some(unix_ts) = client_transmit_unix {
                                event_data["client_transmit_unix"] = serde_json::json!(unix_ts);
                            }

                            let event = Event::new(&NTP_REQUEST_EVENT, event_data);

                            // A normal NTP time response is mechanical: every field has a
                            // correct default (stratum 2, LOCL reference id, current-time
                            // timestamps) and the client's transmit timestamp is echoed as the
                            // origin. There is nothing to decide, so by default we answer
                            // statically with NO LLM round-trip. The model is consulted only when
                            // the operator opts in — a server instruction or a per-event handler
                            // — which is how one asks the server to skew or lie about the time.
                            let wants_dynamic = operator_wants_dynamic(
                                &state_clone,
                                server_id,
                                &event.event_type.id,
                            )
                            .await;

                            if !wants_dynamic {
                                // No model was consulted at all: the operator opted into
                                // nothing, so the mechanical answer *is* the policy. Tagged
                                // distinctly so it can never be read as a model answer.
                                send_static_time_response(
                                    &protocol,
                                    socket_clone.as_ref(),
                                    peer_addr,
                                    &status_clone,
                                    "static_default",
                                )
                                .await;
                                return;
                            }

                            Log::new(Some(&status_clone))
                                .debug(format!("NTP calling LLM for request from {}", peer_addr));

                            match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None, // NTP uses UDP, no persistent connection
                                &event,
                                &protocol,
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    // Display messages from LLM
                                    for message in &execution_result.messages {
                                        Log::new(Some(&status_clone)).info(format!("{}", message));
                                    }

                                    Log::new(Some(&status_clone)).debug(format!(
                                        "NTP parsed {} actions",
                                        execution_result.raw_actions.len()
                                    ));

                                    // Process protocol results
                                    Log::new(Some(&status_clone)).debug(format!(
                                        "NTP got {} protocol results",
                                        execution_result.protocol_results.len()
                                    ));

                                    // Classify the outcome *before* `protocol_results` is
                                    // consumed below. Without this the log cannot tell a model
                                    // that answered from one that refused (`ignore_request`)
                                    // from one that said nothing at all - all three leave the
                                    // same trace today, and two of them leave the client with
                                    // no reply.
                                    let produced_output = execution_result
                                        .protocol_results
                                        .iter()
                                        .any(|r| !r.get_all_output().is_empty());
                                    let explicitly_ignored =
                                        execution_result.raw_actions.iter().any(|a| {
                                            a.get("type").and_then(|t| t.as_str())
                                                == Some("ignore_request")
                                        });
                                    let action_failures = execution_result
                                        .failures
                                        .iter()
                                        .map(|f| format!("{}: {}", f.action, f.error))
                                        .collect::<Vec<_>>()
                                        .join("; ");

                                    let decision_log = Log::new(Some(&status_clone));
                                    if produced_output {
                                        decision_log.info(format!(
                                            "NTP request from {} decision=model_answer",
                                            peer_addr
                                        ));
                                    } else if explicitly_ignored {
                                        decision_log.info(format!(
                                            "NTP request from {} decision=model_reject \
                                             (ignore_request: no reply sent)",
                                            peer_addr
                                        ));
                                    } else if !action_failures.is_empty() {
                                        decision_log.error(format!(
                                            "NTP request from {} decision=fail_closed_bad_action \
                                             ({}); no reply sent",
                                            peer_addr, action_failures
                                        ));
                                    } else {
                                        decision_log.warn(format!(
                                            "NTP request from {} decision=model_silent; no reply \
                                             sent and the client keeps polling",
                                            peer_addr
                                        ));
                                    }

                                    for protocol_result in execution_result.protocol_results {
                                        if let Some(output_data) =
                                            protocol_result.get_all_output().first()
                                        {
                                            let _ =
                                                socket_clone.send_to(output_data, peer_addr).await;

                                            let log = Log::new(Some(&status_clone));

                                            // DEBUG: Log summary (FileOnly, hot per-packet path)
                                            log.debug(format!(
                                                "NTP sent {} bytes to {}",
                                                output_data.len(),
                                                peer_addr
                                            ));

                                            // TRACE: Log full payload (always hex for NTP, FileOnly)
                                            let hex_str = hex::encode(output_data);
                                            log.trace(format!("NTP sent (hex): {}", hex_str));

                                            log.info(format!(
                                                "NTP response to {} ({} bytes)",
                                                peer_addr,
                                                output_data.len()
                                            ));
                                        } else {
                                            Log::new(Some(&status_clone))
                                                .debug("NTP protocol result has no output data");
                                        }
                                    }
                                }
                                Err(e) => {
                                    // The operator opted into LLM control and the backend
                                    // failed. Answering with the mechanical stratum-2 time is
                                    // a **fail-open**: the client accepts it and sets its
                                    // clock, so an outage silently serves time on a server the
                                    // operator had asked a model to decide for.
                                    //
                                    // NTP has no error message, but RFC 5905 §7.4 gives it a
                                    // way to say "do not use me": a Kiss-o'-Death — stratum 0,
                                    // LI 3, a four-character kiss code. chrony, ntpd and
                                    // ntpdate all recognise it, refuse to take time from the
                                    // packet, and back off. It can never be mistaken for a
                                    // time sample, which is exactly what fails closed here,
                                    // and it is still a *reply*, so the client is not left
                                    // retrying against a server it thinks is merely slow.
                                    //
                                    // `RATE` when the backend is saturated (the client should
                                    // slow down and retry) and `INIT` otherwise (not yet
                                    // synchronised) are the registered codes for these two
                                    // cases.
                                    let (category, kiss_code, decision) =
                                        match crate::utils::WireFailure::classify(&e) {
                                            crate::utils::WireFailure::Overloaded => {
                                                ("overloaded", "RATE", "fail_closed_llm_overloaded")
                                            }
                                            crate::utils::WireFailure::Unavailable => {
                                                ("unavailable", "INIT", "fail_closed_llm_error")
                                            }
                                        };
                                    Log::new(Some(&status_clone)).error(format!(
                                        "NTP request from {} decision={} category={}: {} — \
                                         answering Kiss-o'-Death ({}), which no client will \
                                         take time from",
                                        peer_addr, decision, category, e, kiss_code
                                    ));
                                    send_kod_response(
                                        socket_clone.as_ref(),
                                        peer_addr,
                                        client_version,
                                        client_transmit_ntp,
                                        kiss_code,
                                        &status_clone,
                                        decision,
                                    )
                                    .await;
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!("NTP receive error: {}", e);
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

/// Returns true if the operator opted into dynamic (LLM- or handler-driven) responses for
/// this server: either a non-empty server instruction was given, or an event handler is
/// configured for `event_id`. When false the protocol answers with a correct static default
/// and never consults the model — a normal NTP time response is fully determined by the
/// request plus the server's clock.
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

/// Build and send the mechanical NTP time response with no LLM involvement: stratum 2, LOCL
/// reference clock, current-time timestamps. The per-request `protocol` echoes the client's
/// transmit timestamp as the origin and answers in the client's own version.
///
/// `decision` is the token this send is recorded under — `static_default` when no model was
/// consulted, `static_default_llm_error` when one was and it failed — so the two can never be
/// conflated in the log.
/// Answer a request the backend could not decide with a Kiss-o'-Death (RFC 5905 §7.4).
///
/// This is NTP's fail-closed reply. A KoD carries stratum 0 and LI 3, so a client discards
/// it as a time sample rather than stepping its clock — which is the whole point: the
/// alternative on this path is a usable stratum-2 answer, and that turns a backend outage
/// into the server quietly serving time it was told not to serve on its own authority.
///
/// The client's transmit timestamp is echoed as the origin timestamp for the same reason a
/// normal reply echoes it: a reply failing that check is discarded, which would put us back
/// at silence.
#[allow(clippy::too_many_arguments)]
async fn send_kod_response(
    socket: &UdpSocket,
    peer_addr: SocketAddr,
    version: u8,
    origin_timestamp: Option<u64>,
    kiss_code: &str,
    status: &mpsc::UnboundedSender<String>,
    decision: &str,
) {
    let packet =
        crate::server::ntp::actions::build_kod_packet(version, origin_timestamp, kiss_code);
    match socket.send_to(&packet, peer_addr).await {
        Ok(sent) => {
            Log::new(Some(status)).info(format!(
                "NTP Kiss-o'-Death ({}) to {} ({} bytes) decision={}",
                kiss_code, peer_addr, sent, decision
            ));
        }
        Err(e) => {
            Log::new(Some(status)).error(format!(
                "NTP Kiss-o'-Death to {peer_addr} could not be sent \
                 decision=fail_closed_write_error: {e}"
            ));
        }
    }
}

async fn send_static_time_response(
    protocol: &NtpProtocol,
    socket: &UdpSocket,
    peer_addr: SocketAddr,
    status: &mpsc::UnboundedSender<String>,
    decision: &str,
) {
    let action = serde_json::json!({
        "type": "send_ntp_time_response",
        "stratum": 2,
        "reference_id": "LOCL",
        "reference_timestamp": "current_time",
        "receive_timestamp": "current_time",
        "transmit_timestamp": "current_time"
    });
    match protocol.execute_action(action) {
        Ok(ActionResult::Output(bytes)) => {
            let _ = socket.send_to(&bytes, peer_addr).await;
            Log::new(Some(status)).info(format!(
                "NTP static time response to {} ({} bytes) decision={}",
                peer_addr,
                bytes.len(),
                decision
            ));
        }
        Ok(_) => {
            warn!(
                "NTP static time response produced no output for {} \
                 decision=fail_closed_action_error",
                peer_addr
            );
        }
        Err(e) => {
            Log::new(Some(status)).error(format!(
                "NTP static time response to {peer_addr} failed \
                 decision=fail_closed_action_error: {e}"
            ));
        }
    }
}
