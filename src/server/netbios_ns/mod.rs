//! NetBIOS Name Service server (RFC 1001 / RFC 1002).
//!
//! UDP, default port 137. The model decides which NetBIOS names exist, what they resolve to,
//! and what a node status listing says. This file owns the socket, the request decoding and —
//! most importantly — the guarantee that **no decision means no datagram**.
//!
//! # Why silence, and not an error reply
//!
//! NBNS is a caching name service. A querier that receives a POSITIVE NAME QUERY RESPONSE
//! stores the address for the TTL and uses it for every subsequent connection to that name, so
//! a fabricated answer does not merely fail once: it redirects the peer's traffic for hours.
//! NetGet therefore never synthesises an answer of its own. If the model is unreachable, or
//! returns nothing usable, or its action fails to encode, this server sends **nothing** and the
//! querier falls back exactly as it would if no NBNS server were listening — which, on a
//! broadcast query, is the normal behaviour of every node that does not hold the name.
//!
//! That is the deliberately-silent class described in the root `CLAUDE.md`, and NBNS is a
//! strong member of it: every response the protocol defines is a positive assertion about a
//! name, and the one "negative" form (§4.2.14) is still an assertion — that the name does not
//! exist — which a querier may also cache.
//!
//! The distinction the wire cannot carry therefore lives in the log. Every request is logged
//! with a `decision=` token so an operator can tell a refusal from an outage after the fact:
//! `model_answer`, `model_reject`, `model_silent`, `fail_closed_no_action`,
//! `fail_closed_llm_error`, `fail_closed_action_error`. This is the discipline `src/server/radius`
//! established; the difference is that RADIUS can express denial on the wire and NBNS cannot,
//! so here *every* fail-closed path is byte-for-byte identical to a dead server.

pub mod actions;
pub mod packet;

pub use actions::NetbiosNsProtocol;

use crate::llm::action_helper::call_llm;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use actions::{
    RequestContext, DEFAULT_TTL_SECONDS, NETBIOS_NAME_QUERY_EVENT, NETBIOS_NAME_REGISTRATION_EVENT,
    NETBIOS_NODE_STATUS_REQUEST_EVENT,
};
use packet::{NbnsRequest, NodeType};

/// How a request was disposed of.
///
/// The point of this enum is that "the model refused" and "the model could not be reached"
/// produce *identical* silence on the wire, so they must not be allowed to collapse into one
/// another in the log as well. That conflation is the OAuth2 defect recorded in the root
/// `CLAUDE.md`; here it would mean an operator cannot tell a working deny-by-default server
/// from a broken one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The model produced a response and it went on the wire.
    ModelAnswer,
    /// The model explicitly refused with `send_netbios_negative_response`.
    ModelReject,
    /// The model explicitly chose silence with `no_response`.
    ModelSilent,
    /// The model returned no actions at all. Nothing is sent.
    FailClosedNoAction,
    /// The LLM call itself failed. Nothing is sent.
    FailClosedLlmError,
    /// The model's action could not be encoded. Nothing is sent.
    FailClosedActionError,
}

impl Decision {
    /// Stable, grep-able token. `decision=fail_closed_` finds every request the model did not
    /// actually answer.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::ModelAnswer => "model_answer",
            Decision::ModelReject => "model_reject",
            Decision::ModelSilent => "model_silent",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
            Decision::FailClosedActionError => "fail_closed_action_error",
        }
    }

    /// True when the server, not the model, decided to say nothing.
    pub fn is_fail_closed(&self) -> bool {
        matches!(
            self,
            Decision::FailClosedNoAction
                | Decision::FailClosedLlmError
                | Decision::FailClosedActionError
        )
    }
}

pub struct NetbiosNsServer;

impl NetbiosNsServer {
    /// Bind the socket and start serving.
    ///
    /// Awaits the bind and returns `Err` on failure so `server_startup` reports
    /// `ServerStatus::Error` rather than leaving a server that claims to be up while holding
    /// no socket.
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

        // Both parameters are optional and both are really read; a declared knob that turns
        // nothing is the `startup_param_drift_test` defect.
        let (default_ttl, default_node_type) = match &startup_params {
            None => (DEFAULT_TTL_SECONDS, NodeType::B),
            Some(params) => {
                let ttl = match params.get_optional_u64("default_ttl")? {
                    None => DEFAULT_TTL_SECONDS,
                    Some(n) => u32::try_from(n)
                        .context("default_ttl exceeds 32 bits; it is a number of seconds")?,
                };
                let node_type = match params.get_optional_string("node_type")? {
                    None => NodeType::B,
                    Some(s) => NodeType::parse(&s).with_context(|| {
                        format!("node_type must be one of b, p, m, h — got '{}'", s)
                    })?,
                };
                (ttl, node_type)
            }
        };

        let socket = Arc::new(
            UdpSocket::bind(listen_addr)
                .await
                .with_context(|| format!("NetBIOS-NS failed to bind {}", listen_addr))?,
        );
        let local_addr = socket.local_addr()?;

        Log::new(Some(&status_tx)).info(format!(
            "NetBIOS-NS server listening on {} (default_ttl={}s, node_type={})",
            local_addr,
            default_ttl,
            default_node_type.as_str()
        ));

        let task_registrar = state.clone();
        let accept_handle = tokio::spawn(async move {
            // One octet over the RFC 1002 §4.1 limit, so an over-long datagram is detected
            // rather than silently truncated into something that happens to parse.
            let mut buffer = vec![0u8; packet::MAX_DATAGRAM + 1];

            loop {
                let (n, peer_addr) = match socket.recv_from(&mut buffer).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("NetBIOS-NS receive error: {}", e));
                        break;
                    }
                };

                let data = buffer[..n].to_vec();
                trace!(
                    "NetBIOS-NS {} bytes from {}: {}",
                    n,
                    peer_addr,
                    hex::encode(&data)
                );

                let request = match packet::parse_request(&data) {
                    Ok(r) => r,
                    Err(e) => {
                        // A datagram we cannot parse gets no reply. RFC 1002 defines an
                        // FMT_ERR rcode, but building any response needs the question's NAME
                        // field, which is exactly the part that failed to parse — and
                        // answering unparsed input from a spoofable transport is how a
                        // reflector is built.
                        Log::new(Some(&status_tx)).warn(format!(
                            "NetBIOS-NS dropped datagram from {}: {}",
                            peer_addr, e
                        ));
                        continue;
                    }
                };

                Self::record_connection(&state, server_id, local_addr, peer_addr, n).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());

                debug!(
                    "NetBIOS-NS {} {}<{:#04x}> type={} from {} trn_id={:#06x}",
                    packet::opcode_name(request.header.opcode()),
                    request.question_name.name,
                    request.question_name.suffix,
                    packet::qtype_name(request.qtype),
                    peer_addr,
                    request.header.trn_id
                );

                let llm = llm_client.clone();
                let st = state.clone();
                let tx = status_tx.clone();
                let sock = socket.clone();

                let handle = tokio::spawn(async move {
                    Self::handle_request(
                        request,
                        peer_addr,
                        sock,
                        llm,
                        st,
                        tx,
                        server_id,
                        default_ttl,
                        default_node_type,
                    )
                    .await;
                });

                // Every spawned task is registered, not just the accept loop: aborting a task
                // does not abort what it spawned, so an in-flight LLM call would otherwise
                // outlive `stop_server` and could still write to the socket. `register_server_task`
                // prunes finished handles on each call, so this does not grow without bound.
                state.register_server_task(server_id, handle).await;
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Record the datagram against a per-remote-address connection entry.
    ///
    /// The protocol declares `.connectionless()`, so these entries are reaped by
    /// `AppState::cleanup_old_connections` after ten idle seconds — nothing else would ever
    /// close them.
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

    /// Raise the event, ask the model, then apply the silence rule.
    #[allow(clippy::too_many_arguments)]
    async fn handle_request(
        request: NbnsRequest,
        peer_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        llm_client: crate::llm::ollama_client::OllamaClient,
        state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
        default_ttl: u32,
        default_node_type: NodeType,
    ) {
        let header = request.header;
        let opcode = header.opcode();

        let Some(event) = Self::event_for(&request, peer_addr) else {
            // Releases, refreshes and anything else this server does not serve are dropped.
            // Answering an opcode we have no event for would mean answering without ever
            // asking the model.
            debug!(
                "NetBIOS-NS ignoring {} type={} from {}: not a request this server serves",
                packet::opcode_name(opcode),
                packet::qtype_name(request.qtype),
                peer_addr
            );
            return;
        };

        let protocol = NetbiosNsProtocol::for_request(RequestContext {
            trn_id: header.trn_id,
            opcode,
            recursion_desired: header.recursion_desired(),
            name_field: request.question_name.raw.clone(),
            default_ttl,
            default_node_type,
        });

        let llm_outcome = call_llm(&llm_client, &state, server_id, None, &event, &protocol).await;

        let (decision, reply) = Self::decide(llm_outcome, &status_tx, peer_addr);

        let summary = format!(
            "NetBIOS-NS {} {}<{:#04x}> from {} decision={}",
            packet::opcode_name(opcode),
            request.question_name.name,
            request.question_name.suffix,
            peer_addr,
            decision.as_str()
        );
        let log = Log::new(Some(&status_tx));
        if decision.is_fail_closed() {
            // Loud, because on the wire this is indistinguishable from the server being down.
            log.error(format!(
                "{} (nothing sent: no usable answer was produced, and a fabricated NetBIOS \
                 answer would be cached by the querier)",
                summary
            ));
        } else {
            log.info(&summary);
        }

        let Some(reply) = reply else {
            return;
        };

        match socket.send_to(&reply, peer_addr).await {
            Ok(sent) => {
                trace!(
                    "NetBIOS-NS sent {} bytes to {}: {}",
                    sent,
                    peer_addr,
                    hex::encode(&reply)
                );
            }
            Err(e) => {
                Log::new(Some(&status_tx)).error(format!(
                    "NetBIOS-NS failed to reply to {}: {}",
                    peer_addr, e
                ));
            }
        }
    }

    /// Which event, if any, a decoded request raises.
    fn event_for(request: &NbnsRequest, peer_addr: SocketAddr) -> Option<Event> {
        let name = &request.question_name;
        let opcode = request.header.opcode();

        match (opcode, request.qtype) {
            (packet::OPCODE_QUERY, packet::QTYPE_NB) => Some(Event::new(
                &NETBIOS_NAME_QUERY_EVENT,
                serde_json::json!({
                    "name": name.name,
                    "suffix": name.suffix,
                    "question_type": packet::qtype_name(request.qtype),
                    "source_address": peer_addr.to_string(),
                    "transaction_id": request.header.trn_id,
                }),
            )),
            (packet::OPCODE_QUERY, packet::QTYPE_NBSTAT) => Some(Event::new(
                &NETBIOS_NODE_STATUS_REQUEST_EVENT,
                serde_json::json!({
                    "name": name.name,
                    "suffix": name.suffix,
                    "source_address": peer_addr.to_string(),
                    "transaction_id": request.header.trn_id,
                }),
            )),
            (packet::OPCODE_REGISTRATION, _) => {
                let claimed = request.addresses.first();
                Some(Event::new(
                    &NETBIOS_NAME_REGISTRATION_EVENT,
                    serde_json::json!({
                        "name": name.name,
                        "suffix": name.suffix,
                        "address": claimed.map(|a| a.address.to_string()),
                        "group": claimed.map(|a| a.is_group()).unwrap_or(false),
                        "source_address": peer_addr.to_string(),
                        "transaction_id": request.header.trn_id,
                    }),
                ))
            }
            _ => None,
        }
    }

    /// **The silence rule.**
    ///
    /// Returns what happened and the bytes to send, if any. There is no branch that
    /// synthesises a response: every `Vec<u8>` returned here came out of an action the model
    /// named.
    fn decide(
        llm_outcome: Result<crate::llm::ExecutionResult>,
        status_tx: &mpsc::UnboundedSender<String>,
        peer_addr: SocketAddr,
    ) -> (Decision, Option<Vec<u8>>) {
        let log = Log::new(Some(status_tx));

        let execution = match llm_outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }
                result
            }
            Err(e) => {
                // The category is worth having even though the wire cannot carry it: an
                // overloaded backend and a dead one call for different operator responses.
                // The error itself is logged here and never leaves the process.
                let category = match crate::utils::wire_failure::WireFailure::classify(&e) {
                    crate::utils::wire_failure::WireFailure::Overloaded => "overloaded",
                    crate::utils::wire_failure::WireFailure::Unavailable => "unavailable",
                };
                log.error(format!(
                    "NetBIOS-NS LLM call failed for {} (category={}): {}",
                    peer_addr, category, e
                ));
                return (Decision::FailClosedLlmError, None);
            }
        };

        let action_types: Vec<&str> = execution
            .raw_actions
            .iter()
            .filter_map(|a| a.get("type").and_then(|v| v.as_str()))
            .collect();

        let mut outputs: Vec<Vec<u8>> = Vec::new();
        for result in &execution.protocol_results {
            outputs.extend(result.get_all_output());
        }

        if outputs.len() > 1 {
            warn!(
                "NetBIOS-NS ignored {} extra response(s) for {}; one request gets one datagram",
                outputs.len() - 1,
                peer_addr
            );
        }

        if let Some(bytes) = outputs.into_iter().next() {
            let decision = if action_types.contains(&"send_netbios_negative_response") {
                Decision::ModelReject
            } else {
                Decision::ModelAnswer
            };
            return (decision, Some(bytes));
        }

        // Nothing to send. Which of the three silences is it?
        if action_types.contains(&"no_response") {
            // The model was asked and chose to say nothing. That is a real answer here, and
            // must not be reported as a failure.
            return (Decision::ModelSilent, None);
        }
        if execution.raw_actions.is_empty() {
            return (Decision::FailClosedNoAction, None);
        }
        (Decision::FailClosedActionError, None)
    }
}
