//! EAPOL / 802.1X authenticator — the switch-port role.
//!
//! `codec.rs` is the pure wire format; this file is the transport and the session loop that
//! sits on top of it, and — most importantly — the guarantee that **no decision means
//! denial**. See `src/server/eapol/CLAUDE.md` for the design rationale, the exact list of
//! things this server deliberately does not implement, and how it pairs with `radius`.
//!
//! # Two transports, one handler
//!
//! `transport: "raw"` (default) captures and injects EtherType 0x888E frames through libpcap
//! and needs packet-capture privilege. `transport: "udp"` carries the identical EAPOL frames
//! over a UDP socket, each datagram prefixed with the supplicant's six-octet MAC address, so
//! the whole event -> LLM -> action -> frame path can be exercised unprivileged. Both funnel
//! into [`EapolServer::handle_eapol`]; there is no second copy of the state machine.
//!
//! # The fail-closed rule
//!
//! `EAP-Success` is an admission decision. [`EapolServer::decide`] is the only place a reply
//! is chosen, and it can produce Success bytes in exactly one way: by *recognising* them in
//! output the action executor already built from an explicit `send_eap_success`. Every other
//! exit — an LLM error, an empty action list, an action that failed to encode, a Success on a
//! session with no identity — calls [`codec::eapol_eap_failure_frame`] directly.

pub mod actions;
pub mod codec;

pub use actions::EapolProtocol;

use crate::llm::action_helper::call_llm;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

use actions::{
    EAPOL_IDENTITY_RESPONSE_EVENT, EAPOL_LOGOFF_EVENT, EAPOL_METHOD_RESPONSE_EVENT,
    EAPOL_START_EVENT,
};

/// Largest datagram the UDP transport will read. An EAPOL frame is tiny; this is generous
/// enough for a fragmented EAP-TLS record and small enough that a sender cannot make the
/// server allocate on demand.
const MAX_FRAME_LEN: usize = 4096;

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// How a reply was arrived at.
///
/// This exists so the log can never conflate "the model denied" with "the model said
/// nothing" — the OAuth2 failure mode, where those two collapsed into one another and silence
/// became approval. On the wire both are an EAP-Failure, which is correct; the distinction
/// belongs here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The model returned `send_eap_success`: the port is authorized.
    ModelAdmit,
    /// The model returned `send_eap_failure`: the port stays unauthorized.
    ModelReject,
    /// The model asked for an identity, a method or a notification. Nothing is granted.
    ModelChallenge,
    /// The model returned no usable action. The server denies.
    FailClosedNoAction,
    /// The LLM call itself failed. The server denies.
    FailClosedLlmError,
    /// The model's action could not be encoded. The server denies.
    FailClosedActionError,
    /// The model asked to admit a session that has produced no identity. The server denies.
    ///
    /// `execute_action` already refuses this, so reaching it means a second line of defence
    /// fired and something upstream is wrong. It gets its own token so that shows up in the
    /// log rather than hiding inside `fail_closed_action_error`.
    FailClosedNoIdentity,
}

impl Decision {
    /// Stable, grep-able token. One per path, never reused.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::ModelAdmit => "model_admit",
            Decision::ModelReject => "model_reject",
            Decision::ModelChallenge => "model_challenge",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
            Decision::FailClosedActionError => "fail_closed_action_error",
            Decision::FailClosedNoIdentity => "fail_closed_no_identity",
        }
    }

    /// True when the server, not the model, chose to deny.
    pub fn is_fail_closed(&self) -> bool {
        matches!(
            self,
            Decision::FailClosedNoAction
                | Decision::FailClosedLlmError
                | Decision::FailClosedActionError
                | Decision::FailClosedNoIdentity
        )
    }
}

/// Classify a frame the executor produced, by the EAP code inside it.
///
/// Returns `None` for anything that is not a well-formed EAPOL-wrapped EAP packet, which is
/// treated as "no usable answer" and therefore denies.
pub fn classify_frame(frame: &[u8]) -> Option<Decision> {
    let eapol = codec::EapolFrame::decode(frame).ok()?;
    if eapol.packet_type != codec::EAPOL_TYPE_EAP_PACKET {
        return None;
    }
    let eap = codec::EapPacket::decode(&eapol.body).ok()?;
    match eap.code {
        codec::EAP_CODE_SUCCESS => Some(Decision::ModelAdmit),
        codec::EAP_CODE_FAILURE => Some(Decision::ModelReject),
        codec::EAP_CODE_REQUEST => Some(Decision::ModelChallenge),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// An MD5-Challenge NetGet issued and is waiting on.
#[derive(Clone, Debug)]
struct PendingMd5 {
    /// EAP identifier the challenge went out with; the response must echo it.
    identifier: u8,
    /// The challenge NetGet generated. Never chosen by the model.
    challenge: Vec<u8>,
    /// Password the model said this identity should hold, if it supplied one. `None` means
    /// nothing can be verified, and the event says so in as many words.
    expected_password: Option<String>,
}

/// Per-supplicant state, keyed by source MAC.
#[derive(Debug)]
struct Session {
    connection_id: ConnectionId,
    /// Identity claimed by an `EAP-Response/Identity`, if one has arrived.
    identity: Option<String>,
    /// Identifier of the last EAP-Request NetGet sent. RFC 3748 §4.1: a Response that does
    /// not echo it is silently discarded.
    last_request_identifier: Option<u8>,
    /// Next identifier to allocate for an outbound Request.
    next_identifier: u8,
    pending_md5: Option<PendingMd5>,
}

type Sessions = Arc<Mutex<HashMap<[u8; 6], Session>>>;

/// Lock helper that survives a poisoned mutex.
///
/// A panic in one supplicant's task must not take the authenticator down, and the state
/// behind this lock is per-supplicant bookkeeping rather than a security invariant: nothing
/// here can turn a denial into an admission, because that decision lives entirely in
/// [`EapolServer::decide`].
///
/// Note the type: this is a `std::sync::Mutex`, so its guard is `!Send` and holding one
/// across an `.await` in a spawned task is a compile error rather than a review finding.
fn sessions_lock(sessions: &Sessions) -> std::sync::MutexGuard<'_, HashMap<[u8; 6], Session>> {
    sessions.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// How a reply reaches the supplicant. Cloned into every per-frame task.
#[derive(Clone)]
enum Transmit {
    /// Real EAPOL: build an Ethernet frame and hand it to the pcap injection thread.
    Raw {
        frames: mpsc::UnboundedSender<Vec<u8>>,
        local_mac: [u8; 6],
    },
    /// Test transport: `[supplicant MAC (6)][EAPOL frame]` back to the datagram's sender.
    Udp {
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
    },
}

impl Transmit {
    async fn send(&self, supplicant_mac: [u8; 6], eapol_frame: &[u8]) -> Result<usize> {
        match self {
            Transmit::Raw { frames, local_mac } => {
                let ethernet = codec::build_ethernet_frame(
                    supplicant_mac,
                    *local_mac,
                    codec::ETHERTYPE_EAPOL,
                    eapol_frame,
                );
                let len = ethernet.len();
                frames
                    .send(ethernet)
                    .map_err(|_| anyhow::anyhow!("EAPOL injection thread has exited"))?;
                Ok(len)
            }
            Transmit::Udp { socket, peer } => {
                let mut datagram = Vec::with_capacity(6 + eapol_frame.len());
                datagram.extend_from_slice(&supplicant_mac);
                datagram.extend_from_slice(eapol_frame);
                Ok(socket.send_to(&datagram, peer).await?)
            }
        }
    }
}

/// Everything a per-frame task needs that is not the frame itself.
#[derive(Clone)]
struct ServerCx {
    llm_client: crate::llm::ollama_client::OllamaClient,
    state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: ServerId,
    sessions: Sessions,
    local_addr: SocketAddr,
    /// Version forced by the `eapol_version` startup parameter, if any. Without it, replies
    /// echo whatever version the supplicant used.
    forced_version: Option<u8>,
}

/// The result of classifying one received frame: which event it raises, what the model is
/// told, and everything the reply needs. Owned — no lock is held across the LLM call.
struct Prepared {
    event_id: &'static str,
    event_data: serde_json::Value,
    request: actions::RequestContext,
    identity: Option<String>,
    /// True only for EAPOL-Logoff, where the port is already de-authorized and there is
    /// nothing left to deny.
    silence_is_safe: bool,
    /// True when the session ends with this frame regardless of the answer.
    terminal: bool,
}

/// What [`EapolServer::decide`] concluded: the decision, the frame to send, and the one field
/// of the model's action that never reaches the wire.
struct Answer {
    decision: Decision,
    frame: Option<Vec<u8>>,
    /// `expected_password` from the model's own `send_eap_request_method`, if it supplied
    /// one.
    ///
    /// This has to come from the action JSON rather than from the encoded frame, because it
    /// is deliberately the only part of an MD5 challenge that is *not* transmitted: the
    /// challenge goes to the supplicant, the password stays here so NetGet can do the
    /// comparison itself. Reading it back off the wire is therefore impossible by design.
    expected_password: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct EapolServer;

impl EapolServer {
    /// Start the authenticator on whichever transport was asked for.
    ///
    /// Returns `Err` — so `server_startup` sets `ServerStatus::Error` — for an unknown
    /// transport, an out-of-range version, a missing capture privilege, an interface that
    /// does not exist, or a socket that will not bind. Nothing here is fire-and-forget: the
    /// raw path hands its open result back over a oneshot and only returns `Ok` once the
    /// capture is genuinely live.
    pub async fn spawn_with_llm_actions(ctx: SpawnContext) -> Result<SocketAddr> {
        let legacy_addr = ctx.legacy_listen_addr();
        let interface = ctx.interface().map(|s| s.to_string());
        let socket_addr = ctx.socket_addr();

        let SpawnContext {
            llm_client,
            state,
            status_tx,
            server_id,
            startup_params,
            ..
        } = ctx;

        // Both parameters are optional, but a supplied value that makes no sense must refuse
        // the start rather than be silently replaced by a default. Propagated with `?`, never
        // unwrapped: an undeclared key or a wrong type over MCP used to panic the per-request
        // task and hang the caller with no error at all.
        let (transport, forced_version) = match &startup_params {
            Some(params) => {
                let transport = params
                    .get_optional_string("transport")?
                    .unwrap_or_else(|| actions::TRANSPORT_RAW.to_string())
                    .to_ascii_lowercase();
                let version = match params.get_optional_u64("eapol_version")? {
                    Some(v) if (1..=3).contains(&v) => Some(v as u8),
                    Some(v) => {
                        return Err(anyhow::anyhow!(
                            "eapol_version must be 1 (802.1X-2001), 2 (802.1X-2004) or 3 \
                             (802.1X-2010); got {}",
                            v
                        ))
                    }
                    None => None,
                };
                (transport, version)
            }
            None => (actions::TRANSPORT_RAW.to_string(), None),
        };

        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

        match transport.as_str() {
            actions::TRANSPORT_UDP => {
                let bind_addr = socket_addr.unwrap_or(legacy_addr);
                Self::spawn_udp(
                    bind_addr,
                    ServerCx {
                        llm_client,
                        state,
                        status_tx,
                        server_id,
                        sessions,
                        local_addr: bind_addr,
                        forced_version,
                    },
                )
                .await
            }
            actions::TRANSPORT_RAW => {
                let interface = interface.context(
                    "EAPOL with transport 'raw' requires a network interface to capture on",
                )?;
                Self::spawn_raw(
                    interface,
                    ServerCx {
                        llm_client,
                        state,
                        status_tx,
                        server_id,
                        sessions,
                        local_addr: legacy_addr,
                        forced_version,
                    },
                )
                .await?;
                Ok(legacy_addr)
            }
            other => Err(anyhow::anyhow!(
                "Unknown EAPOL transport '{}'. Use 'raw' (real EAPOL on EtherType 0x888E, \
                 needs packet-capture privilege) or 'udp' (the unprivileged test transport).",
                other
            )),
        }
    }

    // -----------------------------------------------------------------------
    // UDP test transport
    // -----------------------------------------------------------------------

    /// The unprivileged transport. Each datagram is `[supplicant MAC (6)][EAPOL frame]`, in
    /// both directions — the six octets are always the *supplicant's* address, which is the
    /// one piece of Ethernet framing the events actually carry.
    async fn spawn_udp(bind_addr: SocketAddr, cx: ServerCx) -> Result<SocketAddr> {
        let socket = Arc::new(
            UdpSocket::bind(bind_addr)
                .await
                .with_context(|| format!("EAPOL failed to bind {}", bind_addr))?,
        );
        let local_addr = socket.local_addr()?;

        // Two lines, not one: the test harness reads the bound port back out of a
        // "listening on ADDR:PORT" line with `rfind("on ")`, so anything trailing the address
        // risks being parsed as the port.
        let log = Log::new(Some(&cx.status_tx));
        log.info(format!("EAPOL authenticator listening on {}", local_addr));
        log.info("EAPOL transport=udp: each datagram is [supplicant MAC (6 octets)][EAPOL frame]");

        let mut cx = cx;
        cx.local_addr = local_addr;

        let registrar = cx.state.clone();
        let server_id = cx.server_id;

        let handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; MAX_FRAME_LEN];
            loop {
                let (n, peer) = match socket.recv_from(&mut buffer).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        Log::new(Some(&cx.status_tx)).error(format!("EAPOL receive error: {}", e));
                        break;
                    }
                };

                if n < 6 {
                    Log::new(Some(&cx.status_tx)).warn(format!(
                        "EAPOL dropped a {}-octet datagram from {}: the udp transport prefixes \
                         every frame with the supplicant's 6-octet MAC address",
                        n, peer
                    ));
                    continue;
                }

                let mut supplicant_mac = [0u8; 6];
                supplicant_mac.copy_from_slice(&buffer[..6]);
                let payload = buffer[6..n].to_vec();

                let transmit = Transmit::Udp {
                    socket: socket.clone(),
                    peer,
                };
                let task_cx = cx.clone();
                tokio::spawn(async move {
                    Self::handle_eapol(
                        supplicant_mac,
                        codec::PAE_GROUP_ADDRESS,
                        payload,
                        transmit,
                        peer,
                        task_cx,
                    )
                    .await;
                });
            }
        });

        registrar.register_server_task(server_id, handle).await;
        Ok(local_addr)
    }

    // -----------------------------------------------------------------------
    // Raw Ethernet transport
    // -----------------------------------------------------------------------

    /// Real EAPOL over libpcap.
    ///
    /// **This code path has never been executed.** It needs packet-capture privilege, which
    /// the environment it was written in does not have, and no third-party supplicant has
    /// ever spoken to it. `metadata().notes` and the module CLAUDE.md both say so, and the
    /// path from here to Beta is written down there rather than assumed.
    async fn spawn_raw(interface: String, cx: ServerCx) -> Result<()> {
        use pcap::{Capture, Device};

        // The privilege gate lives here rather than in `metadata()`, and the comment on
        // `privilege_requirement` in actions.rs explains why: a static PacketCapture
        // declaration would also refuse `transport: "udp"`, which needs nothing at all.
        let caps = cx.state.get_system_capabilities().await;
        if !caps.has_packet_capture_access {
            return Err(anyhow::anyhow!(
                "EAPOL transport 'raw' needs layer-2 capture and injection on '{}': root, or \
                 read/write access to /dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux. \
                 Current capabilities: {}. Use startup_params {{\"transport\": \"udp\"}} to \
                 exercise the authenticator without privilege.",
                interface,
                caps.description()
            ));
        }

        // pnet supplies the interface's own MAC, which becomes the source address of every
        // frame this authenticator emits. libpcap does not expose it.
        let mac = pnet::datalink::interfaces()
            .into_iter()
            .find(|i| i.name == interface)
            .with_context(|| format!("no such network interface '{}'", interface))?
            .mac
            .with_context(|| {
                format!(
                    "interface '{}' has no MAC address, so it has no Ethernet link layer for \
                     EAPOL to live on. Point this server at a real Ethernet or Wi-Fi interface.",
                    interface
                )
            })?;
        let local_mac: [u8; 6] = [mac.0, mac.1, mac.2, mac.3, mac.4, mac.5];

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // `JoinHandle::abort()` cannot interrupt a thread parked in `next_packet()`, so the
        // capture loop stops cooperatively, exactly as `arp` does.
        let stop = crate::utils::StopSignal::new();
        let stop_in_loop = stop.clone();

        let registrar = cx.state.clone();
        let server_id = cx.server_id;
        let status_after = cx.status_tx.clone();
        let interface_for_task = interface.clone();
        let status_for_task = cx.status_tx.clone();

        tokio::task::spawn_blocking(move || {
            let open = || -> Result<(Capture<pcap::Active>, Capture<pcap::Active>)> {
                let device = Device::list()
                    .context("failed to enumerate capture devices")?
                    .into_iter()
                    .find(|d| d.name == interface_for_task)
                    .with_context(|| format!("no such capture device '{}'", interface_for_task))?;

                let mut rx = Capture::from_device(device.clone())
                    .map(|c| c.promisc(true).snaplen(65535).timeout(1000))
                    .and_then(|c| c.open())
                    .with_context(|| {
                        format!(
                            "failed to open pcap capture on '{}' (needs root, or read access \
                             to /dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux)",
                            interface_for_task
                        )
                    })?;

                // Without the filter every frame on the segment reaches the LLM, so a failure
                // here refuses the start rather than falling through. `ether proto` is an
                // Ethernet-only keyword: on loopback, a tunnel or a raw-IP device libpcap
                // rejects it outright, which is the same trap `arp` and `isis` document.
                rx.filter("ether proto 0x888e", true).with_context(|| {
                    format!(
                        "failed to apply the EAPOL BPF filter on '{}'. EAPOL is an \
                         Ethernet-only protocol, so this fails on any interface with no \
                         Ethernet link layer — loopback (lo/lo0), tunnels and raw-IP devices \
                         carry no EAPOL. Point this server at a real Ethernet or Wi-Fi \
                         interface, or use startup_params {{\"transport\": \"udp\"}}.",
                        interface_for_task
                    )
                })?;

                let tx = Capture::from_device(device)
                    .map(|c| c.promisc(true).snaplen(65535).timeout(1000))
                    .and_then(|c| c.open())
                    .with_context(|| {
                        format!(
                            "failed to open pcap injection handle on '{}'",
                            interface_for_task
                        )
                    })?;

                Ok((rx, tx))
            };

            let (mut rx, mut tx) = match open() {
                Ok(handles) => {
                    let _ = ready_tx.send(Ok(()));
                    handles
                }
                Err(e) => {
                    Log::new(Some(&status_for_task))
                        .error(format!("EAPOL capture startup failed: {:#}", e));
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            // Injection runs on its own thread: sendpacket blocks, and the capture loop must
            // not miss frames while it does.
            std::thread::spawn(move || {
                while let Some(frame) = frames_rx.blocking_recv() {
                    if let Err(e) = tx.sendpacket(frame) {
                        error!("EAPOL failed to inject frame: {}", e);
                    }
                }
            });

            let runtime = tokio::runtime::Handle::current();
            // Raw Ethernet has no peer socket address; the dashboard row wants one anyway.
            let peer_placeholder = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

            loop {
                if stop_in_loop.is_stopped() {
                    debug!("EAPOL capture on '{}' stopping", interface_for_task);
                    break;
                }
                match rx.next_packet() {
                    Ok(packet) => {
                        let ethernet = match codec::parse_ethernet_frame(packet.data) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!("EAPOL dropped an unparseable frame: {}", e);
                                continue;
                            }
                        };
                        if ethernet.ethertype != codec::ETHERTYPE_EAPOL {
                            continue;
                        }
                        // Our own injected frames come back through the capture handle.
                        if ethernet.source == local_mac {
                            continue;
                        }

                        let transmit = Transmit::Raw {
                            frames: frames_tx.clone(),
                            local_mac,
                        };
                        let task_cx = cx.clone();
                        runtime.spawn(async move {
                            Self::handle_eapol(
                                ethernet.source,
                                ethernet.destination,
                                ethernet.payload,
                                transmit,
                                peer_placeholder,
                                task_cx,
                            )
                            .await;
                        });
                    }
                    Err(pcap::Error::TimeoutExpired) => continue,
                    Err(e) => {
                        error!("EAPOL capture error on '{}': {}", interface_for_task, e);
                        break;
                    }
                }
            }
        });

        // Only report success once the capture is genuinely live. A server that sits in
        // `Running` having captured nothing is worse than one that refuses to start.
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "EAPOL capture task on '{}' exited before signalling readiness",
                    interface
                ))
            }
        }

        Log::new(Some(&status_after)).info(format!(
            "EAPOL authenticator capturing on '{}' (EtherType 0x888E, local MAC {})",
            interface,
            codec::format_mac(&local_mac)
        ));

        registrar
            .register_server_task(server_id, stop.park_task())
            .await;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // One received frame
    // -----------------------------------------------------------------------

    /// Decode one EAPOL frame, raise the matching event, apply the fail-closed rule and write
    /// the answer. Shared by both transports.
    async fn handle_eapol(
        supplicant_mac: [u8; 6],
        destination_mac: [u8; 6],
        payload: Vec<u8>,
        transmit: Transmit,
        peer: SocketAddr,
        cx: ServerCx,
    ) {
        let log = Log::new(Some(&cx.status_tx));
        let mac_text = codec::format_mac(&supplicant_mac);

        trace!(
            "EAPOL {} octets from {}: {}",
            payload.len(),
            mac_text,
            hex::encode(&payload)
        );

        let frame = match codec::EapolFrame::decode(&payload) {
            Ok(f) => f,
            Err(e) => {
                log.warn(format!("EAPOL dropped a frame from {}: {}", mac_text, e));
                return;
            }
        };

        let version = cx.forced_version.unwrap_or(frame.version);

        // Register (or find) the session, and with it the connection row the dashboard draws.
        let connection_id = Self::ensure_session(&cx, supplicant_mac, peer, payload.len()).await;

        // Classify the frame into an event. `prepare` takes and releases the session lock
        // internally and returns owned data, so no guard is alive across the LLM call.
        let Some(prepared) = Self::prepare(
            &cx,
            supplicant_mac,
            &frame,
            version,
            &mac_text,
            &destination_mac,
        ) else {
            return;
        };

        debug!(
            "EAPOL {} from {} raising {}",
            codec::eapol_packet_type_name(frame.packet_type),
            mac_text,
            prepared.event_id
        );

        let protocol = EapolProtocol::for_request(prepared.request.clone());
        let event = match prepared.event_id {
            "eapol_start" => Event::new(&EAPOL_START_EVENT, prepared.event_data.clone()),
            "eapol_identity_response" => {
                Event::new(&EAPOL_IDENTITY_RESPONSE_EVENT, prepared.event_data.clone())
            }
            "eapol_method_response" => {
                Event::new(&EAPOL_METHOD_RESPONSE_EVENT, prepared.event_data.clone())
            }
            _ => Event::new(&EAPOL_LOGOFF_EVENT, prepared.event_data.clone()),
        };

        let outcome = call_llm(
            &cx.llm_client,
            &cx.state,
            cx.server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await;

        let answer = Self::decide(
            outcome,
            &prepared.request,
            prepared.silence_is_safe,
            &cx.status_tx,
            &mac_text,
        );
        let decision = answer.decision;

        // Log the decision before writing it, and make every fail-closed path loud. The token
        // is stable so an operator can grep `decision=fail_closed_` and find every supplicant
        // the model did not actually answer for.
        let summary = format!(
            "EAPOL {} mac={} identity={} decision={}",
            prepared.event_id,
            mac_text,
            prepared.identity.as_deref().unwrap_or("-"),
            decision.as_str()
        );
        if decision.is_fail_closed() {
            log.error(format!(
                "{} (denied because no usable admission decision was produced)",
                summary
            ));
        } else {
            log.info(&summary);
        }

        let Some(reply) = answer.frame else {
            debug!("EAPOL sending nothing to {}", mac_text);
            if prepared.terminal {
                Self::end_session(&cx, supplicant_mac).await;
            }
            return;
        };

        let outcome_class = classify_frame(&reply);

        // Bookkeeping for whatever we are about to send. The identifier and any challenge are
        // read back out of the encoded frame — the bytes on the wire are the authority — and
        // only the expected password comes from the action, because it is the one field the
        // wire deliberately never carries.
        Self::record_outbound(&cx, supplicant_mac, &reply, &answer.expected_password);

        match transmit.send(supplicant_mac, &reply).await {
            Ok(sent) => {
                trace!(
                    "EAPOL sent {} octets to {}: {}",
                    sent,
                    mac_text,
                    hex::encode(&reply)
                );
                cx.state
                    .update_connection_stats(
                        cx.server_id,
                        connection_id,
                        None,
                        Some(sent as u64),
                        None,
                        Some(1),
                    )
                    .await;
            }
            Err(e) => {
                log.error(format!("EAPOL failed to reply to {}: {}", mac_text, e));
            }
        }

        // A Success or a Failure ends the exchange either way.
        let terminal_reply = matches!(
            outcome_class,
            Some(Decision::ModelAdmit) | Some(Decision::ModelReject)
        );
        if terminal_reply || prepared.terminal {
            let authorized = matches!(outcome_class, Some(Decision::ModelAdmit));
            log.info(format!(
                "EAPOL port for {} is now {} (identity={}, decision={})",
                mac_text,
                if authorized {
                    "AUTHORIZED"
                } else {
                    "unauthorized"
                },
                prepared.identity.as_deref().unwrap_or("-"),
                decision.as_str()
            ));
            Self::end_session(&cx, supplicant_mac).await;
        }
    }

    // -----------------------------------------------------------------------
    // The fail-closed rule
    // -----------------------------------------------------------------------

    /// **The fail-closed rule.** Returns the decision taken and the frame to send, if any.
    ///
    /// - A usable model action is used verbatim.
    /// - No usable action, an unencodable action, or an LLM error produces a synthesised
    ///   **EAP-Failure** and a `fail_closed_*` decision. It is never reported as
    ///   `model_reject`: a model that denies and a model that is unreachable must remain
    ///   distinguishable, which is exactly what OAuth2 lost.
    /// - An `EAP-Success` is used only when the session has an established identity. The
    ///   executor already refuses otherwise; this is the second gate, and it has its own
    ///   token so a first-gate regression is visible in the log rather than silent.
    /// - `silence_is_safe` is set only for EAPOL-Logoff, where the port was already
    ///   de-authorized before the model was asked, so there is nothing left to deny.
    ///
    /// There is no argument to this function, and no branch inside it, that can produce
    /// `EAP-Success` bytes. Success can only arrive here already encoded, from
    /// `execute_action`'s `send_eap_success` arm.
    fn decide(
        llm_outcome: Result<crate::llm::ExecutionResult>,
        request: &actions::RequestContext,
        silence_is_safe: bool,
        status_tx: &mpsc::UnboundedSender<String>,
        mac_text: &str,
    ) -> Answer {
        let log = Log::new(Some(status_tx));

        // Every failure exit below carries `expected_password: None`. That is not an
        // oversight: a challenge that was never issued has no password to remember.
        let denied = |decision: Decision| Answer {
            decision,
            frame: Self::synthesised_failure(request, silence_is_safe),
            expected_password: None,
        };

        let execution = match llm_outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }
                result
            }
            Err(e) => {
                // EAPOL has exactly one way to say no, so an overloaded backend and a dead
                // one both produce the same EAP-Failure on the wire. The distinction is still
                // worth having, so it goes in the log next to the decision token — and the
                // error itself never reaches the supplicant.
                let category = match crate::utils::wire_failure::WireFailure::classify(&e) {
                    crate::utils::wire_failure::WireFailure::Overloaded => "overloaded",
                    crate::utils::wire_failure::WireFailure::Unavailable => "unavailable",
                };
                log.error(format!(
                    "EAPOL LLM call failed for {} (category={}): {}",
                    mac_text, category, e
                ));
                return denied(Decision::FailClosedLlmError);
            }
        };

        // The password the model wants an MD5 response compared against. Taken from the
        // action rather than the frame, for the reason `Answer::expected_password` records.
        let expected_password = execution
            .raw_actions
            .iter()
            .filter(|a| a.get("type").and_then(|v| v.as_str()) == Some("send_eap_request_method"))
            .find_map(|a| {
                a.get("expected_password")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        // Take the first output that decodes as an EAPOL reply. Extra outputs are a model
        // error, not a licence to send several frames for one received packet.
        let mut chosen: Option<(Decision, Vec<u8>)> = None;
        let mut extra = 0usize;
        for protocol_result in &execution.protocol_results {
            for output in protocol_result.get_all_output() {
                match classify_frame(&output) {
                    Some(decision) if chosen.is_none() => chosen = Some((decision, output)),
                    _ => extra += 1,
                }
            }
        }
        if extra > 0 {
            warn!(
                "EAPOL ignored {} extra frame(s) for {}; one received packet gets one reply",
                extra, mac_text
            );
        }

        if let Some((decision, bytes)) = chosen {
            // Second gate on admission. `execute_action` refuses a Success without an
            // established identity, so this cannot fire today — which is the point: if it
            // ever does, an authentication bypass is reported in the log instead of served
            // on the wire.
            if decision == Decision::ModelAdmit && !request.identity_established {
                log.error(format!(
                    "EAPOL refusing an EAP-Success for {}: this session has produced no \
                     identity. Denying instead. This should be unreachable — the action \
                     executor refuses it first — so treat it as a defect report.",
                    mac_text
                ));
                return Answer {
                    decision: Decision::FailClosedNoIdentity,
                    // Never silent: an admission that had to be refused is a denial, even on
                    // the one event where saying nothing would otherwise be safe.
                    frame: Self::synthesised_failure(request, false),
                    expected_password: None,
                };
            }
            return Answer {
                decision,
                frame: Some(bytes),
                expected_password,
            };
        }

        // Nothing usable came back. If the model produced actions at all, they failed to
        // encode; if it produced none, it stayed silent. Both deny, and both are recorded as
        // the server's decision rather than the model's.
        denied(if execution.raw_actions.is_empty() {
            Decision::FailClosedNoAction
        } else {
            Decision::FailClosedActionError
        })
    }

    /// The EAP-Failure the server sends when the model did not decide.
    ///
    /// Built by calling [`codec::eapol_eap_failure_frame`] directly — it never goes near the
    /// action executor, so no model output and no error path can steer it.
    fn synthesised_failure(
        request: &actions::RequestContext,
        silence_is_safe: bool,
    ) -> Option<Vec<u8>> {
        if silence_is_safe {
            // EAPOL-Logoff: the session was destroyed before the model was asked, so there is
            // no access left to deny and a fabricated frame would say nothing true.
            return None;
        }
        Some(codec::eapol_eap_failure_frame(
            request.eapol_version,
            request.result_identifier,
        ))
    }

    // -----------------------------------------------------------------------
    // Session bookkeeping
    // -----------------------------------------------------------------------

    /// Find or create the session for a supplicant, and the connection row that represents it.
    async fn ensure_session(
        cx: &ServerCx,
        mac: [u8; 6],
        peer: SocketAddr,
        bytes: usize,
    ) -> ConnectionId {
        // Fast path: an existing session. The guard is dropped before the await.
        let existing = {
            let sessions = sessions_lock(&cx.sessions);
            sessions.get(&mac).map(|s| s.connection_id)
        };
        if let Some(id) = existing {
            cx.state
                .update_connection_stats(cx.server_id, id, Some(bytes as u64), None, Some(1), None)
                .await;
            return id;
        }

        let connection_id = ConnectionId::new(cx.state.get_next_unified_id().await);
        {
            let mut sessions = sessions_lock(&cx.sessions);
            sessions.entry(mac).or_insert(Session {
                connection_id,
                identity: None,
                last_request_identifier: None,
                next_identifier: 1,
                pending_md5: None,
            });
        }

        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let now = std::time::Instant::now();
        cx.state
            .add_connection_to_server(
                cx.server_id,
                ServerConnectionState {
                    id: connection_id,
                    remote_addr: peer,
                    local_addr: cx.local_addr,
                    bytes_sent: 0,
                    bytes_received: bytes as u64,
                    packets_sent: 0,
                    packets_received: 1,
                    last_activity: now,
                    status: ConnectionStatus::Active,
                    status_changed_at: now,
                    protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                        "supplicant_mac": codec::format_mac(&mac),
                    })),
                },
            )
            .await;
        let _ = cx.status_tx.send("__UPDATE_UI__".to_string());
        connection_id
    }

    /// Drop a session and close its dashboard row.
    ///
    /// EAPOL sessions are removed explicitly here rather than by the 10-second idle sweep:
    /// `metadata()` deliberately does not set `connectionless()`, because an exchange parked
    /// on a manual handler waiting for a human routinely outlives 10 seconds, and the sweep
    /// would draw it `(closed)` while it was still live.
    async fn end_session(cx: &ServerCx, mac: [u8; 6]) {
        let connection_id = {
            let mut sessions = sessions_lock(&cx.sessions);
            sessions.remove(&mac).map(|s| s.connection_id)
        };
        if let Some(id) = connection_id {
            cx.state.close_connection_on_server(cx.server_id, id).await;
            let _ = cx.status_tx.send("__UPDATE_UI__".to_string());
        }
    }

    /// Record what an outbound frame implies for the session: the identifier a Response must
    /// echo, and any MD5 challenge now outstanding.
    ///
    /// Read from the encoded frame, not from the model's JSON — `expected_password` is the
    /// only thing the action carries that the wire does not.
    fn record_outbound(
        cx: &ServerCx,
        mac: [u8; 6],
        frame: &[u8],
        expected_password: &Option<String>,
    ) {
        let Ok(eapol) = codec::EapolFrame::decode(frame) else {
            return;
        };
        if eapol.packet_type != codec::EAPOL_TYPE_EAP_PACKET {
            return;
        }
        let Ok(eap) = codec::EapPacket::decode(&eapol.body) else {
            return;
        };
        if eap.code != codec::EAP_CODE_REQUEST {
            return;
        }

        let pending = if eap.eap_type == Some(codec::EAP_TYPE_MD5_CHALLENGE) {
            codec::decode_md5_value(&eap.type_data)
                .ok()
                .map(|(challenge, _name)| PendingMd5 {
                    identifier: eap.identifier,
                    challenge,
                    expected_password: expected_password.clone(),
                })
        } else {
            None
        };

        let mut sessions = sessions_lock(&cx.sessions);
        if let Some(session) = sessions.get_mut(&mac) {
            session.last_request_identifier = Some(eap.identifier);
            session.next_identifier = eap.identifier.wrapping_add(1);
            if pending.is_some() {
                session.pending_md5 = pending;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Event construction
    // -----------------------------------------------------------------------

    /// Classify a received frame into an event, and build the reply context for it.
    ///
    /// Returns `None` for anything this server must not answer — a Request or a
    /// Success/Failure sent *by* a supplicant, a Response whose identifier does not echo
    /// ours, an EAPOL-Key, or a malformed EAP packet. Dropping is what RFC 3748 §4.1
    /// prescribes.
    fn prepare(
        cx: &ServerCx,
        mac: [u8; 6],
        frame: &codec::EapolFrame,
        version: u8,
        mac_text: &str,
        destination_mac: &[u8; 6],
    ) -> Option<Prepared> {
        let log = Log::new(Some(&cx.status_tx));

        match frame.packet_type {
            codec::EAPOL_TYPE_START => {
                let request_identifier = {
                    let mut sessions = sessions_lock(&cx.sessions);
                    let session = sessions.get_mut(&mac)?;
                    // An EAPOL-Start restarts the exchange: whatever was claimed before is no
                    // longer evidence of anything.
                    session.identity = None;
                    session.pending_md5 = None;
                    session.next_identifier
                };
                Some(Prepared {
                    event_id: "eapol_start",
                    event_data: serde_json::json!({
                        "source_mac": mac_text,
                        "destination_mac": codec::format_mac(destination_mac),
                        "eapol_version": version,
                    }),
                    request: actions::RequestContext {
                        eapol_version: version,
                        request_identifier,
                        // Nothing has been received to echo, so a denial carries the
                        // identifier the next Request would have used.
                        result_identifier: request_identifier,
                        identity_established: false,
                        md5_challenge: Self::fresh_challenge(),
                    },
                    identity: None,
                    silence_is_safe: false,
                    terminal: false,
                })
            }

            codec::EAPOL_TYPE_LOGOFF => {
                // De-authorize first, then ask. Ending access is not the model's decision to
                // make, and a backend outage must not be able to keep a port open.
                let (identity, request_identifier) = {
                    let mut sessions = sessions_lock(&cx.sessions);
                    let session = sessions.get_mut(&mac)?;
                    session.pending_md5 = None;
                    let identity = session.identity.take();
                    (identity, session.next_identifier)
                };
                Some(Prepared {
                    event_id: "eapol_logoff",
                    event_data: serde_json::json!({
                        "source_mac": mac_text,
                        "identity": identity,
                        "eapol_version": version,
                    }),
                    request: actions::RequestContext {
                        eapol_version: version,
                        request_identifier,
                        result_identifier: request_identifier,
                        identity_established: false,
                        md5_challenge: Self::fresh_challenge(),
                    },
                    identity,
                    silence_is_safe: true,
                    terminal: true,
                })
            }

            codec::EAPOL_TYPE_EAP_PACKET => {
                let eap = match codec::EapPacket::decode(&frame.body) {
                    Ok(p) => p,
                    Err(e) => {
                        log.warn(format!(
                            "EAPOL dropped an EAP packet from {}: {}",
                            mac_text, e
                        ));
                        return None;
                    }
                };

                if eap.code != codec::EAP_CODE_RESPONSE {
                    // An authenticator answers Responses. A Request, Success or Failure sent
                    // by a supplicant is either a protocol violation or an attempt to talk
                    // the server into agreeing with itself.
                    log.warn(format!(
                        "EAPOL ignoring {} from {}: an authenticator answers Responses only",
                        codec::eap_code_name(eap.code),
                        mac_text
                    ));
                    return None;
                }

                let (identity, pending, expected_identifier) = {
                    let sessions = sessions_lock(&cx.sessions);
                    let session = sessions.get(&mac)?;
                    (
                        session.identity.clone(),
                        session.pending_md5.clone(),
                        session.last_request_identifier,
                    )
                };

                // RFC 3748 §4.1: a Response that does not echo the outstanding Request's
                // identifier is silently discarded. Getting this wrong looks exactly like a
                // hang, which is why it is checked rather than assumed.
                match expected_identifier {
                    Some(expected) if eap.identifier != expected => {
                        log.warn(format!(
                            "EAPOL discarding a Response from {}: identifier {} does not echo \
                             the outstanding Request's {}",
                            mac_text, eap.identifier, expected
                        ));
                        return None;
                    }
                    Some(_) => {}
                    None => debug!(
                        "EAPOL accepting an unsolicited Response from {} (no Request was \
                         outstanding)",
                        mac_text
                    ),
                }

                let eap_type = eap.eap_type.unwrap_or(0);

                if eap_type == codec::EAP_TYPE_IDENTITY {
                    let claimed = String::from_utf8_lossy(&eap.type_data).into_owned();
                    {
                        let mut sessions = sessions_lock(&cx.sessions);
                        if let Some(session) = sessions.get_mut(&mac) {
                            session.identity = Some(claimed.clone());
                            session.pending_md5 = None;
                        }
                    }
                    return Some(Prepared {
                        event_id: "eapol_identity_response",
                        event_data: serde_json::json!({
                            "identity": claimed,
                            "source_mac": mac_text,
                            "eap_identifier": eap.identifier,
                            "eapol_version": version,
                        }),
                        request: actions::RequestContext {
                            eapol_version: version,
                            request_identifier: eap.identifier.wrapping_add(1),
                            // RFC 3748 §4.2: a Success or Failure echoes the Response it
                            // answers. A mismatch is silently discarded by the supplicant,
                            // which looks exactly like a hang.
                            result_identifier: eap.identifier,
                            identity_established: true,
                            md5_challenge: Self::fresh_challenge(),
                        },
                        identity: Some(claimed),
                        silence_is_safe: false,
                        terminal: false,
                    });
                }

                // A method response. NetGet does the cryptography; the model gets a verdict.
                let mut data = serde_json::json!({
                    "eap_type": codec::eap_type_name(eap_type),
                    "eap_type_number": eap_type,
                    "identity": identity,
                    "source_mac": mac_text,
                    "eap_identifier": eap.identifier,
                    "eapol_version": version,
                    "type_data_length": eap.type_data.len(),
                });

                if eap_type == codec::EAP_TYPE_MD5_CHALLENGE {
                    let (verification, verified) =
                        Self::verify_md5(&pending, eap.identifier, &eap.type_data);
                    data["md5_verification"] = serde_json::json!(verification);
                    data["md5_verified"] = serde_json::json!(verified);
                } else if eap_type == codec::EAP_TYPE_NAK {
                    let desired: Vec<String> = eap
                        .type_data
                        .iter()
                        .map(|t| codec::eap_type_name(*t).to_string())
                        .collect();
                    data["desired_eap_types"] = serde_json::json!(desired);
                } else if eap_type == codec::EAP_TYPE_TLS || eap_type == codec::EAP_TYPE_PEAP {
                    if let Some((length_included, more, start)) = codec::tls_flags(&eap.type_data) {
                        data["tls_length_included"] = serde_json::json!(length_included);
                        data["tls_more_fragments"] = serde_json::json!(more);
                        data["tls_start"] = serde_json::json!(start);
                    }
                }

                Some(Prepared {
                    event_id: "eapol_method_response",
                    event_data: data,
                    request: actions::RequestContext {
                        eapol_version: version,
                        request_identifier: eap.identifier.wrapping_add(1),
                        result_identifier: eap.identifier,
                        // A method response can only admit a session that already claimed an
                        // identity. Without one there is nothing to admit, whatever the
                        // digest says.
                        identity_established: identity.is_some(),
                        md5_challenge: Self::fresh_challenge(),
                    },
                    identity,
                    silence_is_safe: false,
                    terminal: false,
                })
            }

            other => {
                // EAPOL-Key (MKA) and Encapsulated-ASF-Alert are not implemented, and no
                // event is declared for them. Declaring one that nothing could answer would
                // be worse than saying so here.
                log.warn(format!(
                    "EAPOL ignoring {} from {}: not implemented by this authenticator",
                    codec::eapol_packet_type_name(other),
                    mac_text
                ));
                None
            }
        }
    }

    /// Compare a supplicant's MD5-Challenge response against the outstanding challenge.
    ///
    /// Every branch that is not a genuine match reports `false`. In particular, "the model
    /// never told us a password" reports `not_checked_no_expected_password`, not `verified` —
    /// the OAuth2 shape, where the absence of information became a positive assertion.
    fn verify_md5(
        pending: &Option<PendingMd5>,
        identifier: u8,
        type_data: &[u8],
    ) -> (&'static str, bool) {
        let Some(pending) = pending else {
            return ("unsolicited", false);
        };
        if pending.identifier != identifier {
            return ("unsolicited", false);
        }
        let Ok((value, _name)) = codec::decode_md5_value(type_data) else {
            return ("malformed", false);
        };
        let Some(password) = pending.expected_password.as_deref() else {
            return ("not_checked_no_expected_password", false);
        };
        if codec::md5_response_matches(identifier, password, &pending.challenge, &value) {
            ("verified", true)
        } else {
            ("mismatch", false)
        }
    }

    /// A fresh MD5 challenge. Generated here, never by the model: RFC 1994 §2.2 requires it
    /// to be unpredictable, and a model cannot produce randomness.
    fn fresh_challenge() -> Vec<u8> {
        use rand::Rng;
        let mut challenge = vec![0u8; codec::MD5_CHALLENGE_LEN];
        rand::thread_rng().fill(&mut challenge[..]);
        challenge
    }
}
