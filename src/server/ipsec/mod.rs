//! IPSec/IKEv2 parse-and-log honeypot
//!
//! # What this actually is
//!
//! A **read-only IKE honeypot**. It binds a UDP socket, parses the 28-byte IKE
//! header and walks the payload chain of every datagram it receives, then reports
//! what it saw. It performs **no cryptography, establishes no Security
//! Associations, creates no tunnel interface, and never transmits a single byte
//! back to the peer**. Nothing here is a VPN; use WireGuard
//! (`src/server/wireguard/`) when a working tunnel is required.
//!
//! Staying silent is deliberate: it avoids accidentally negotiating anything and
//! avoids fingerprinting the honeypot by its responses.
//!
//! # What the LLM controls
//!
//! Each IKE_SA_INIT / IKE_AUTH (or IKEv1 Identity Protection / Aggressive Mode)
//! datagram raises an `ipsec_handshake` event through
//! [`crate::llm::action_helper::call_llm`], which runs any configured
//! script/static event handler first and falls back to a real LLM call only when
//! none is configured. The resulting actions are **classification and logging
//! decisions only** - `accept_connection`, `reject_connection`, `log_handshake`
//! and `send_notify` all record intent without changing what goes on the wire.
//!
//! # Status
//!
//! `DevelopmentState::Experimental`. Turning this into a real IKEv2 responder
//! means implementing SA negotiation, Diffie-Hellman, authentication, ESP and
//! kernel XFRM/SAD/SPD programming - a multi-month project, not a patch. It is
//! deliberately out of scope.

pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use actions::{IpsecProtocol, IPSEC_HANDSHAKE_EVENT};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

/// Maximum IKE packet size
const MAX_PACKET_SIZE: usize = 65535;

/// IKEv2 header minimum size (28 bytes)
const IKE_HEADER_SIZE: usize = 28;

/// IKEv2 version (major=2, minor=0)
const IKEV2_VERSION: u8 = 0x20;

/// IKEv2 Exchange Types (from RFC 7296)
const IKE_SA_INIT: u8 = 34;
const IKE_AUTH: u8 = 35;
const CREATE_CHILD_SA: u8 = 36;
const INFORMATIONAL: u8 = 37;

/// IKEv1 Exchange Types (for detection)
const IKEV1_IDENTITY_PROTECTION: u8 = 2;
const IKEV1_AGGRESSIVE: u8 = 4;

/// IKE Header Flags (RFC 7296 Section 3.1)
const FLAG_INITIATOR: u8 = 0x08; // Initiator bit
const FLAG_VERSION: u8 = 0x10; // Version bit (must be 0 for IKEv2)
const FLAG_RESPONSE: u8 = 0x20; // Response bit

/// IKE Payload Types (RFC 7296 Section 3.2)
const PAYLOAD_NONE: u8 = 0;
const PAYLOAD_SA: u8 = 33; // Security Association
const PAYLOAD_KE: u8 = 34; // Key Exchange
const PAYLOAD_IDI: u8 = 35; // Identification - Initiator
const PAYLOAD_IDR: u8 = 36; // Identification - Responder
const PAYLOAD_CERT: u8 = 37; // Certificate
const PAYLOAD_CERTREQ: u8 = 38; // Certificate Request
const PAYLOAD_AUTH: u8 = 39; // Authentication
const PAYLOAD_NONCE: u8 = 40; // Nonce
const PAYLOAD_NOTIFY: u8 = 41; // Notify
const PAYLOAD_DELETE: u8 = 42; // Delete
const PAYLOAD_VENDOR: u8 = 43; // Vendor ID
const PAYLOAD_TSI: u8 = 44; // Traffic Selector - Initiator
const PAYLOAD_TSR: u8 = 45; // Traffic Selector - Responder
const PAYLOAD_SK: u8 = 46; // Encrypted and Authenticated
const PAYLOAD_CP: u8 = 47; // Configuration
const PAYLOAD_EAP: u8 = 48; // Extensible Authentication

/// IPSec/IKEv2 enhanced honeypot server
pub struct IpsecServer;

impl IpsecServer {
    /// Spawn IPSec/IKEv2 honeypot with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        bind_addr: SocketAddr,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<SocketAddr> {
        info!(
            "Starting IPSec/IKEv2 parse-and-log honeypot on {}",
            bind_addr
        );
        Log::new(Some(&status_tx)).info(format!(
            "Starting IPSec/IKEv2 honeypot on {} (parses and logs IKE, never replies)",
            bind_addr
        ));

        // Bind UDP socket (IKE uses UDP port 500, NAT-T uses 4500)
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx))
            .info(format!("IPSec/IKEv2 honeypot listening on {}", local_addr));

        let socket = Arc::new(socket);

        // Spawn packet handler
        let socket_clone = socket.clone();
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            if let Err(e) =
                Self::handle_packets(socket_clone, llm_client, app_state, server_id, status_tx)
                    .await
            {
                error!("IPSec/IKEv2 honeypot error: {}", e);
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Handle incoming IKE packets
    async fn handle_packets(
        socket: Arc<UdpSocket>,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let mut buf = vec![0u8; MAX_PACKET_SIZE];

        loop {
            // Receive packet
            let (len, peer_addr) = match socket.recv_from(&mut buf).await {
                Ok(result) => result,
                Err(e) => {
                    error!("UDP recv error: {}", e);
                    continue;
                }
            };

            let packet = &buf[..len];

            // Parse IKE header
            if len < IKE_HEADER_SIZE {
                trace!(
                    "Received undersized packet from {} ({} bytes)",
                    peer_addr,
                    len
                );
                continue;
            }

            // Extract IKE header fields (28 bytes - RFC 7296 Section 3.1)
            let initiator_spi = u64::from_be_bytes([
                packet[0], packet[1], packet[2], packet[3], packet[4], packet[5], packet[6],
                packet[7],
            ]);
            let responder_spi = u64::from_be_bytes([
                packet[8], packet[9], packet[10], packet[11], packet[12], packet[13], packet[14],
                packet[15],
            ]);
            let next_payload = packet[16];
            let version = packet[17];
            let exchange_type = packet[18];
            let flags = packet[19];
            let message_id = u32::from_be_bytes([packet[20], packet[21], packet[22], packet[23]]);
            let packet_length =
                u32::from_be_bytes([packet[24], packet[25], packet[26], packet[27]]);

            // Analyze flags
            let is_initiator = (flags & FLAG_INITIATOR) != 0;
            let is_response = (flags & FLAG_RESPONSE) != 0;
            let version_bit = (flags & FLAG_VERSION) != 0;

            // Extract payload chain
            let payload_types = Self::extract_payload_types(packet, next_payload);

            // Determine IKE version and exchange type
            let (ike_version, exchange_name, is_handshake) = if version == IKEV2_VERSION {
                let (name, handshake) = match exchange_type {
                    IKE_SA_INIT => ("IKE_SA_INIT", true),
                    IKE_AUTH => ("IKE_AUTH", true),
                    CREATE_CHILD_SA => ("CREATE_CHILD_SA", false),
                    INFORMATIONAL => ("INFORMATIONAL", false),
                    _ => ("Unknown", false),
                };
                ("IKEv2", name, handshake)
            } else {
                let (name, handshake) = match exchange_type {
                    IKEV1_IDENTITY_PROTECTION => ("Identity Protection", true),
                    IKEV1_AGGRESSIVE => ("Aggressive Mode", true),
                    _ => ("Unknown", false),
                };
                ("IKEv1", name, handshake)
            };

            // Format payload chain for logging
            let payload_names = Self::format_payload_types(&payload_types);

            trace!(
                "IKE packet from {}: version={}, exchange={}, flags=0x{:02x} (I={}, R={}, V={}), msg_id={}, len={}, payloads=[{}]",
                peer_addr,
                ike_version,
                exchange_name,
                flags,
                if is_initiator { "1" } else { "0" },
                if is_response { "1" } else { "0" },
                if version_bit { "1" } else { "0" },
                message_id,
                packet_length,
                payload_names
            );
            Log::new(Some(&status_tx)).trace(format!(
                "IPSec: {} {} from {} ({} bytes, payloads=[{}])",
                ike_version, exchange_name, peer_addr, len, payload_names
            ));

            // For handshake initiation, provide detailed analysis
            if is_handshake {
                Self::handle_handshake_initiation(
                    peer_addr,
                    packet,
                    ike_version,
                    exchange_name,
                    initiator_spi,
                    responder_spi,
                    is_initiator,
                    is_response,
                    message_id,
                    &payload_types,
                    &llm_client,
                    &app_state,
                    server_id,
                    &status_tx,
                )
                .await;
            } else {
                // Log other packet types for reconnaissance detection
                debug!(
                    "IPSec {} {} from {} (honeypot: logged only, payloads=[{}])",
                    ike_version, exchange_name, peer_addr, payload_names
                );
                Log::new(Some(&status_tx)).debug(format!(
                    "IPSec: {} {} from {} (logged, payloads=[{}])",
                    ike_version, exchange_name, peer_addr, payload_names
                ));
            }
        }
    }

    /// Extract payload types from IKE message
    fn extract_payload_types(packet: &[u8], mut next_payload: u8) -> Vec<u8> {
        let mut payload_types = Vec::new();
        let mut offset = IKE_HEADER_SIZE;

        // Walk the payload chain
        while next_payload != PAYLOAD_NONE && offset + 4 <= packet.len() {
            payload_types.push(next_payload);

            // Each payload has: next_payload(1) + reserved(1) + length(2)
            if offset + 4 > packet.len() {
                break;
            }

            let payload_length =
                u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
            if payload_length < 4 || offset + payload_length > packet.len() {
                break;
            }

            next_payload = packet[offset];
            offset += payload_length;
        }

        payload_types
    }

    /// Format payload types as human-readable names
    fn format_payload_types(payload_types: &[u8]) -> String {
        payload_types
            .iter()
            .map(|&p| Self::payload_type_name(p))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Get payload type name
    fn payload_type_name(payload_type: u8) -> &'static str {
        match payload_type {
            PAYLOAD_SA => "SA",
            PAYLOAD_KE => "KE",
            PAYLOAD_IDI => "IDi",
            PAYLOAD_IDR => "IDr",
            PAYLOAD_CERT => "CERT",
            PAYLOAD_CERTREQ => "CERTREQ",
            PAYLOAD_AUTH => "AUTH",
            PAYLOAD_NONCE => "NONCE",
            PAYLOAD_NOTIFY => "NOTIFY",
            PAYLOAD_DELETE => "DELETE",
            PAYLOAD_VENDOR => "VENDOR",
            PAYLOAD_TSI => "TSi",
            PAYLOAD_TSR => "TSr",
            PAYLOAD_SK => "SK",
            PAYLOAD_CP => "CP",
            PAYLOAD_EAP => "EAP",
            _ => "UNKNOWN",
        }
    }

    /// Handle handshake initiation - parse, report, and raise the LLM event.
    ///
    /// No IKE response is ever transmitted: this honeypot is receive-only.
    #[allow(clippy::too_many_arguments)]
    async fn handle_handshake_initiation(
        peer_addr: SocketAddr,
        packet: &[u8],
        ike_version: &str,
        exchange_type: &str,
        initiator_spi: u64,
        responder_spi: u64,
        is_initiator: bool,
        is_response: bool,
        message_id: u32,
        payload_types: &[u8],
        llm_client: &OllamaClient,
        app_state: &AppState,
        server_id: crate::state::ServerId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let payload_names = Self::format_payload_types(payload_types);

        info!(
            "IPSec {} handshake from {} (honeypot, payloads=[{}])",
            ike_version, peer_addr, payload_names
        );
        Log::new(Some(status_tx)).info(format!(
            "IPSec: {} handshake from {} (payloads=[{}])",
            ike_version, peer_addr, payload_names
        ));

        // Build event for the script/static handler or the LLM
        let event = Event::new(
            &IPSEC_HANDSHAKE_EVENT,
            serde_json::json!({
                "peer_addr": peer_addr.to_string(),
                "packet_size": packet.len(),
                "ike_version": ike_version,
                "exchange_type": exchange_type,
                "initiator_spi": format!("{:016x}", initiator_spi),
                "responder_spi": format!("{:016x}", responder_spi),
                "is_initiator": is_initiator,
                "is_response": is_response,
                "message_id": message_id,
                "payloads": payload_types.iter().map(|&p| Self::payload_type_name(p)).collect::<Vec<_>>(),
                "honeypot_mode": true,
                "responds_to_peer": false,
                "analysis": {
                    "expected_payloads": if exchange_type == "IKE_SA_INIT" {
                        "SA, KE, NONCE"
                    } else if exchange_type == "IKE_AUTH" {
                        "IDi, AUTH, SA, TSi, TSr"
                    } else {
                        "varies"
                    },
                    "has_encryption": payload_types.contains(&PAYLOAD_SK),
                    "has_vendor_id": payload_types.contains(&PAYLOAD_VENDOR),
                    "has_certificate": payload_types.contains(&PAYLOAD_CERT) || payload_types.contains(&PAYLOAD_CERTREQ),
                }
            }),
        );

        debug!(
            "IPSec/IKEv2 handshake analyzed from {} ({} payloads detected)",
            peer_addr,
            payload_types.len()
        );

        // Route through the event handler / LLM. Any configured script or static
        // handler runs in-process with no model call; otherwise this falls back to
        // the LLM. The resulting actions are classification decisions only - the
        // honeypot still transmits nothing.
        let protocol = IpsecProtocol::new();
        match call_llm(
            llm_client, app_state, server_id,
            None, // IKE is connectionless; no per-connection state is tracked
            &event, &protocol,
        )
        .await
        {
            Ok(result) => {
                let log = Log::new(Some(status_tx));
                for message in &result.messages {
                    log.info(format!("{}", message));
                }
                // Three outcomes have to stay distinguishable in the log, because the
                // wire cannot distinguish them: this honeypot is receive-only and every
                // one of them looks identical to the peer (nothing arrives). An operator
                // greps `decision=` to tell "the model refused" from "the model said
                // nothing" from "the backend broke".
                let decision =
                    if result.raw_actions.iter().any(|a| {
                        a.get("type").and_then(|t| t.as_str()) == Some("reject_connection")
                    }) {
                        "model_reject"
                    } else if result.raw_actions.is_empty() {
                        "no_answer"
                    } else {
                        "model_answer"
                    };
                info!(
                    "IPSec {} {} from {} decision={} actions={} (honeypot, nothing sent)",
                    ike_version,
                    exchange_type,
                    peer_addr,
                    decision,
                    result.raw_actions.len()
                );
            }
            Err(e) => {
                // Deliberately no wire response. There is nothing correct to send: the
                // honeypot never transmits (see the module docs), and an IKE NOTIFY here
                // would both fingerprint it and answer for a negotiation it cannot run.
                // Classify anyway so an overloaded backend is distinguishable from a
                // broken one in the log; the error itself goes to the log and the status
                // stream, never to the peer.
                let decision = match crate::utils::WireFailure::classify(&e) {
                    crate::utils::WireFailure::Overloaded => "fail_closed_overloaded",
                    crate::utils::WireFailure::Unavailable => "fail_closed_unavailable",
                };
                error!(
                    "IPSec {} {} from {} decision={} (honeypot, nothing sent): {:#}",
                    ike_version, exchange_type, peer_addr, decision, e
                );
                Log::new(Some(status_tx)).error(format!(
                    "IPSec handshake event handling failed ({}): {}",
                    decision, e
                ));
            }
        }
    }
}
