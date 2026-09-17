//! CoAP (RFC 7252) server over UDP.
//!
//! Owns the message layer — types, message ids, tokens, and the mechanical replies the
//! specification leaves no room to decide — while the model owns the resource layer. See
//! `src/server/coap/CLAUDE.md`.

pub mod actions;
pub mod codec;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::{console_debug, console_trace};

use actions::{CoapProtocol, COAP_REQUEST_EVENT, RESULT_IGNORE, RESULT_RESET, RESULT_RESPONSE};
use codec::{CoapMessage, MessageType};

/// CoAP server.
pub struct CoapServer;

impl CoapServer {
    /// Bind the UDP socket and start the receive loop.
    ///
    /// Returns `Err` when the socket cannot be bound, so `server_startup` records
    /// `ServerStatus::Error` instead of a server that never received anything.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("CoAP server listening on {local_addr}"));

        let protocol = Arc::new(CoapProtocol::new());
        // Message ids for Non-confirmable replies, which do not reuse the request's.
        let next_message_id = Arc::new(AtomicU16::new(1));

        let task_registrar = app_state.clone();
        let recv_handle = tokio::spawn(async move {
            Log::new(Some(&status_tx)).info(format!("CoAP receive loop started on {local_addr}"));

            // Deliberately larger than `codec::MAX_MESSAGE_LEN` (1152), and that is the
            // whole point of the number rather than headroom for its own sake: `recv_from`
            // silently discards whatever does not fit, so a buffer sized *at* the bound
            // would deliver a 5000-byte datagram as a legal-looking 1152-byte one and the
            // guard below could never fire. With 2048 anything over the bound is seen to
            // be over it.
            let mut buffer = vec![0u8; 2048];

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
                        let now = crate::utils::clock::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ServerConnectionState {
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
                                },
                            )
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        console_debug!(status_tx, "CoAP received {} bytes from {}", n, peer_addr);
                        console_trace!(status_tx, "CoAP received (hex): {}", hex::encode(&data));

                        let llm = llm_client.clone();
                        let st = app_state.clone();
                        let stx = status_tx.clone();
                        let sock = socket.clone();
                        let proto = protocol.clone();
                        let mid = next_message_id.clone();

                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                Self::handle_datagram(
                                    data,
                                    peer_addr,
                                    connection_id,
                                    server_id,
                                    llm,
                                    st,
                                    stx,
                                    sock,
                                    proto,
                                    mid,
                                )
                                .await;
                            })
                            .await;
                    }
                    Err(e) => {
                        error!("CoAP receive error: {}", e);
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, recv_handle)
            .await;

        Ok(local_addr)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_datagram(
        data: Vec<u8>,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        socket: Arc<UdpSocket>,
        protocol: Arc<CoapProtocol>,
        next_message_id: Arc<AtomicU16>,
    ) {
        // --- The inbound size bound, before anything is decoded or asked -------------
        //
        // RFC 7252 §4.6 puts MAX_MESSAGE_SIZE at 1152 bytes when the path MTU is unknown,
        // and Block-wise transfer (RFC 7959) — the legal way to exceed it — is not
        // implemented here. So a larger datagram is refused in CoAP's own vocabulary
        // (4.13 Request Entity Too Large, §5.9.2.9) rather than decoded, and the model is
        // never asked about it: an oversize request must not become a prompt.
        if data.len() > codec::MAX_MESSAGE_LEN {
            warn!(
                "CoAP refusing a {}-byte datagram from {} (limit {}) decision={}",
                data.len(),
                peer_addr,
                codec::MAX_MESSAGE_LEN,
                Decision::RefusedTooLarge.as_str()
            );
            let _ = status_tx.send(format!(
                "✗ CoAP refused a {}-byte datagram from {peer_addr}: over the {}-byte limit",
                data.len(),
                codec::MAX_MESSAGE_LEN
            ));

            // The refusal still has to be matchable, so it carries the request's own
            // message id and token — read from the fixed-offset prefix, which needs no
            // option walk. Without that echo a client discards it and the refusal is
            // indistinguishable from the silence it exists to avoid.
            if let Some(prefix) = codec::message_prefix(&data) {
                let reply = if codec::method_name(prefix.code).is_some() {
                    let fresh = next_message_id.fetch_add(1, Ordering::Relaxed);
                    Some(codec::response_to_prefix(
                        &prefix,
                        fresh,
                        codec::CODE_REQUEST_ENTITY_TOO_LARGE,
                    ))
                } else if prefix.mtype == MessageType::Confirmable {
                    // Not a request, so 4.13 would be a category error; RFC 7252 §4.2
                    // rejects a Confirmable message that cannot be processed with a Reset.
                    Some(codec::reset_for(prefix.message_id))
                } else {
                    None
                };
                if let Some(reply) = reply {
                    Self::send(
                        &reply,
                        peer_addr,
                        connection_id,
                        server_id,
                        &socket,
                        &app_state,
                        &status_tx,
                    )
                    .await;
                }
            }
            return;
        }

        let request = match CoapMessage::decode(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!("CoAP malformed message from {}: {}", peer_addr, e);
                let _ = status_tx.send(format!("✗ CoAP malformed message from {peer_addr}: {e}"));

                // RFC 7252 §4.2: a Confirmable message that cannot be processed is
                // rejected with a Reset. The message id lives in bytes 2-3, which we
                // still have whenever the datagram reached the header length at all.
                if data.len() >= codec::HEADER_LEN
                    && MessageType::from_bits(data[0] >> 4) == MessageType::Confirmable
                {
                    let message_id = u16::from_be_bytes([data[2], data[3]]);
                    Self::send(
                        &codec::reset_for(message_id),
                        peer_addr,
                        connection_id,
                        server_id,
                        &socket,
                        &app_state,
                        &status_tx,
                    )
                    .await;
                }
                return;
            }
        };

        // --- Message-layer traffic the specification answers on its own -------------

        if request.is_empty_message() {
            match request.mtype {
                // RFC 7252 §4.3 CoAP Ping: an empty Confirmable message is answered with
                // a Reset. This is not a decision, so it does not reach the model.
                MessageType::Confirmable => {
                    debug!(
                        "CoAP ping from {}; replying RST decision={}",
                        peer_addr,
                        Decision::SpecReply.as_str()
                    );
                    Self::send(
                        &codec::reset_for(request.message_id),
                        peer_addr,
                        connection_id,
                        server_id,
                        &socket,
                        &app_state,
                        &status_tx,
                    )
                    .await;
                }
                other => {
                    debug!(
                        "CoAP empty {} message from {} ignored",
                        other.as_str(),
                        peer_addr
                    );
                }
            }
            return;
        }

        if !request.is_request() {
            // A response code arriving at a server, or a method this version of CoAP does
            // not define. RFC 7252 §5.8 defines only GET/POST/PUT/DELETE.
            if codec::code_class(request.code) == 0 {
                warn!(
                    "CoAP unrecognised method {} from {}; replying 4.05 decision={}",
                    codec::code_to_string(request.code),
                    peer_addr,
                    Decision::SpecReply.as_str()
                );
                let fresh = next_message_id.fetch_add(1, Ordering::Relaxed);
                let response = codec::response_to(&request, fresh, codec::CODE_METHOD_NOT_ALLOWED);
                Self::send(
                    &response,
                    peer_addr,
                    connection_id,
                    server_id,
                    &socket,
                    &app_state,
                    &status_tx,
                )
                .await;
            } else {
                debug!(
                    "CoAP {} message with response code {} from {} ignored; this is a server",
                    request.mtype.as_str(),
                    codec::code_to_string(request.code),
                    peer_addr
                );
            }
            return;
        }

        // --- Resource layer: the model decides -------------------------------------

        let method = codec::method_name(request.code).unwrap_or("GET");
        let path = request.uri_path();

        let mut event_data = serde_json::json!({
            "method": method,
            "path": path,
            "path_segments": request.path_segments(),
            "message_type": request.mtype.as_str(),
            "message_id": request.message_id,
        });
        if let Some(query) = request.uri_query() {
            event_data["query"] = serde_json::json!(query);
        }
        if let Some(cf) = request.option_uint(codec::OPT_CONTENT_FORMAT) {
            let id = cf as u16;
            event_data["content_format"] = serde_json::json!(
                codec::content_format_name(id).unwrap_or("application/octet-stream")
            );
        }
        if let Some(accept) = request.option_uint(codec::OPT_ACCEPT) {
            let id = accept as u16;
            event_data["accept"] = serde_json::json!(
                codec::content_format_name(id).unwrap_or("application/octet-stream")
            );
        }
        if !request.payload.is_empty() {
            let printable = request
                .payload
                .iter()
                .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace());
            if printable {
                event_data["payload"] =
                    serde_json::json!(String::from_utf8_lossy(&request.payload).to_string());
                event_data["payload_encoding"] = serde_json::json!("utf8");
            } else {
                event_data["payload"] = serde_json::json!(hex::encode(&request.payload));
                event_data["payload_encoding"] = serde_json::json!("hex");
            }
        }

        debug!(
            "CoAP {} {} {} from {} (mid={})",
            request.mtype.as_str(),
            method,
            path,
            peer_addr,
            request.message_id
        );

        let event = Event::new(&COAP_REQUEST_EVENT, event_data);

        let (decision, outcome) = match call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                for msg in execution_result.messages {
                    let _ = status_tx.send(msg);
                }
                Self::outcome_from_results(&request, &execution_result.protocol_results)
            }
            Err(e) => {
                // The error text stays in the log; the peer gets only the code.
                error!("CoAP LLM error for {} {}: {}", method, path, e);
                let _ = status_tx.send(format!("✗ CoAP LLM error: {e}"));
                // Fail closed with a code that means exactly what happened, rather than
                // inventing a representation or leaving the client to retransmit.
                (
                    Decision::FailClosedLlmError,
                    Outcome::Response {
                        code: codec::CODE_SERVICE_UNAVAILABLE,
                        payload: Vec::new(),
                        content_format: None,
                    },
                )
            }
        };

        // Log the decision before writing it, and make the fail-closed paths loud. 5.03 is
        // both what a backend outage produces and something the model may pick itself, so
        // the peer cannot tell them apart and an operator has to be able to.
        let summary = format!(
            "CoAP {method} {path} from {peer_addr} decision={}",
            decision.as_str()
        );
        if decision.is_fail_closed() {
            Log::new(Some(&status_tx)).error(format!(
                "{summary} (answered 5.03 because no usable answer was produced)"
            ));
        } else {
            debug!("{summary}");
        }

        let fresh = next_message_id.fetch_add(1, Ordering::Relaxed);
        let message = match outcome {
            Outcome::Ignore => {
                debug!(
                    "CoAP deliberately sending no reply to {} {} from {}",
                    method, path, peer_addr
                );
                return;
            }
            Outcome::Reset => codec::reset_for(request.message_id),
            Outcome::Response {
                code,
                payload,
                content_format,
            } => {
                let mut response = codec::response_to(&request, fresh, code);
                if let Some(cf) = content_format {
                    response
                        .options
                        .push((codec::OPT_CONTENT_FORMAT, uint_option_value(cf as u32)));
                }
                response.payload = payload;
                response
            }
        };

        Self::send(
            &message,
            peer_addr,
            connection_id,
            server_id,
            &socket,
            &app_state,
            &status_tx,
        )
        .await;
    }

    /// Interpret the model's structured answer.
    ///
    /// Fails closed: no usable action becomes 5.03 Service Unavailable, never a
    /// plausible-looking 2.05 with an empty body.
    fn outcome_from_results(
        request: &CoapMessage,
        results: &[ActionResult],
    ) -> (Decision, Outcome) {
        for result in results {
            let ActionResult::Custom { name, data } = result else {
                continue;
            };
            match name.as_str() {
                RESULT_RESPONSE => {
                    let code = data
                        .get("code")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(codec::CODE_INTERNAL_SERVER_ERROR as u64)
                        as u8;
                    let payload = data
                        .get("payload_hex")
                        .and_then(|v| v.as_str())
                        .and_then(|s| hex::decode(s).ok())
                        .unwrap_or_default();
                    let content_format = data
                        .get("content_format")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u16);
                    // A class-2 code is the model serving the resource; 4.xx and 5.xx are
                    // it refusing. `execute_action` has already rejected every other class.
                    let decision = if codec::code_class(code) == 2 {
                        Decision::ModelAnswer
                    } else {
                        Decision::ModelReject
                    };
                    return (
                        decision,
                        Outcome::Response {
                            code,
                            payload,
                            content_format,
                        },
                    );
                }
                RESULT_RESET => return (Decision::ModelReset, Outcome::Reset),
                RESULT_IGNORE => return (Decision::ModelSilent, Outcome::Ignore),
                _ => {}
            }
        }

        error!(
            "CoAP: no usable action returned for {} {}; answering 5.03 Service Unavailable",
            codec::method_name(request.code).unwrap_or("?"),
            request.uri_path()
        );
        (
            Decision::FailClosedNoAction,
            Outcome::Response {
                code: codec::CODE_SERVICE_UNAVAILABLE,
                payload: Vec::new(),
                content_format: None,
            },
        )
    }

    /// Encode and send one message, updating counters and the dual logs.
    async fn send(
        message: &CoapMessage,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        socket: &Arc<UdpSocket>,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        // A message the codec refuses is never patched up and sent anyway: a CoAP reply whose
        // token was shortened to fit matches nothing at the client, which is a silent drop
        // dressed as an answer.
        let bytes = match message.encode() {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(
                    "CoAP decision=fail_closed_encode: refusing to send {} to {}: {}",
                    codec::code_to_string(message.code),
                    peer_addr,
                    e
                );
                let _ = status_tx.send(format!(
                    "✗ CoAP refused to encode a reply to {peer_addr}: {e}"
                ));
                return;
            }
        };
        if let Err(e) = socket.send_to(&bytes, peer_addr).await {
            error!("CoAP send to {} failed: {}", peer_addr, e);
            let _ = status_tx.send(format!("✗ CoAP send to {peer_addr} failed: {e}"));
            return;
        }

        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;

        let log = Log::new(Some(&status_tx));
        log.debug(format!(
            "CoAP sent {} {} ({} bytes) to {peer_addr}",
            message.mtype.as_str(),
            codec::code_to_string(message.code),
            bytes.len()
        ));
        log.trace(format!("CoAP sent (hex): {}", hex::encode(&bytes)));
        let _ = status_tx.send(format!("→ CoAP response to {peer_addr}"));
    }
}

/// Why a request is being answered the way it is.
///
/// 5.03 Service Unavailable is what both fail-closed paths put on the wire, and it is also
/// a code the model may legitimately choose itself, so the three are indistinguishable to
/// the peer. The log carries the distinction instead, with stable tokens -
/// `decision=fail_closed_` finds every request the model did not actually answer. Mirrors
/// `src/server/radius/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// The model served the resource (a class-2 response).
    ModelAnswer,
    /// The model refused, with a class-4 or class-5 code of its choosing.
    ModelReject,
    /// The model chose a Reset - "this message makes no sense at all".
    ModelReset,
    /// The model chose to send nothing. A decision, not an absence of one.
    ModelSilent,
    /// The specification determined the reply; the model was never asked.
    SpecReply,
    /// The datagram was over `codec::MAX_MESSAGE_LEN` and was refused unread. A decision
    /// the server makes on its own, so not a `fail_closed_` — but loud, because it is the
    /// tag that says an oversize request never became a prompt.
    RefusedTooLarge,
    /// The LLM backend failed. Nobody decided anything.
    FailClosedLlmError,
    /// The model was asked and returned nothing this request could use.
    FailClosedNoAction,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::ModelAnswer => "model_answer",
            Decision::ModelReject => "model_reject",
            Decision::ModelReset => "model_reset",
            Decision::ModelSilent => "model_silent",
            Decision::SpecReply => "spec_reply",
            Decision::RefusedTooLarge => "refused_too_large",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
            Decision::FailClosedNoAction => "fail_closed_no_action",
        }
    }

    fn is_fail_closed(self) -> bool {
        matches!(
            self,
            Decision::FailClosedLlmError | Decision::FailClosedNoAction
        )
    }
}

/// What to put on the wire for a request.
enum Outcome {
    Response {
        code: u8,
        payload: Vec<u8>,
        content_format: Option<u16>,
    },
    Reset,
    Ignore,
}

/// CoAP unsigned option values are minimum-length big-endian (RFC 7252 §3.2), so zero is
/// the empty string rather than a zero byte.
fn uint_option_value(value: u32) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    bytes[first..].to_vec()
}
