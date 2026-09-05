//! RTP server (RFC 3550).
//!
//! Binds a UDP socket and speaks Real-time Transport Protocol. When a datagram arrives it is
//! parsed as RTP (or RTCP) and turned into an event; the model answers by *describing* what the
//! stream should carry (a tone, DTMF, silence) and this server synthesizes G.711 and frames it
//! into correct RTP packets sent back to the peer. See `media.rs` for the VNC-style text-to-media
//! engine, shared with the `rtsp` control server.

pub mod actions;
pub mod media;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::error;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::RtpProtocol;
use crate::state::app_state::AppState;
use actions::{RTCP_RECEIVED_EVENT, RTP_RECEIVED_EVENT};
use media::{AudioCodec, RtpPacketizer};

/// RTP server that generates/answers media streams under LLM control.
pub struct RtpServer;

impl RtpServer {
    /// Spawn the RTP server. Awaits the socket bind so a failure is reported to
    /// `server_startup` as `Err`, and registers the accept loop so `stop_server` can abort it.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("RTP server listening on {}", local_addr));

        let protocol = Arc::new(RtpProtocol::new());
        let task_registrar = app_state.clone();

        let accept_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 65535];
            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

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
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            Self::handle_datagram(
                                &data,
                                peer_addr,
                                local_addr,
                                connection_id,
                                server_id,
                                &llm_clone,
                                &state_clone,
                                &status_clone,
                                &socket_clone,
                                protocol_clone.as_ref(),
                            )
                            .await;
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("RTP recv error: {}", e));
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;
        Ok(local_addr)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_datagram(
        data: &[u8],
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm: &OllamaClient,
        state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
        socket: &UdpSocket,
        protocol: &RtpProtocol,
    ) {
        let is_rtcp = media::is_rtcp(data);
        let base = serde_json::json!({
            "peer_addr": peer_addr.to_string(),
            "local_addr": local_addr.to_string(),
            "connection_id": connection_id.to_string(),
        });

        let event = if is_rtcp {
            let mut d = base;
            d["packet_type"] = serde_json::json!(data.get(1).copied().unwrap_or(0));
            d["length"] = serde_json::json!(data.len());
            Event {
                event_type: &RTCP_RECEIVED_EVENT,
                data: d,
            }
        } else {
            let parsed = match media::parse_rtp(data) {
                Some(p) => p,
                None => {
                    Log::new(Some(status_tx)).warn(format!(
                        "RTP unparseable datagram from {} ({} bytes)",
                        peer_addr,
                        data.len()
                    ));
                    return;
                }
            };
            Log::new(Some(status_tx)).trace(format!(
                "RTP in: pt={} seq={} ts={} ssrc={:08x} len={}",
                parsed.payload_type,
                parsed.sequence,
                parsed.timestamp,
                parsed.ssrc,
                parsed.payload_len
            ));
            let mut d = base;
            d["payload_type"] = serde_json::json!(parsed.payload_type);
            d["sequence"] = serde_json::json!(parsed.sequence);
            d["timestamp"] = serde_json::json!(parsed.timestamp);
            d["ssrc"] = serde_json::json!(parsed.ssrc);
            d["marker"] = serde_json::json!(parsed.marker);
            d["payload_len"] = serde_json::json!(parsed.payload_len);
            Event {
                event_type: &RTP_RECEIVED_EVENT,
                data: d,
            }
        };

        let event_id = event.event_type.id.clone();
        match call_llm(llm, state, server_id, Some(connection_id), &event, protocol).await {
            Ok(result) => {
                if result.raw_actions.is_empty() {
                    // The model answered, and its answer was "stream nothing". For RTP that is a
                    // legitimate answer — a receiver owes its sender no media — but on the wire it
                    // is byte-identical to a backend outage, so the two must be separable here.
                    Log::new(Some(status_tx)).info(format!(
                        "RTP {} from {} decision=model_sent_nothing (no media requested)",
                        event_id, peer_addr
                    ));
                }
                for action in &result.raw_actions {
                    Self::execute_send_action(
                        action, peer_addr, socket, status_tx, state, server_id,
                    )
                    .await;
                }
            }
            Err(e) => {
                // Fail closed: RTP is a one-way media transport with no error frame and no
                // request/response turn, so we emit nothing on the wire rather than falling
                // through to some default stream (which would fabricate media the model never
                // authorized) or inventing an RTCP BYE for a session we never joined. The peer is
                // not blocked on us; the operator is the one who needs to know, so the whole
                // error goes to the log and the status stream and nothing goes to the socket.
                //
                // `decision=` mirrors `src/server/radius/`: an operator greps `fail_closed_` to
                // find every datagram the model did not actually answer, and the overloaded /
                // errored split is kept even though RTP has no way to express it on the wire.
                // There is no `model_reject` counterpart — RTP has no accept/deny semantics, so
                // a model declining to stream is exactly the `model_sent_nothing` case above.
                let decision = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                    "fail_closed_backend_overloaded"
                } else {
                    "fail_closed_backend_error"
                };
                Log::new(Some(status_tx)).error(format!(
                    "RTP {} from {} decision={}; no media sent: {}",
                    event_id, peer_addr, decision, e
                ));
            }
        }
    }

    /// Interpret one model action and, if it is a media-send, synthesize and transmit RTP.
    async fn execute_send_action(
        action: &serde_json::Value,
        peer_addr: SocketAddr,
        socket: &UdpSocket,
        status_tx: &mpsc::UnboundedSender<String>,
        state: &AppState,
        server_id: crate::state::ServerId,
    ) {
        let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match action_type {
            "send_rtp_audio" => {
                Self::send_rtp_audio(action, peer_addr, socket, status_tx, state, server_id).await;
            }
            "send_rtcp_sender_report" => {
                Self::send_rtcp(action, peer_addr, socket, status_tx).await;
            }
            // Common/no-op actions (set_memory, show_message, …) are executed by the shared
            // executor already; nothing to put on the wire here.
            "" => {
                Log::new(Some(status_tx)).warn(format!(
                    "RTP action with no \"type\" field, ignored: {}",
                    action
                ));
            }
            other => {
                // Not on the wire, but not silent either: a misspelled media action is
                // otherwise indistinguishable from the model deciding to stream nothing.
                Log::new(Some(status_tx)).debug(format!(
                    "RTP no wire output for action type \"{}\" (handled by the shared executor \
                     if it is a common action)",
                    other
                ));
            }
        }
    }

    async fn send_rtp_audio(
        action: &serde_json::Value,
        peer_addr: SocketAddr,
        socket: &UdpSocket,
        status_tx: &mpsc::UnboundedSender<String>,
        state: &AppState,
        server_id: crate::state::ServerId,
    ) {
        let codec = match action
            .get("payload_type")
            .and_then(|v| v.as_str())
            .map(AudioCodec::parse)
            .unwrap_or(Ok(AudioCodec::Pcmu))
        {
            Ok(c) => c,
            Err(e) => {
                Log::new(Some(status_tx)).warn(format!("RTP send_rtp_audio: {}", e));
                return;
            }
        };
        let content = match media::parse_audio_content(action) {
            Ok(c) => c,
            Err(e) => {
                Log::new(Some(status_tx)).warn(format!("RTP send_rtp_audio content: {}", e));
                return;
            }
        };
        let duration_ms = action
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(1000);
        let payload = match media::synthesize(codec, &content, duration_ms) {
            Ok(p) => p,
            Err(e) => {
                Log::new(Some(status_tx)).warn(format!("RTP synthesis: {}", e));
                return;
            }
        };

        let ssrc = action
            .get("ssrc")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or_else(|| rand::random());
        let seq = action
            .get("start_sequence")
            .and_then(|v| v.as_u64())
            .map(|v| v as u16);
        let ts = action
            .get("start_timestamp")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let mut packetizer = RtpPacketizer::new(ssrc, codec.payload_type(), seq, ts);
        let packets = packetizer.packetize(&payload, media::G711_SAMPLES_PER_FRAME);

        let mut sent = 0u64;
        let mut bytes = 0u64;
        for pkt in &packets {
            match socket.send_to(pkt, peer_addr).await {
                Ok(w) => {
                    sent += 1;
                    bytes += w as u64;
                }
                Err(e) => {
                    error!("RTP send failed to {}: {}", peer_addr, e);
                    break;
                }
            }
            // Pace at the frame interval (20 ms) so this is a real-time stream, not a burst.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        state
            .with_server_mut(server_id, |s| {
                if let Some(c) = s
                    .connections
                    .values_mut()
                    .find(|c| c.remote_addr == peer_addr)
                {
                    c.bytes_sent += bytes;
                    c.packets_sent += sent;
                }
            })
            .await;
        // FileOnly: the send_rtp_audio action's own log_template already reports
        // "-> RTP {content} {payload_type} {duration_ms}ms" to the TUI at INFO.
        Log::new(Some(status_tx)).debug(format!(
            "RTP sent {} {} packet(s) ({} bytes) to {}",
            sent,
            codec.rtpmap_name(),
            bytes,
            peer_addr
        ));
    }

    async fn send_rtcp(
        action: &serde_json::Value,
        peer_addr: SocketAddr,
        socket: &UdpSocket,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let ssrc = action
            .get("ssrc")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or_else(rand::random);
        let rtp_ts = action
            .get("rtp_timestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let packet_count = action
            .get("packet_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let octet_count = action
            .get("octet_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let sr = media::build_rtcp_sender_report(ssrc, rtp_ts, packet_count, octet_count);
        match socket.send_to(&sr, peer_addr).await {
            Ok(w) => {
                // FileOnly: the send_rtcp_sender_report action's own log_template already
                // reports "-> RTCP SR" to the TUI at INFO.
                Log::new(Some(status_tx)).debug(format!("RTCP SR to {} ({} bytes)", peer_addr, w));
            }
            Err(e) => {
                Log::new(Some(status_tx)).error(format!("RTCP send failed: {}", e));
            }
        }
    }
}
