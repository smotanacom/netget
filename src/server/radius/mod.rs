//! RADIUS server (RFC 2865 authentication/authorization, RFC 2866 accounting).
//!
//! The model makes the authorization decision. This file owns the socket, the codec and —
//! most importantly — the guarantee that **no decision means denial**.
//!
//! See `src/server/radius/CLAUDE.md` for the design rationale and the list of things this
//! server deliberately does not implement.

pub mod actions;
pub mod packet;

pub use actions::RadiusProtocol;

use crate::llm::action_helper::call_llm;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

use crate::logging::emit::Log;

use actions::{
    RADIUS_ACCESS_REQUEST_EVENT, RADIUS_ACCOUNTING_REQUEST_EVENT, RADIUS_STATUS_SERVER_EVENT,
};
use packet::{Attribute, RadiusPacket};

/// How the reply was arrived at. This exists so the log can never conflate "the model said
/// no" with "the model said nothing" — the OAuth2 failure mode, where those two collapsed
/// into one another and silence became approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The model returned `send_access_accept`.
    ModelAccept,
    /// The model returned `send_access_reject`.
    ModelReject,
    /// The model returned `send_access_challenge`.
    ModelChallenge,
    /// The model returned `send_accounting_response`.
    ModelAccountingResponse,
    /// The model returned no usable action. The server denies.
    FailClosedNoAction,
    /// The LLM call itself failed. The server denies.
    FailClosedLlmError,
    /// The model's action could not be encoded. The server denies.
    FailClosedActionError,
}

impl Decision {
    /// Stable token used in logs and in the status stream. Grep-able, and distinct per path.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::ModelAccept => "model_accept",
            Decision::ModelReject => "model_reject",
            Decision::ModelChallenge => "model_challenge",
            Decision::ModelAccountingResponse => "model_accounting_response",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
            Decision::FailClosedActionError => "fail_closed_action_error",
        }
    }

    /// True when the server, not the model, chose to deny.
    pub fn is_fail_closed(&self) -> bool {
        matches!(
            self,
            Decision::FailClosedNoAction
                | Decision::FailClosedLlmError
                | Decision::FailClosedActionError
        )
    }
}

/// Classify a reply the model produced, by its RADIUS code.
fn decision_for_code(code: u8) -> Option<Decision> {
    match code {
        packet::CODE_ACCESS_ACCEPT => Some(Decision::ModelAccept),
        packet::CODE_ACCESS_REJECT => Some(Decision::ModelReject),
        packet::CODE_ACCESS_CHALLENGE => Some(Decision::ModelChallenge),
        packet::CODE_ACCOUNTING_RESPONSE => Some(Decision::ModelAccountingResponse),
        _ => None,
    }
}

pub struct RadiusServer;

impl RadiusServer {
    /// Bind the socket and start serving.
    ///
    /// Returns `Err` — so `server_startup` sets `ServerStatus::Error` — if the socket cannot
    /// be bound, or if no `shared_secret` was supplied. Running without a secret is not an
    /// option: the secret keys both the Response Authenticator and the User-Password
    /// decryption, and inventing a default would be exactly the fail-open pattern this
    /// protocol exists to avoid.
    pub async fn spawn_with_llm_actions(ctx: SpawnContext) -> Result<SocketAddr> {
        let listen_addr = ctx.legacy_listen_addr();
        let SpawnContext {
            llm_client,
            state,
            status_tx,
            server_id,
            startup_params,
            ..
        } = ctx;

        let params = startup_params.context(
            "RADIUS requires a shared_secret startup parameter; none were supplied. \
             Start the server with startup_params: {\"shared_secret\": \"...\"}",
        )?;
        let secret: Arc<Vec<u8>> = Arc::new(params.get_string("shared_secret")?.into_bytes());
        if secret.is_empty() {
            return Err(anyhow::anyhow!(
                "RADIUS shared_secret must not be empty: it keys the Response Authenticator \
                 and the User-Password decryption"
            ));
        }

        let socket = Arc::new(
            UdpSocket::bind(listen_addr)
                .await
                .with_context(|| format!("RADIUS failed to bind {}", listen_addr))?,
        );
        let local_addr = socket.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("RADIUS server listening on {}", local_addr));

        let task_registrar = state.clone();
        let accept_handle = tokio::spawn(async move {
            // RFC 2865 §3 caps a packet at 4096 octets. A larger datagram is over-long and
            // is rejected by the decoder rather than silently truncated here.
            let mut buffer = vec![0u8; packet::MAX_PACKET_LEN + 1];

            loop {
                let (n, peer_addr) = match socket.recv_from(&mut buffer).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("RADIUS receive error: {}", e));
                        break;
                    }
                };

                let data = buffer[..n].to_vec();
                trace!(
                    "RADIUS {} bytes from {}: {}",
                    n,
                    peer_addr,
                    hex::encode(&data)
                );

                let request = match RadiusPacket::decode(&data) {
                    Ok(p) => p,
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .warn(format!("RADIUS dropped datagram from {}: {}", peer_addr, e));
                        continue;
                    }
                };

                // Accounting-Request carries a verifiable authenticator. A mismatch means
                // the sender does not hold the shared secret, so the packet is dropped —
                // this is the one inbound integrity check RADIUS actually affords.
                if request.code == packet::CODE_ACCOUNTING_REQUEST {
                    if let Err(e) = packet::verify_accounting_request(&request, &secret) {
                        Log::new(Some(&status_tx)).warn(format!(
                            "RADIUS dropped Accounting-Request id={} from {}: {}",
                            request.identifier, peer_addr, e
                        ));
                        continue;
                    }
                }

                Self::record_connection(&state, server_id, local_addr, peer_addr, n).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());

                debug!(
                    "RADIUS {} id={} from {} ({} attributes)",
                    packet::code_name(request.code),
                    request.identifier,
                    peer_addr,
                    request.attributes.len()
                );

                let llm = llm_client.clone();
                let st = state.clone();
                let tx = status_tx.clone();
                let sock = socket.clone();
                let sec = secret.clone();

                tokio::spawn(async move {
                    Self::handle_request(request, peer_addr, sock, llm, st, tx, server_id, sec)
                        .await;
                });
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    async fn record_connection(
        state: &Arc<AppState>,
        server_id: ServerId,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
        bytes: usize,
    ) {
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let connection_id = ConnectionId::new(state.get_next_unified_id().await);
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr: peer_addr,
            local_addr,
            bytes_sent: 0,
            bytes_received: bytes as u64,
            packets_sent: 0,
            packets_received: 1,
            last_activity: now,
            status: ConnectionStatus::Active,
            status_changed_at: now,
            protocol_info: ProtocolConnectionInfo::empty(),
        };
        state.add_connection_to_server(server_id, conn_state).await;
    }

    /// Ask the model, then apply the fail-closed rule.
    #[allow(clippy::too_many_arguments)]
    async fn handle_request(
        request: RadiusPacket,
        peer_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        llm_client: crate::llm::ollama_client::OllamaClient,
        state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
        secret: Arc<Vec<u8>>,
    ) {
        let protocol = actions::RadiusProtocol::for_request(actions::RequestContext {
            identifier: request.identifier,
            authenticator: request.authenticator,
            secret: secret.as_ref().clone(),
            proxy_state: request
                .all(packet::ATTR_PROXY_STATE)
                .into_iter()
                .map(|v| v.to_vec())
                .collect(),
        });

        let event = match request.code {
            packet::CODE_ACCESS_REQUEST => Event::new(
                &RADIUS_ACCESS_REQUEST_EVENT,
                Self::access_request_data(&request, peer_addr, &secret),
            ),
            packet::CODE_ACCOUNTING_REQUEST => Event::new(
                &RADIUS_ACCOUNTING_REQUEST_EVENT,
                Self::accounting_request_data(&request, peer_addr),
            ),
            packet::CODE_STATUS_SERVER => Event::new(
                &RADIUS_STATUS_SERVER_EVENT,
                serde_json::json!({
                    "identifier": request.identifier,
                    "source_addr": peer_addr.to_string(),
                    "attributes": actions::attributes_json(&request),
                }),
            ),
            other => {
                // Responses and unknown codes are not requests; a server must not answer
                // them. Dropping is the RFC 2865 §3 behaviour for an invalid code.
                warn!(
                    "RADIUS ignoring {} (code {}) from {}: not a request this server serves",
                    packet::code_name(other),
                    other,
                    peer_addr
                );
                return;
            }
        };

        let llm_outcome = call_llm(&llm_client, &state, server_id, None, &event, &protocol).await;

        let (decision, reply) =
            Self::decide(&request, llm_outcome, &protocol, &status_tx, peer_addr);

        // Log the decision before writing it, and make the fail-closed paths loud. The token
        // is stable so an operator can grep for `decision=fail_closed_` and find every
        // request the model did not actually answer.
        let summary = format!(
            "RADIUS {} id={} from {} decision={}",
            packet::code_name(request.code),
            request.identifier,
            peer_addr,
            decision.as_str()
        );
        let log = Log::new(Some(&status_tx));
        if decision.is_fail_closed() {
            log.error(format!(
                "{} (denied because no usable decision was produced)",
                summary
            ));
        } else {
            log.info(&summary);
        }

        let Some(reply) = reply else {
            debug!(
                "RADIUS sending nothing for {} id={} from {}",
                packet::code_name(request.code),
                request.identifier,
                peer_addr
            );
            return;
        };

        match socket.send_to(&reply, peer_addr).await {
            Ok(sent) => {
                trace!(
                    "RADIUS sent {} bytes to {}: {}",
                    sent,
                    peer_addr,
                    hex::encode(&reply)
                );
            }
            Err(e) => {
                Log::new(Some(&status_tx))
                    .error(format!("RADIUS failed to reply to {}: {}", peer_addr, e));
            }
        }
    }

    /// **The fail-closed rule.**
    ///
    /// Returns the decision taken and the bytes to send, if any.
    ///
    /// - A usable model action is used verbatim.
    /// - No usable action, or an LLM error, on an *authorization* request produces a
    ///   synthesised Access-Reject and a `fail_closed_*` decision. It is never reported as
    ///   `model_reject`: a model that denies and a model that is unreachable must remain
    ///   distinguishable, which is precisely what OAuth2 lost.
    /// - Accounting has nothing to deny, so its safe default is *silence*: no
    ///   Accounting-Response, and the NAS retransmits.
    fn decide(
        request: &RadiusPacket,
        llm_outcome: Result<crate::llm::ExecutionResult>,
        protocol: &actions::RadiusProtocol,
        status_tx: &mpsc::UnboundedSender<String>,
        peer_addr: SocketAddr,
    ) -> (Decision, Option<Vec<u8>>) {
        let is_authorization = packet::is_authorization_request(request.code);

        let log = Log::new(Some(status_tx));
        let execution = match llm_outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }
                result
            }
            Err(e) => {
                // RADIUS has exactly one way to say no, so an overloaded backend and a dead
                // one both produce the same Access-Reject on the wire. The distinction is
                // still worth having, so it goes in the log next to the decision token —
                // the error itself is logged here and never reaches the peer.
                let category = match crate::utils::wire_failure::WireFailure::classify(&e) {
                    crate::utils::wire_failure::WireFailure::Overloaded => "overloaded",
                    crate::utils::wire_failure::WireFailure::Unavailable => "unavailable",
                };
                log.error(format!(
                    "RADIUS LLM call failed for {} (category={}): {}",
                    peer_addr, category, e
                ));
                return (
                    Decision::FailClosedLlmError,
                    Self::synthesised_reject(request, protocol, is_authorization),
                );
            }
        };

        // Take the first output that decodes as a RADIUS reply. Extra outputs are a model
        // error, not a licence to send several packets for one request.
        let mut chosen: Option<(Decision, Vec<u8>)> = None;
        let mut extra = 0usize;
        for protocol_result in &execution.protocol_results {
            for output in protocol_result.get_all_output() {
                let code = output.first().copied().unwrap_or(0);
                match decision_for_code(code) {
                    Some(decision) if chosen.is_none() => chosen = Some((decision, output)),
                    _ => extra += 1,
                }
            }
        }
        if extra > 0 {
            warn!(
                "RADIUS ignored {} extra output(s) for id={} from {}; one request gets one reply",
                extra, request.identifier, peer_addr
            );
        }

        if let Some((decision, bytes)) = chosen {
            return (decision, Some(bytes));
        }

        // Nothing usable came back. If the model produced actions at all, they failed to
        // encode; if it produced none, it stayed silent. Both deny, and both are recorded
        // as the server's decision rather than the model's.
        let decision = if execution.raw_actions.is_empty() {
            Decision::FailClosedNoAction
        } else {
            Decision::FailClosedActionError
        };
        (
            decision,
            Self::synthesised_reject(request, protocol, is_authorization),
        )
    }

    /// Build the Access-Reject the server sends when the model did not decide.
    ///
    /// The Reply-Message deliberately differs from anything the model can produce, so the
    /// two denial paths are distinguishable on the wire as well as in the log.
    fn synthesised_reject(
        request: &RadiusPacket,
        protocol: &actions::RadiusProtocol,
        is_authorization: bool,
    ) -> Option<Vec<u8>> {
        if !is_authorization {
            // Accounting: stay silent. There is no "deny" to express, and an unearned
            // Accounting-Response would tell the NAS its record was stored when it was not.
            return None;
        }

        let mut attributes = vec![Attribute::text(
            packet::ATTR_REPLY_MESSAGE,
            "Access denied: no authorization decision was produced",
        )];
        for state in protocol.proxy_state() {
            attributes.push(Attribute::new(packet::ATTR_PROXY_STATE, state.clone()));
        }

        match protocol.encode_reply(packet::CODE_ACCESS_REJECT, &attributes) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                // Encoding a 1-attribute Access-Reject cannot realistically fail, but if it
                // did, silence is still a denial — never an accept.
                error!(
                    "RADIUS could not encode the fail-closed Access-Reject for id={}: {}",
                    request.identifier, e
                );
                None
            }
        }
    }

    /// Structured payload for `radius_access_request`.
    fn access_request_data(
        request: &RadiusPacket,
        peer_addr: SocketAddr,
        secret: &[u8],
    ) -> serde_json::Value {
        let has_chap = request.first(packet::ATTR_CHAP_PASSWORD).is_some();
        let has_eap = request.first(packet::ATTR_EAP_MESSAGE).is_some();

        // PAP: unhide User-Password per RFC 2865 §5.2. A password that is not valid UTF-8
        // is reported as hex rather than mangled through a lossy conversion, because a
        // model told `caf?` cannot tell whether the user typed that.
        let (auth_method, password, password_encoding) =
            match request.first(packet::ATTR_USER_PASSWORD) {
                Some(ciphertext) => {
                    match packet::decode_user_password(ciphertext, &request.authenticator, secret) {
                        Ok(bytes) => match String::from_utf8(bytes.clone()) {
                            Ok(s) => ("pap", Some(s), "utf8"),
                            Err(_) => ("pap", Some(hex::encode(&bytes)), "hex"),
                        },
                        Err(e) => {
                            warn!("RADIUS could not decode User-Password: {}", e);
                            ("pap", None, "utf8")
                        }
                    }
                }
                None if has_chap => ("chap", None, "utf8"),
                None if has_eap => ("eap", None, "utf8"),
                None => ("none", None, "utf8"),
            };

        let (state_value, state_encoding) = match request.first(packet::ATTR_STATE) {
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) if s.chars().all(|c| !c.is_control()) => (Some(s.to_string()), "utf8"),
                _ => (Some(hex::encode(bytes)), "hex"),
            },
            None => (None, "utf8"),
        };

        serde_json::json!({
            "identifier": request.identifier,
            "user_name": actions::text_attr(request, packet::ATTR_USER_NAME),
            "auth_method": auth_method,
            "password": password,
            "password_encoding": password_encoding,
            "nas_ip_address": actions::ip_attr(request, packet::ATTR_NAS_IP_ADDRESS),
            "nas_identifier": actions::text_attr(request, packet::ATTR_NAS_IDENTIFIER),
            "nas_port": actions::int_attr(request, packet::ATTR_NAS_PORT),
            "nas_port_type": actions::int_attr(request, packet::ATTR_NAS_PORT_TYPE),
            "calling_station_id": actions::text_attr(request, packet::ATTR_CALLING_STATION_ID),
            "called_station_id": actions::text_attr(request, packet::ATTR_CALLED_STATION_ID),
            "service_type": actions::int_attr(request, packet::ATTR_SERVICE_TYPE),
            "state": state_value,
            "state_encoding": state_encoding,
            "source_addr": peer_addr.to_string(),
            "attributes": actions::attributes_json(request),
        })
    }

    /// Structured payload for `radius_accounting_request`.
    fn accounting_request_data(request: &RadiusPacket, peer_addr: SocketAddr) -> serde_json::Value {
        serde_json::json!({
            "identifier": request.identifier,
            "acct_status_type": actions::int_attr(request, packet::ATTR_ACCT_STATUS_TYPE),
            "user_name": actions::text_attr(request, packet::ATTR_USER_NAME),
            "acct_session_id": actions::text_attr(request, packet::ATTR_ACCT_SESSION_ID),
            "acct_session_time": actions::int_attr(request, packet::ATTR_ACCT_SESSION_TIME),
            "acct_input_octets": actions::int_attr(request, packet::ATTR_ACCT_INPUT_OCTETS),
            "acct_output_octets": actions::int_attr(request, packet::ATTR_ACCT_OUTPUT_OCTETS),
            "nas_ip_address": actions::ip_attr(request, packet::ATTR_NAS_IP_ADDRESS),
            "source_addr": peer_addr.to_string(),
            "attributes": actions::attributes_json(request),
        })
    }
}
