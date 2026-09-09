//! IGMP server implementation using raw IP sockets
pub mod actions;

use crate::server::connection::ConnectionId;
use actions::IgmpProtocol;
use anyhow::{Context, Result};
use socket2::Socket;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::FromRawFd;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use actions::{IGMP_LEAVE_RECEIVED_EVENT, IGMP_QUERY_RECEIVED_EVENT, IGMP_REPORT_RECEIVED_EVENT};

/// IGMP message types (RFC 2236)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IgmpMessageType {
    /// Membership Query (0x11)
    MembershipQuery = 0x11,
    /// IGMPv1 Membership Report (0x12)
    V1MembershipReport = 0x12,
    /// IGMPv2 Membership Report (0x16)
    V2MembershipReport = 0x16,
    /// Leave Group (0x17)
    LeaveGroup = 0x17,
    /// IGMPv3 Membership Report (0x22)
    V3MembershipReport = 0x22,
}

impl IgmpMessageType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x11 => Some(Self::MembershipQuery),
            0x12 => Some(Self::V1MembershipReport),
            0x16 => Some(Self::V2MembershipReport),
            0x17 => Some(Self::LeaveGroup),
            0x22 => Some(Self::V3MembershipReport),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::MembershipQuery => "Membership Query",
            Self::V1MembershipReport => "IGMPv1 Membership Report",
            Self::V2MembershipReport => "IGMPv2 Membership Report",
            Self::LeaveGroup => "Leave Group",
            Self::V3MembershipReport => "IGMPv3 Membership Report",
        }
    }
}

/// Smallest legal IPv4 header (IHL 5 words), in bytes.
const MIN_IP_HEADER: usize = 20;
/// Largest legal IPv4 header (IHL 15 words), in bytes.
const MAX_IP_HEADER: usize = 60;
/// IANA protocol number for IGMP.
const IP_PROTO_IGMP: u8 = 2;
/// `224.0.0.2`, the ALL-ROUTERS group a Leave Group is addressed to (RFC 2236 §2.9).
const ALL_ROUTERS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 2);
/// `224.0.0.22`, the group every IGMPv3 Membership Report is addressed to (RFC 3376 §4.2.14).
const ALL_IGMPV3_ROUTERS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 22);

/// Why a captured IP packet carries no IGMP message we can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadReject {
    /// Fewer bytes than the smallest possible IPv4 header.
    TooShort(usize),
    /// The IHL field names a header length that is illegal, or longer than the packet.
    HeaderLength { ihl: usize, len: usize },
    /// The IPv4 protocol field is not 2.
    NotIgmp(u8),
}

impl std::fmt::Display for PayloadReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort(len) => write!(f, "packet too short for an IPv4 header ({len} bytes)"),
            Self::HeaderLength { ihl, len } => {
                write!(f, "malformed IPv4 header (IHL={ihl} bytes, packet={len})")
            }
            Self::NotIgmp(proto) => write!(f, "IP protocol {proto} is not IGMP"),
        }
    }
}

/// Strip the IPv4 header a raw socket delivers and return the IGMP message inside.
///
/// Every index below is reachable from the wire, so each one is bounds-checked before it is
/// taken. In particular the IHL field is four bits, so a hostile packet can name a header of
/// 0..=60 bytes: without the lower bound `&raw[ihl..]` would silently hand the IP header itself
/// to the IGMP parser, and without the upper one it would be an out-of-range slice and a panic
/// inside the capture task — where `tokio::spawn` swallows it and the server goes on reporting
/// `Running`.
pub fn igmp_payload(raw: &[u8]) -> std::result::Result<&[u8], PayloadReject> {
    if raw.len() < MIN_IP_HEADER {
        return Err(PayloadReject::TooShort(raw.len()));
    }
    let ihl = (raw[0] & 0x0F) as usize * 4;
    if !(MIN_IP_HEADER..=MAX_IP_HEADER).contains(&ihl) || raw.len() < ihl {
        return Err(PayloadReject::HeaderLength {
            ihl,
            len: raw.len(),
        });
    }
    if raw[9] != IP_PROTO_IGMP {
        return Err(PayloadReject::NotIgmp(raw[9]));
    }
    Ok(&raw[ihl..])
}

/// Where an IGMP message we are about to emit has to be addressed.
///
/// This is spec-fixed, not a choice: a Membership Report goes to the group it reports
/// (RFC 2236 §9), a Leave Group to ALL-ROUTERS `224.0.0.2` (§2.9), and an IGMPv3 report to
/// `224.0.0.22` (RFC 3376 §4.2.14). Sending any of them back to whoever queried us — which is
/// what the fallback does, and what v1 and v3 reports used to get — puts a multicast control
/// message on a unicast address, where no router is listening for it.
pub fn response_destination(packet: &[u8], peer: SocketAddr) -> SocketAddr {
    if packet.len() < 8 {
        return peer;
    }
    let group = Ipv4Addr::new(packet[4], packet[5], packet[6], packet[7]);
    match packet[0] {
        0x12 | 0x16 if group.is_multicast() => SocketAddr::new(std::net::IpAddr::V4(group), 0),
        0x17 => SocketAddr::new(std::net::IpAddr::V4(ALL_ROUTERS), 0),
        0x22 => SocketAddr::new(std::net::IpAddr::V4(ALL_IGMPV3_ROUTERS), 0),
        _ => peer,
    }
}

/// Parsed IGMP message
#[derive(Debug, Clone)]
pub struct IgmpMessage {
    pub msg_type: IgmpMessageType,
    pub max_response_time: u8,
    /// The checksum field as it arrived, bytes 2-3. See [`Self::checksum_valid`].
    pub checksum: u16,
    pub group_address: Ipv4Addr,
    pub raw_data: Vec<u8>,
}

impl IgmpMessage {
    /// Parse an IGMP message from raw bytes
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(anyhow::anyhow!(
                "IGMP message too short: {} bytes",
                data.len()
            ));
        }

        let msg_type = IgmpMessageType::from_u8(data[0]).context("Unknown IGMP message type")?;

        let max_response_time = data[1];

        let checksum = u16::from_be_bytes([data[2], data[3]]);

        let group_address = Ipv4Addr::new(data[4], data[5], data[6], data[7]);

        Ok(Self {
            msg_type,
            max_response_time,
            checksum,
            group_address,
            raw_data: data.to_vec(),
        })
    }

    /// Whether the RFC 1071 checksum over the whole message is correct.
    ///
    /// Checked separately from [`Self::parse`] so the two failures stay distinguishable in the
    /// log: a message we cannot parse is not IGMP, one that fails here *is* IGMP and arrived
    /// corrupt. Acting on a corrupt message would spend an LLM round-trip — and possibly a
    /// Membership Report, which is a positive claim — on bytes no conforming receiver would
    /// have accepted.
    pub fn checksum_valid(&self) -> bool {
        crate::server::igmp::actions::igmp_checksum(&self.raw_data) == 0
    }

    /// Check if this is a general query (group address is 0.0.0.0)
    pub fn is_general_query(&self) -> bool {
        self.msg_type == IgmpMessageType::MembershipQuery && self.group_address.is_unspecified()
    }

    /// Get human-readable description
    pub fn description(&self) -> String {
        format!(
            "{} for group {} (max_resp={})",
            self.msg_type.as_str(),
            self.group_address,
            self.max_response_time
        )
    }
}

/// IGMP server state
pub struct IgmpServerState {
    /// Set of multicast groups we've joined
    pub joined_groups: HashSet<Ipv4Addr>,
}

impl IgmpServerState {
    fn new() -> Self {
        Self {
            joined_groups: HashSet::new(),
        }
    }
}

/// IGMP server that manages multicast group membership
pub struct IgmpServer;

impl IgmpServer {
    /// Spawn IGMP server with action-based LLM handling using raw IP sockets
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        Log::new(Some(&status_tx))
            .info("IGMP server starting with raw IP sockets (requires root privileges)");

        // Create raw socket for IGMP (protocol 2).
        // AF_INET/SOCK_RAW/IPPROTO_IGMP are POSIX constants available on Linux, macOS and the
        // BSDs alike; raw sockets require root (or CAP_NET_RAW on Linux) on all of them.
        //
        // The fd MUST be validated *before* handing it to `Socket::from_raw_fd`: that function's
        // safety contract requires an open, exclusively-owned descriptor, and wrapping -1 would
        // also make the eventual `Drop` call `close(-1)`.
        let raw_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_IGMP) };
        if raw_fd < 0 {
            let os_err = std::io::Error::last_os_error();
            let msg = format!(
                "Failed to create raw IGMP socket on {}: {} \
                 (IGMP needs root/CAP_NET_RAW - re-run netget with sudo)",
                listen_addr.ip(),
                os_err
            );
            Log::new(Some(&status_tx)).error(&msg);
            return Err(anyhow::anyhow!(msg));
        }
        // SAFETY: `raw_fd` was just returned by `socket(2)`, is >= 0, is open, and nothing else
        // owns it, so transferring ownership to `Socket` is sound.
        let socket = unsafe { Socket::from_raw_fd(raw_fd) };

        // Bind to the interface address (0.0.0.0 to listen on all interfaces)
        let bind_addr = std::net::SocketAddrV4::new(
            match listen_addr.ip() {
                std::net::IpAddr::V4(addr) => addr,
                std::net::IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
            },
            0, // Port is ignored for raw sockets
        );

        // Any of these can still fail (e.g. binding an address the host does not own). Report the
        // failure on both channels and propagate so the server is marked Error, never Running.
        let setup = (|| -> Result<()> {
            socket
                .set_reuse_address(true)
                .context("set_reuse_address failed")?;
            socket
                .bind(&bind_addr.into())
                .with_context(|| format!("bind to {} failed", bind_addr))?;
            socket
                .set_nonblocking(true)
                .context("set_nonblocking failed")?;
            Ok(())
        })();
        if let Err(e) = setup {
            let msg = format!("IGMP raw socket setup failed: {:#}", e);
            Log::new(Some(&status_tx)).error(&msg);
            return Err(e);
        }

        // Get the local address (for display purposes)
        let local_addr = SocketAddr::new(
            std::net::IpAddr::V4(*bind_addr.ip()),
            2, // IGMP protocol number
        );

        info!(
            "IGMP server listening on {} (raw socket, protocol 2)",
            local_addr
        );

        let protocol = Arc::new(IgmpProtocol::new());
        let server_state = Arc::new(Mutex::new(IgmpServerState::new()));

        // Convert socket2::Socket to std::net::UdpSocket for tokio
        // Even though this is a raw socket, we can wrap it as UdpSocket
        let std_socket: std::net::UdpSocket = socket.into();
        let socket = Arc::new(
            tokio::net::UdpSocket::from_std(std_socket)
                .inspect_err(|e| {
                    Log::new(Some(&status_tx)).error(format!(
                        "IGMP failed to register raw socket with tokio: {}",
                        e
                    ));
                })
                .context("failed to register IGMP raw socket with the tokio reactor")?,
        );

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 65535];
            let log = Log::new(Some(&status_tx));

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let raw_data = &buffer[..n];

                        // Raw sockets receive the whole IP packet; every bounds check that
                        // makes stripping the header safe lives in `igmp_payload`.
                        let data = match igmp_payload(raw_data) {
                            Ok(payload) => payload.to_vec(),
                            Err(reject) => {
                                debug!("IGMP discarded a capture from {}: {}", peer_addr, reject);
                                continue;
                            }
                        };
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

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
                            bytes_received: data.len() as u64,
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

                        // Parse IGMP message
                        let igmp_msg = match IgmpMessage::parse(&data) {
                            Ok(msg) => msg,
                            Err(e) => {
                                log.debug(format!(
                                    "IGMP received non-IGMP packet ({} bytes): {}",
                                    data.len(),
                                    e
                                ));
                                continue;
                            }
                        };

                        // A message that fails its own checksum is corrupt, not merely
                        // unfamiliar. Answering one would spend an LLM round-trip - and
                        // possibly a Membership Report, which is a positive membership claim -
                        // on bytes a conforming receiver would have dropped.
                        if !igmp_msg.checksum_valid() {
                            log.warn(format!(
                                "IGMP {} from {} discarded: bad checksum 0x{:04x} decision=bad_checksum",
                                igmp_msg.msg_type.as_str(),
                                peer_addr,
                                igmp_msg.checksum
                            ));
                            continue;
                        }

                        // Summary + full payload FileOnly: the igmp_* event templates
                        // render the equivalent line to the TUI.
                        log.debug(format!(
                            "IGMP received from {}: {}",
                            peer_addr,
                            igmp_msg.description()
                        ));
                        log.trace(format!("IGMP data (hex): {}", hex::encode(&data)));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();
                        let protocol_clone = protocol.clone();
                        let server_state_clone = server_state.clone();

                        tokio::spawn(async move {
                            let log = Log::new(Some(&status_clone));
                            // Determine event type and build event data
                            let (event, _event_type_ref) = match igmp_msg.msg_type {
                                IgmpMessageType::MembershipQuery => {
                                    let query_type = if igmp_msg.is_general_query() {
                                        "General"
                                    } else {
                                        "Group-Specific"
                                    };
                                    (
                                        Event::new(
                                            &IGMP_QUERY_RECEIVED_EVENT,
                                            serde_json::json!({
                                                "query_type": query_type,
                                                "group_address": igmp_msg.group_address.to_string(),
                                                "max_response_time": igmp_msg.max_response_time
                                            }),
                                        ),
                                        &IGMP_QUERY_RECEIVED_EVENT,
                                    )
                                }
                                IgmpMessageType::V1MembershipReport
                                | IgmpMessageType::V2MembershipReport
                                | IgmpMessageType::V3MembershipReport => (
                                    Event::new(
                                        &IGMP_REPORT_RECEIVED_EVENT,
                                        serde_json::json!({
                                            "group_address": igmp_msg.group_address.to_string()
                                        }),
                                    ),
                                    &IGMP_REPORT_RECEIVED_EVENT,
                                ),
                                IgmpMessageType::LeaveGroup => (
                                    Event::new(
                                        &IGMP_LEAVE_RECEIVED_EVENT,
                                        serde_json::json!({
                                            "group_address": igmp_msg.group_address.to_string()
                                        }),
                                    ),
                                    &IGMP_LEAVE_RECEIVED_EVENT,
                                ),
                            };

                            // What to do with an IGMP message is not wire-determined. Which
                            // groups to report membership in (answering a query) is a membership
                            // policy choice, not derivable from the query (a general query names
                            // group 0.0.0.0); and an observed report or leave has no spec-mandated
                            // packet response at all (report suppression / passive observation).
                            // So with no operator policy — no server instruction and no per-event
                            // handler — the spec-safe default is to advertise no memberships and
                            // stay silent, WITHOUT burning an LLM round-trip per captured packet.
                            // The model is consulted only when the operator supplies the
                            // membership policy it should apply.
                            if !operator_wants_dynamic(
                                &state_clone,
                                server_id,
                                &event.event_type.id,
                            )
                            .await
                            {
                                log.info(format!(
                                    "IGMP {} from {} ignored: no membership policy configured (static default, no LLM) decision=static_no_policy",
                                    igmp_msg.msg_type.as_str(),
                                    peer_addr
                                ));
                                return;
                            }

                            log.debug(format!(
                                "IGMP calling LLM for {} from {}",
                                igmp_msg.msg_type.as_str(),
                                peer_addr
                            ));

                            match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None,
                                &event,
                                protocol_clone.as_ref(),
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    for message in &execution_result.messages {
                                        log.info(message);
                                    }

                                    // Three outcomes have to stay apart in the log even though
                                    // all three are silent on the wire (see the Err arm below
                                    // for why silence is the only correct IGMP answer):
                                    // the model explicitly declined (`ignore_message` ->
                                    // NoAction), the model answered nothing at all, and the
                                    // backend errored.
                                    let model_declined = execution_result
                                        .protocol_results
                                        .iter()
                                        .any(|r| {
                                            matches!(
                                                r,
                                                crate::llm::actions::protocol_trait::ActionResult::NoAction
                                            )
                                        });
                                    let has_output = execution_result
                                        .protocol_results
                                        .iter()
                                        .any(|r| !r.get_all_output().is_empty());
                                    if !has_output {
                                        if model_declined {
                                            log.info(format!(
                                                "IGMP {} from {}: model chose to send nothing decision=model_ignore",
                                                igmp_msg.msg_type.as_str(),
                                                peer_addr
                                            ));
                                        } else {
                                            log.warn(format!(
                                                "IGMP {} from {}: model produced no usable action, staying silent decision=model_no_answer",
                                                igmp_msg.msg_type.as_str(),
                                                peer_addr
                                            ));
                                        }
                                    }

                                    log.debug(format!(
                                        "IGMP got {} protocol results",
                                        execution_result.protocol_results.len()
                                    ));

                                    // Process protocol results
                                    for protocol_result in &execution_result.protocol_results {
                                        if let Some(output_data) =
                                            protocol_result.get_all_output().first()
                                        {
                                            // Spec-fixed, and a pure function so it is testable
                                            // without a raw socket.
                                            let dest_addr =
                                                response_destination(output_data, peer_addr);

                                            // Send the IGMP packet via raw socket
                                            if let Err(e) =
                                                socket_clone.send_to(output_data, dest_addr).await
                                            {
                                                log.error(format!(
                                                    "Failed to send IGMP response: {}",
                                                    e
                                                ));
                                            } else {
                                                // Summary + full payload FileOnly: the
                                                // send action template already reports the
                                                // send to the TUI.
                                                log.debug(format!(
                                                    "IGMP sent {} bytes to {}",
                                                    output_data.len(),
                                                    dest_addr
                                                ));
                                                log.trace(format!(
                                                    "IGMP sent (hex): {}",
                                                    hex::encode(output_data)
                                                ));
                                            }
                                        }
                                    }

                                    // Process async custom actions (join_group/leave_group)
                                    use crate::llm::actions::protocol_trait::ActionResult;
                                    for protocol_result in &execution_result.protocol_results {
                                        if let ActionResult::Custom { name, data } = protocol_result
                                        {
                                            match name.as_str() {
                                                "igmp_join_group" => {
                                                    if let Some(group_str) = data
                                                        .get("group_address")
                                                        .and_then(|v| v.as_str())
                                                    {
                                                        if let Ok(group_addr) =
                                                            group_str.parse::<Ipv4Addr>()
                                                        {
                                                            // `joined_groups` is what makes a
                                                            // repeated join idempotent: without
                                                            // this check the second one fails
                                                            // with EADDRINUSE and is logged as
                                                            // an error, when in fact the
                                                            // membership the model asked for
                                                            // already exists.
                                                            if server_state_clone
                                                                .lock()
                                                                .await
                                                                .joined_groups
                                                                .contains(&group_addr)
                                                            {
                                                                log.debug(format!("IGMP already a member of {}; join ignored", group_addr));
                                                                continue;
                                                            }
                                                            // Join the multicast group on all interfaces (0.0.0.0)
                                                            match socket_clone.join_multicast_v4(
                                                                group_addr,
                                                                Ipv4Addr::UNSPECIFIED,
                                                            ) {
                                                                Ok(_) => {
                                                                    let mut state =
                                                                        server_state_clone
                                                                            .lock()
                                                                            .await;
                                                                    state
                                                                        .joined_groups
                                                                        .insert(group_addr);
                                                                    log.info(format!("IGMP joined multicast group {}", group_addr));
                                                                }
                                                                Err(e) => {
                                                                    log.error(format!("Failed to join multicast group {}: {}", group_addr, e));
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                "igmp_leave_group" => {
                                                    if let Some(group_str) = data
                                                        .get("group_address")
                                                        .and_then(|v| v.as_str())
                                                    {
                                                        if let Ok(group_addr) =
                                                            group_str.parse::<Ipv4Addr>()
                                                        {
                                                            // Leave the multicast group on all interfaces (0.0.0.0)
                                                            match socket_clone.leave_multicast_v4(
                                                                group_addr,
                                                                Ipv4Addr::UNSPECIFIED,
                                                            ) {
                                                                Ok(_) => {
                                                                    let mut state =
                                                                        server_state_clone
                                                                            .lock()
                                                                            .await;
                                                                    state
                                                                        .joined_groups
                                                                        .remove(&group_addr);
                                                                    log.info(format!("IGMP left multicast group {}", group_addr));
                                                                }
                                                                Err(e) => {
                                                                    log.error(format!("Failed to leave multicast group {}: {}", group_addr, e));
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    // IGMP has no error message and no way to answer without
                                    // asserting a membership. A Membership Report is a positive
                                    // claim ("this host is in group G") that makes the querier
                                    // forward that group's traffic to this segment; emitting one
                                    // we cannot substantiate is worse than not answering, and
                                    // reports/leaves have no spec response at all. So the wire
                                    // stays silent and the failure goes to the operator only —
                                    // classified, so an overload reads differently from a hard
                                    // backend fault, and never rendered onto the socket.
                                    let category =
                                        crate::utils::wire_failure::WireFailure::classify(&e);
                                    let category_tag = if category.is_overloaded() {
                                        "overloaded"
                                    } else {
                                        "unavailable"
                                    };
                                    log.error(format!(
                                        "IGMP {} from {} unanswered: LLM call failed: {} decision=fail_closed_silent category={}",
                                        igmp_msg.msg_type.as_str(),
                                        peer_addr,
                                        e,
                                        category_tag
                                    ));
                                }
                            }
                        });
                    }
                    Err(e) => {
                        // A raw socket that keeps erroring would otherwise spin this loop at
                        // 100% CPU forever, so stop the capture loop and say so loudly.
                        log.error(format!("IGMP receive error, stopping capture loop: {}", e));
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

/// Returns true if the operator opted into dynamic (LLM- or handler-driven) responses for this
/// server: either a non-empty server instruction was given, or an event handler is configured
/// for `event_id`. When false the protocol applies its static default and never consults the
/// model — for IGMP that default is to advertise no memberships and stay silent, because with no
/// configured membership policy there is nothing to report and an observed report/leave needs no
/// reply.
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
