//! HSRP (Hot Standby Router Protocol) speaker — RFC 2281 for v1, Cisco's TLV format for v2.
//!
//! HSRP is a first-hop redundancy protocol: a group of routers share one virtual IP, elect one
//! of themselves Active, and that one answers for the address every host on the segment has
//! configured as its default gateway. Every message the protocol defines — Hello, Coup,
//! Resign — is a **positive claim about who owns that address**.
//!
//! That single fact decides the whole design of this file:
//!
//! * **An LLM failure produces SILENCE.** HSRP is in the deliberately-silent class the root
//!   `CLAUDE.md` catalogues, and its case is one of the strongest in it. There is no error
//!   frame to send — the protocol has no negative message at all — and the only thing this
//!   server *could* emit is a Hello, which asserts gateway ownership. A fabricated Hello during
//!   a backend outage can win an election NetGet cannot serve, and every host on the link then
//!   sends its off-subnet traffic into a black hole. So on any failure path, zero bytes go out,
//!   and the reason goes in the log instead, exactly as `src/server/radius/` does.
//! * **There is no election state machine here, deliberately.** A real HSRP router runs timers
//!   and keeps advertising on its own. This one advertises only in reply to something that
//!   actually arrived, because an autonomous Active router that keeps announcing after the
//!   model stops answering is precisely the fail-open shape this repo forbids.
//!
//! **The model chooses the priority and may send a Coup.** Winning makes NetGet the active
//! gateway for the segment. See `src/server/hsrp/CLAUDE.md`.

pub mod actions;
pub mod codec;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use actions::{event_for_opcode, HsrpProtocol, HSRP_V1_GROUP, HSRP_V2_GROUP, HSRP_V6_GROUP};
use anyhow::{Context, Result};
use codec::{HsrpMessage, HsrpVersion};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// Largest datagram this speaker will read.
///
/// An HSRPv1 packet is 20 bytes and a v2 one with both authentication TLVs is under 100, so
/// this is pure headroom for unknown TLVs while bounding a hostile datagram.
const MAX_DATAGRAM_LEN: usize = 4096;

/// How this speaker arrived at what it did (or did not) put on the wire.
///
/// **The point of this enum is that "the model declined to join the election" and "the model
/// could not be reached" produce identical bytes — none — and must never be identical in the
/// log.** That collapse is the OAuth2 failure the root `CLAUDE.md` records; here it would hide
/// a total backend outage as protocol-correct quiet, indefinitely, because a silent HSRP
/// speaker is completely normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The model advertised. Bytes went out.
    ModelResponse,
    /// The model chose `no_advertisement`: it looked at the election and stayed out of it.
    /// A deliberate answer, and the safe one.
    ModelSilent,
    /// The model returned no usable action at all.
    FailClosedNoAction,
    /// The model produced actions but none of them encoded — a bad state name, a priority that
    /// does not fit v1's single byte, an MD5 TLV that cannot be generated.
    FailClosedActionError,
    /// The LLM call itself failed — outage, overload, unusable output.
    FailClosedLlmError,
}

impl Decision {
    /// Stable, grep-able token. An operator looking for advertisements the model did not
    /// actually authorise greps `decision=fail_closed_`.
    ///
    /// **There is deliberately no `model_reject` token here, unlike `radius`.** RADIUS can
    /// distinguish a refusal from silence because it has an Access-Reject packet to send. HSRP
    /// has no negative message of any kind: the only way a speaker declines to participate is
    /// by not advertising. So "the model refused" and "the model chose to stay out" are the
    /// same act, and `model_silent` is it. What must stay distinguishable — and does — is that
    /// act versus every `fail_closed_*`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::ModelResponse => "model_response",
            Decision::ModelSilent => "model_silent",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedActionError => "fail_closed_action_error",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
        }
    }

    /// True when the *server*, not the model, chose to say nothing.
    pub fn is_fail_closed(&self) -> bool {
        matches!(
            self,
            Decision::FailClosedNoAction
                | Decision::FailClosedActionError
                | Decision::FailClosedLlmError
        )
    }
}

/// Everything handling one datagram needs, gathered so the handler is not a seven-argument
/// function.
#[derive(Clone)]
struct Speaker {
    llm_client: OllamaClient,
    state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: ServerId,
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    /// The version this instance was started for. Reported to the model on every event; it
    /// also chose which multicast group was joined.
    configured_version: HsrpVersion,
}

pub struct HsrpServer;

impl HsrpServer {
    /// Bind the speaker and start listening.
    ///
    /// Returns `Err` if the UDP socket cannot be bound, so `server_startup` marks the instance
    /// `ServerStatus::Error` rather than showing a speaker that is not listening. The multicast
    /// join is best-effort by design — see `join_hsrp_group`.
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

        // Every accessor is fallible because the values come from the model or an MCP client.
        // Propagate with `?` so an undeclared key or a wrong type produces a clean startup
        // error naming the key, rather than a panic in the spawning task.
        let (version_param, join_multicast, multicast_interface) = match startup_params {
            Some(params) => (
                params.get_optional_u64("version")?,
                params.get_optional_bool("join_multicast")?.unwrap_or(true),
                params.get_optional_string("multicast_interface")?,
            ),
            None => (None, true, None),
        };
        let configured_version = match version_param {
            Some(value) => HsrpVersion::from_number(value)?,
            None => HsrpVersion::V1,
        };

        let socket = Arc::new(
            UdpSocket::bind(listen_addr)
                .await
                .with_context(|| format!("HSRP failed to bind UDP {listen_addr}"))?,
        );
        let local_addr = socket
            .local_addr()
            .context("HSRP could not read its own UDP address")?;

        let log = Log::new(Some(&status_tx));
        info!(
            "HSRP speaker listening on UDP {local_addr}, configured for HSRPv{}",
            configured_version.as_number()
        );
        log.info(format!(
            "HSRP speaker listening on {local_addr} (configured for HSRPv{})",
            configured_version.as_number()
        ));

        if join_multicast {
            join_hsrp_group(
                &socket,
                local_addr,
                configured_version,
                multicast_interface.as_deref(),
                &status_tx,
            );
        } else {
            debug!("HSRP multicast join skipped (join_multicast=false)");
        }

        let speaker = Speaker {
            llm_client,
            state: state.clone(),
            status_tx: status_tx.clone(),
            server_id,
            local_addr,
            socket: socket.clone(),
            configured_version,
        };

        let handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; MAX_DATAGRAM_LEN];
            loop {
                let (n, peer_addr) = match speaker.socket.recv_from(&mut buffer).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        Log::new(Some(&speaker.status_tx))
                            .error(format!("HSRP UDP receive error: {e}"));
                        break;
                    }
                };

                let data = buffer[..n].to_vec();
                let s = speaker.clone();
                let task = tokio::spawn(async move {
                    s.handle_datagram(data, peer_addr).await;
                });
                // Register even these short-lived tasks: `register_server_task` prunes finished
                // ones on every call so the list stays bounded, and `stop_server` then cancels
                // an in-flight LLM call instead of letting it advertise after the socket is
                // gone. Aborting the receive loop alone does not abort what it spawned.
                speaker.state.register_server_task(server_id, task).await;
            }
        });
        state.register_server_task(server_id, handle).await;

        Ok(local_addr)
    }
}

impl Speaker {
    /// Parse one datagram, raise its event, ask the model, decide, and — only on
    /// `ModelResponse` — send.
    async fn handle_datagram(&self, data: Vec<u8>, peer_addr: SocketAddr) {
        let log = Log::new(Some(&self.status_tx));
        trace!(
            "HSRP {} bytes from {peer_addr}: {}",
            data.len(),
            hex::encode(&data)
        );

        let message = match codec::decode(&data) {
            Ok(m) => m,
            Err(e) => {
                // A multicast group carries other protocols' traffic and plain noise. An
                // unparseable datagram is normal background, not an error worth the status
                // stream, and it certainly is not a reason to advertise anything.
                debug!("HSRP dropped unparseable datagram from {peer_addr}: {e}");
                return;
            }
        };

        let connection_id = self.record_connection(peer_addr, data.len()).await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());

        let event_type = event_for_opcode(message.opcode);
        let event = Event::new(event_type, self.event_data(&message, peer_addr));

        debug!(
            "HSRPv{} {} group={} state={} priority={} vip={} from {peer_addr}",
            message.version.as_number(),
            message.opcode.as_str(),
            message.group,
            message.state.as_str(),
            message.priority,
            message.virtual_ip
        );

        let protocol = HsrpProtocol::new();
        let llm_outcome = call_llm(
            &self.llm_client,
            &self.state,
            self.server_id,
            None,
            &event,
            &protocol,
        )
        .await;

        let (decision, reply) = self.decide(llm_outcome, peer_addr);

        // Log the decision before acting on it. `model_silent` and every `fail_closed_*`
        // produce the same zero bytes, so this line is the only place the difference survives.
        // `opcode=` is included so an operator can grep the inbound side too.
        let summary = format!(
            "HSRPv{} {} group={} from {peer_addr} decision={}",
            message.version.as_number(),
            message.opcode.as_str(),
            message.group,
            decision.as_str()
        );
        if decision.is_fail_closed() {
            error!(
                "{summary} (nothing sent: no usable answer was produced. An HSRP Hello asserts \
                 ownership of the segment's gateway address, so a guessed one can win an \
                 election NetGet cannot serve and black-hole the link)"
            );
            log.error(format!(
                "{summary} - staying silent rather than guessing a gateway claim"
            ));
        } else {
            log.info(&summary);
        }

        let Some(reply) = reply else {
            return;
        };

        // Be loud when the model has just claimed the gateway. This is the one thing in the
        // protocol that changes where a whole segment's traffic goes, and it must never be
        // something an operator has to reconstruct from a packet capture afterwards.
        if let Ok(sent_message) = codec::decode(&reply) {
            if sent_message.opcode == codec::Opcode::Coup || sent_message.state.claims_gateway() {
                warn!(
                    "HSRP is claiming the gateway role: opcode={} state={} priority={} group={} \
                     vip={} to {peer_addr}. If this election is won, hosts on the segment will \
                     send their off-subnet traffic to NetGet, which does not forward it.",
                    sent_message.opcode.as_str(),
                    sent_message.state.as_str(),
                    sent_message.priority,
                    sent_message.group,
                    sent_message.virtual_ip
                );
                log.warn(format!(
                    "HSRP {} claiming the gateway role for {} (priority {}) - NetGet does not \
                     forward traffic",
                    sent_message.opcode.as_str(),
                    sent_message.virtual_ip,
                    sent_message.priority
                ));
            }
        }

        // Unicast back to whoever spoke to us, rather than to the multicast group.
        //
        // Two reasons, and the second is the load-bearing one. Loopback carries no multicast
        // route, so `send_to(224.0.0.2:1985)` from a socket bound to 127.0.0.1 fails with
        // EADDRNOTAVAIL and this speaker would be unobservable in any local test. And a reply
        // aimed at the peer that prompted it is the only advertisement this server has any
        // business sending: it never speaks unprompted, so it never has a reason to address
        // the whole group. `CLAUDE.md` records this as a deviation from a real HSRP router.
        match self.socket.send_to(&reply, peer_addr).await {
            Ok(sent) => {
                trace!(
                    "HSRP sent {sent} bytes to {peer_addr}: {}",
                    hex::encode(&reply)
                );
                self.count_sent_for(connection_id, sent).await;
                log.debug(format!("HSRP sent {sent} bytes to {peer_addr}"));
            }
            Err(e) => {
                Log::new(Some(&self.status_tx))
                    .error(format!("HSRP failed to reply to {peer_addr}: {e}"));
            }
        }
    }

    /// Everything the model is told about an advertisement. Structured fields only — no raw
    /// bytes, no base64, no version-dependent state numbers.
    fn event_data(&self, message: &HsrpMessage, peer_addr: SocketAddr) -> serde_json::Value {
        serde_json::json!({
            "version": message.version.as_number(),
            "opcode": message.opcode.as_str(),
            "state": message.state.as_str(),
            "priority": message.priority,
            "group": message.group,
            "hellotime": message.hellotime_secs,
            "holdtime": message.holdtime_secs,
            "virtual_ip": message.virtual_ip.to_string(),
            "auth_data": message.auth_data,
            "identifier": message.identifier_str(),
            "md5_auth": message.md5_auth.map(|md5| serde_json::json!({
                "algorithm": md5.algorithm,
                "flags": md5.flags,
                "sender_address": md5.sender_address.to_string(),
                "key_id": md5.key_id,
            })),
            "source_address": peer_addr.to_string(),
            "configured_version": self.configured_version.as_number(),
        })
    }

    /// **The fail-closed rule: nothing usable means nothing on the wire.**
    ///
    /// Most UDP protocols in this repo answer a backend failure with an error frame, because
    /// silence costs the peer its own timeout. HSRP cannot: it has no error frame, and its
    /// success frame is a claim of gateway ownership. A peer that hears nothing simply keeps
    /// its own view of the election, which is the correct outcome when NetGet has nothing
    /// authorised to say.
    fn decide(
        &self,
        llm_outcome: Result<crate::llm::ExecutionResult>,
        peer_addr: SocketAddr,
    ) -> (Decision, Option<Vec<u8>>) {
        let log = Log::new(Some(&self.status_tx));

        let execution = match llm_outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }
                result
            }
            Err(e) => {
                // Classified for the log and never rendered onto the wire. There is no wire
                // text here at all — HSRP is fixed binary — but the category is still what
                // tells an operator whether to wait or to look.
                let category = match crate::utils::wire_failure::WireFailure::classify(&e) {
                    crate::utils::wire_failure::WireFailure::Overloaded => "overloaded",
                    crate::utils::wire_failure::WireFailure::Unavailable => "unavailable",
                };
                error!("HSRP LLM call failed for {peer_addr} (category={category}): {e}");
                return (Decision::FailClosedLlmError, None);
            }
        };

        // Take the first advertisement that encoded. Extra ones are a model error: two
        // advertisements for one event look to the segment like two speakers, and if they
        // disagree about priority they fight each other's election.
        let mut chosen: Option<Vec<u8>> = None;
        let mut extra = 0usize;
        for protocol_result in &execution.protocol_results {
            for output in protocol_result.get_all_output() {
                if chosen.is_none() {
                    chosen = Some(output);
                } else {
                    extra += 1;
                }
            }
        }
        if extra > 0 {
            warn!(
                "HSRP ignored {extra} extra advertisement(s) for the datagram from {peer_addr}; \
                 one event gets one advertisement"
            );
        }

        if let Some(bytes) = chosen {
            return (Decision::ModelResponse, Some(bytes));
        }

        // No bytes. Three different reasons, and they must stay apart.
        if execution.raw_actions.is_empty() {
            return (Decision::FailClosedNoAction, None);
        }
        if execution
            .raw_actions
            .iter()
            .any(|a| a.get("type").and_then(|t| t.as_str()) == Some("no_advertisement"))
        {
            // The model looked at the election and stayed out of it. This is the protocol
            // working, and it is the safe answer.
            return (Decision::ModelSilent, None);
        }
        (Decision::FailClosedActionError, None)
    }

    /// Record the datagram as a connection entry so the dashboard shows the neighbour.
    ///
    /// Nothing ever closes these — HSRP has no connection — which is why `metadata()` declares
    /// `.connectionless()`: the 10-second idle sweep is what reaps them.
    async fn record_connection(&self, peer_addr: SocketAddr, bytes: usize) -> ConnectionId {
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let connection_id = ConnectionId::new(self.state.get_next_unified_id().await);
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr: peer_addr,
            local_addr: self.local_addr,
            bytes_sent: 0,
            bytes_received: bytes as u64,
            packets_sent: 0,
            packets_received: 1,
            last_activity: now,
            status: ConnectionStatus::Active,
            status_changed_at: now,
            protocol_info: ProtocolConnectionInfo::empty(),
        };
        self.state
            .add_connection_to_server(self.server_id, conn_state)
            .await;
        connection_id
    }

    /// Keep the rail's `↑` counter in step with what actually went out.
    async fn count_sent_for(&self, connection_id: ConnectionId, bytes: usize) {
        self.state
            .update_connection_stats(
                self.server_id,
                connection_id,
                None,
                Some(bytes as u64),
                None,
                Some(1),
            )
            .await;
    }
}

/// Join the HSRP multicast group, non-fatally.
///
/// **Deliberately best-effort, and this is measured rather than assumed.** On macOS, a socket
/// bound to `127.0.0.1` can *join* a group successfully and still fail to *send* to one with
/// `EADDRNOTAVAIL`, because loopback carries no multicast route. A speaker that refused to
/// start over a join would be unusable for exactly the local testing this repo does — while
/// still being perfectly able to handle a datagram sent straight to its port, which is how the
/// e2e suite drives it. The failure is logged on both channels so it is never *silently*
/// absent.
///
/// The group depends on the configured version, not just the address family: HSRPv2 moved off
/// the all-routers group (`224.0.0.2`) onto `224.0.0.102` precisely so v1 speakers do not see
/// its TLVs.
fn join_hsrp_group(
    socket: &UdpSocket,
    local_addr: SocketAddr,
    configured_version: HsrpVersion,
    multicast_interface: Option<&str>,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    let log = Log::new(Some(status_tx));

    match local_addr.ip() {
        IpAddr::V4(_) => {
            let group = match configured_version {
                HsrpVersion::V1 => HSRP_V1_GROUP,
                HsrpVersion::V2 => HSRP_V2_GROUP,
            };
            let interface = match multicast_interface {
                Some(value) => match value.parse::<Ipv4Addr>() {
                    Ok(ip) => ip,
                    Err(e) => {
                        warn!(
                            "HSRP multicast_interface '{value}' is not an IPv4 address ({e}); \
                             letting the host choose"
                        );
                        Ipv4Addr::UNSPECIFIED
                    }
                },
                None => Ipv4Addr::UNSPECIFIED,
            };
            match socket.join_multicast_v4(group, interface) {
                Ok(()) => {
                    info!("HSRP joined {group} on interface {interface}");
                    log.info(format!("HSRP joined multicast group {group}"));
                }
                Err(e) => {
                    warn!(
                        "HSRP could not join {group} on interface {interface}: {e}; datagrams \
                         sent directly to this port are still handled"
                    );
                    log.warn(format!(
                        "HSRP multicast join failed ({e}); handling direct datagrams only"
                    ));
                }
            }
        }
        IpAddr::V6(_) => {
            // HSRPv2 over IPv6 uses FF02::66 on port 2029. Interface index 0 lets the kernel
            // pick; there is no address-based selector for IPv6 joins, so `multicast_interface`
            // (an IPv4 address) has no meaning here.
            match socket.join_multicast_v6(&HSRP_V6_GROUP, 0) {
                Ok(()) => {
                    info!("HSRP joined {HSRP_V6_GROUP}");
                    log.info(format!("HSRP joined multicast group {HSRP_V6_GROUP}"));
                }
                Err(e) => {
                    warn!(
                        "HSRP could not join {HSRP_V6_GROUP}: {e}; datagrams sent directly to \
                         this port are still handled"
                    );
                    log.warn(format!(
                        "HSRP multicast join failed ({e}); handling direct datagrams only"
                    ));
                }
            }
        }
    }
}
