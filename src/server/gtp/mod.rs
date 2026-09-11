//! GTP-C / GTP-U server — a mobile-core node driven by the model.
//!
//! Two UDP sockets: the control plane (2123 by default), where GTPv1-C and GTPv2-C session
//! management arrives, and the user plane (2152), where subscribers' own IP packets arrive
//! wrapped in G-PDUs. Both ports are above 1023, so this server needs no privilege at all.
//!
//! This file owns the sockets, the version demultiplex, the event payloads and — the part
//! that matters — **the fail-closed rule**. `codec.rs` owns the bytes; `actions.rs` owns the
//! model's vocabulary. See `src/server/gtp/CLAUDE.md`.
//!
//! # Fail closed
//!
//! A subscriber session is network access. When the model does not answer, or the LLM call
//! fails, this server answers with a **refusing** cause — never a success one, and never the
//! error text (`crate::utils::wire_failure` classifies; the peer gets a category, the log
//! gets the error). Echo Requests and G-PDUs get *silence* instead, because every reply they
//! define is a positive assertion: an Echo Response asserts this node is healthy, and
//! fabricating that during an outage is the `openvpn` mistake in miniature.
//!
//! Every path logs a stable `decision=` token, so `model_reject` and `fail_closed_llm_error`
//! can never be confused for one another. That is the RADIUS discipline, and the reason for
//! it is in the root `CLAUDE.md`'s OAuth2 post-mortem.

pub mod actions;
pub mod codec;

pub use actions::GtpProtocol;

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use crate::utils::wire_failure::WireFailure;

use actions::{
    GTP_CREATE_SESSION_REQUEST_EVENT, GTP_DELETE_SESSION_REQUEST_EVENT, GTP_ECHO_REQUEST_EVENT,
    GTP_GPDU_RECEIVED_EVENT, GTP_UPDATE_CONTEXT_REQUEST_EVENT,
};
use codec::{GtpV1Header, GtpV1Ie, GtpV1Message, GtpV2Header, GtpV2Ie, GtpV2Message, GtpVersion};

/// Which socket a datagram arrived on. GTP runs path management independently per plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plane {
    Control,
    User,
}

impl Plane {
    fn as_str(self) -> &'static str {
        match self {
            Plane::Control => "control",
            Plane::User => "user",
        }
    }
}

/// How a reply was arrived at.
///
/// The distinction the tokens preserve is the one OAuth2 lost: **a model that refuses and a
/// model that is unreachable must never look the same**, in the log or on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    ModelEcho,
    /// The model granted or confirmed something — a session created, an update kept.
    ModelAccept,
    /// The model refused, with a cause it chose.
    ModelReject,
    ModelGpdu,
    ModelErrorIndication,
    /// The model deliberately said nothing (`no_response`).
    ModelSilent,
    /// The model produced no usable action. The server refuses.
    FailClosedNoAction,
    /// The model's action could not be turned into a packet. The server refuses.
    FailClosedActionError,
    /// The LLM call itself failed. The server refuses.
    FailClosedLlmError,
}

impl Decision {
    /// Stable, grep-able token. `decision=fail_closed_` finds every request the model did not
    /// actually answer.
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::ModelEcho => "model_echo",
            Decision::ModelAccept => "model_accept",
            Decision::ModelReject => "model_reject",
            Decision::ModelGpdu => "model_gpdu",
            Decision::ModelErrorIndication => "model_error_indication",
            Decision::ModelSilent => "model_silent",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedActionError => "fail_closed_action_error",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
        }
    }

    pub fn is_fail_closed(self) -> bool {
        matches!(
            self,
            Decision::FailClosedNoAction
                | Decision::FailClosedActionError
                | Decision::FailClosedLlmError
        )
    }
}

/// Everything a datagram handler needs, gathered so the handlers do not take a dozen
/// arguments each.
#[derive(Clone)]
struct Shared {
    llm_client: OllamaClient,
    state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: ServerId,
    control_socket: Arc<UdpSocket>,
    user_socket: Option<Arc<UdpSocket>>,
    control_addr: SocketAddr,
    user_addr: Option<SocketAddr>,
}

impl Shared {
    fn socket_for(&self, plane: Plane) -> &Arc<UdpSocket> {
        match plane {
            Plane::User => self.user_socket.as_ref().unwrap_or(&self.control_socket),
            Plane::Control => &self.control_socket,
        }
    }

    fn local_addr(&self, plane: Plane) -> SocketAddr {
        match plane {
            Plane::User => self.user_addr.unwrap_or(self.control_addr),
            Plane::Control => self.control_addr,
        }
    }

    /// Address to advertise for this node's control plane in a GSN Address or F-TEID.
    fn advertised_control_ip(&self) -> IpAddr {
        concrete_ip(self.control_addr.ip())
    }

    /// The same for the user plane.
    fn advertised_user_ip(&self) -> IpAddr {
        concrete_ip(self.user_addr.unwrap_or(self.control_addr).ip())
    }
}

/// A wildcard bind gives an unspecified address, which is useless inside an F-TEID or a GSN
/// Address — the peer would send traffic to 0.0.0.0. Substitute the loopback so the field is
/// at least well-formed, and say so in the docs rather than pretending to know the right one.
fn concrete_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    }
}

/// What the request was, reduced to the fields a reply needs.
struct RequestContext {
    version: GtpVersion,
    message_type: u8,
    message_name: &'static str,
    sequence: u32,
    /// TEID to put in the reply header unless the model overrides it.
    default_response_teid: u32,
    /// NSAPI (GTPv1) or EPS Bearer Identity (GTPv2), when the request named one.
    bearer_id: Option<u8>,
    plane: Plane,
    peer_addr: SocketAddr,
}

pub struct GtpServer;

impl GtpServer {
    /// Bind both planes and start serving.
    ///
    /// Returns `Err` — so `server_startup` records `ServerStatus::Error` rather than a server
    /// that lies about being up — when either socket cannot be bound or a startup parameter
    /// is unusable.
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

        // Startup parameters are untrusted model/MCP input: propagate with `?`, never
        // `unwrap()`, or an undeclared key kills the task that would have reported it.
        let (requested_user_port, enable_user_plane) = match &startup_params {
            Some(params) => {
                let port = match params.get_optional_u64("user_plane_port")? {
                    Some(v) if v <= u16::MAX as u64 => Some(v as u16),
                    Some(v) => anyhow::bail!(
                        "GTP user_plane_port must be a UDP port number 0-65535, got {v}"
                    ),
                    None => None,
                };
                (
                    port,
                    params
                        .get_optional_bool("enable_user_plane")?
                        .unwrap_or(true),
                )
            }
            None => (None, true),
        };

        let control_socket =
            Arc::new(UdpSocket::bind(listen_addr).await.with_context(|| {
                format!("GTP failed to bind the control plane on {listen_addr}")
            })?);
        let control_addr = control_socket.local_addr()?;

        let log = Log::new(Some(&status_tx));
        log.info(format!("GTP-C server listening on {control_addr}"));

        let (user_socket, user_addr) = if enable_user_plane {
            // 2152 only when the control plane is on its own standard port. Anywhere else —
            // an e2e test on an ephemeral port, two instances side by side — a fixed 2152
            // would collide with whatever else is running, so take an ephemeral one.
            let port = requested_user_port.unwrap_or({
                if control_addr.port() == codec::GTPC_PORT {
                    codec::GTPU_PORT
                } else {
                    0
                }
            });
            let addr = SocketAddr::new(control_addr.ip(), port);
            let socket = Arc::new(UdpSocket::bind(addr).await.with_context(|| {
                format!(
                    "GTP failed to bind the user plane on {addr}. Pass a different \
                         user_plane_port, or enable_user_plane: false for a control-plane-only \
                         node."
                )
            })?);
            let local = socket.local_addr()?;
            // Deliberately not the words "listening on": the e2e harness scans stdout for
            // that phrase to learn a port-0 server's real port, and a second match would
            // hand it the user-plane port for the server it thinks is the control plane.
            log.info(format!("GTP-U user plane bound to {local}"));
            (Some(socket), Some(local))
        } else {
            log.info("GTP-U disabled by enable_user_plane: false; control plane only");
            (None, None)
        };

        let shared = Shared {
            llm_client,
            state: state.clone(),
            status_tx: status_tx.clone(),
            server_id,
            control_socket: control_socket.clone(),
            user_socket: user_socket.clone(),
            control_addr,
            user_addr,
        };

        let control_loop = tokio::spawn(Self::receive_loop(
            shared.clone(),
            control_socket,
            Plane::Control,
        ));
        state.register_server_task(server_id, control_loop).await;

        if let Some(socket) = user_socket {
            let user_loop = tokio::spawn(Self::receive_loop(shared, socket, Plane::User));
            state.register_server_task(server_id, user_loop).await;
        }

        Ok(control_addr)
    }

    async fn receive_loop(shared: Shared, socket: Arc<UdpSocket>, plane: Plane) {
        let local_addr = shared.local_addr(plane);
        Log::new(Some(&shared.status_tx)).info(format!(
            "GTP {} plane receive loop started on {local_addr}",
            plane.as_str()
        ));

        // A G-PDU carries a whole user IP packet, and GTP-U is routinely offered a 1500-octet
        // inner MTU plus headers. 2048 covers that with headroom; 65535 would be a jumbogram.
        let mut buffer = vec![0u8; 2048];

        loop {
            let (n, peer_addr) = match socket.recv_from(&mut buffer).await {
                Ok(pair) => pair,
                Err(e) => {
                    Log::new(Some(&shared.status_tx))
                        .error(format!("GTP {} plane receive error: {e}", plane.as_str()));
                    break;
                }
            };

            let data = buffer[..n].to_vec();
            let shared = shared.clone();
            tokio::spawn(async move {
                Self::handle_datagram(shared, data, peer_addr, plane).await;
            });
        }
    }

    async fn handle_datagram(shared: Shared, data: Vec<u8>, peer_addr: SocketAddr, plane: Plane) {
        let connection_id = Self::record_connection(&shared, peer_addr, plane, data.len()).await;

        let log = Log::new(Some(&shared.status_tx));
        log.debug(format!(
            "GTP received {} octets from {peer_addr} on the {} plane",
            data.len(),
            plane.as_str()
        ));
        log.trace(format!("GTP received (hex): {}", hex::encode(&data)));

        match codec::peek_version(&data) {
            Some(1) => Self::handle_v1(shared, data, peer_addr, plane, connection_id).await,
            Some(2) => Self::handle_v2(shared, data, peer_addr, plane, connection_id).await,
            Some(other) => {
                // TS 29.060 §11.1.1: a node that receives a message of an unsupported version
                // answers Version Not Supported, in the highest version it does support. This
                // is mechanical — there is nothing to decide — so it never reaches the model.
                warn!("GTP version {other} from {peer_addr} is not supported");
                if plane == Plane::Control {
                    let reply = GtpV1Message {
                        header: GtpV1Header::with_sequence(codec::V1_VERSION_NOT_SUPPORTED, 0, 0),
                        body: Vec::new(),
                    }
                    .encode();
                    Self::send(&shared, &reply, peer_addr, plane, connection_id).await;
                }
            }
            None => debug!("GTP empty datagram from {peer_addr} ignored"),
        }
    }

    // -----------------------------------------------------------------------
    // GTPv1
    // -----------------------------------------------------------------------

    async fn handle_v1(
        shared: Shared,
        data: Vec<u8>,
        peer_addr: SocketAddr,
        plane: Plane,
        connection_id: ConnectionId,
    ) {
        let message = match GtpV1Message::decode(&data) {
            Ok(m) => m,
            Err(e) => {
                Log::new(Some(&shared.status_tx))
                    .warn(format!("GTPv1 dropped datagram from {peer_addr}: {e}"));
                return;
            }
        };

        let message_type = message.header.message_type;
        let message_name = codec::v1_message_name(message_type);
        let sequence = message.header.sequence.unwrap_or(0) as u32;

        // A G-PDU is not IE-encoded: its body is the subscriber's own packet.
        if message_type == codec::V1_G_PDU {
            let ctx = RequestContext {
                version: GtpVersion::V1,
                message_type,
                message_name,
                sequence,
                default_response_teid: message.header.teid,
                bearer_id: None,
                plane,
                peer_addr,
            };
            let data = Self::gpdu_event_data(&message, peer_addr);
            Self::consult(shared, ctx, &GTP_GPDU_RECEIVED_EVENT, data, connection_id).await;
            return;
        }

        let ies = match codec::parse_v1_ies(&message.body) {
            Ok(ies) => ies,
            Err(e) => {
                Log::new(Some(&shared.status_tx)).warn(format!(
                    "GTPv1 {message_name} from {peer_addr} has unparseable information \
                     elements: {e}"
                ));
                return;
            }
        };

        // The peer's own control TEID is what a response header must carry. On a first
        // contact the request header's TEID is 0, so IE 17 is the only source — and this
        // server holds no context table to look it up in later, deliberately.
        let peer_control_teid = codec::find_v1(&ies, codec::V1_IE_TEID_CONTROL_PLANE)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_be_bytes);
        let peer_data_teid = codec::find_v1(&ies, codec::V1_IE_TEID_DATA_I)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_be_bytes);
        let bearer_id = codec::find_v1(&ies, codec::V1_IE_NSAPI)
            .and_then(|v| v.first().copied())
            .map(|v| v & 0x0F);

        let ctx = RequestContext {
            version: GtpVersion::V1,
            message_type,
            message_name,
            sequence,
            default_response_teid: peer_control_teid.unwrap_or(message.header.teid),
            bearer_id,
            plane,
            peer_addr,
        };

        let mut base = serde_json::json!({
            "version": 1,
            "message": message_name,
            "sequence": sequence,
            "teid": message.header.teid,
            "source_address": peer_addr.to_string(),
            "information_elements": Self::v1_ie_summary(&ies),
        });

        match message_type {
            codec::V1_ECHO_REQUEST => {
                base["plane"] = serde_json::json!(plane.as_str());
                Self::consult(shared, ctx, &GTP_ECHO_REQUEST_EVENT, base, connection_id).await;
            }
            codec::V1_CREATE_PDP_CONTEXT_REQUEST => {
                Self::add_v1_subscriber_fields(&mut base, &ies);
                base["control_teid"] = serde_json::json!(peer_control_teid);
                base["data_teid"] = serde_json::json!(peer_data_teid);
                base["nsapi"] = serde_json::json!(bearer_id);
                Self::consult(
                    shared,
                    ctx,
                    &GTP_CREATE_SESSION_REQUEST_EVENT,
                    base,
                    connection_id,
                )
                .await;
            }
            codec::V1_UPDATE_PDP_CONTEXT_REQUEST => {
                Self::add_v1_subscriber_fields(&mut base, &ies);
                base["control_teid"] = serde_json::json!(peer_control_teid);
                base["data_teid"] = serde_json::json!(peer_data_teid);
                base["nsapi"] = serde_json::json!(bearer_id);
                Self::consult(
                    shared,
                    ctx,
                    &GTP_UPDATE_CONTEXT_REQUEST_EVENT,
                    base,
                    connection_id,
                )
                .await;
            }
            codec::V1_DELETE_PDP_CONTEXT_REQUEST => {
                base["nsapi"] = serde_json::json!(bearer_id);
                base["teardown_indicator"] =
                    serde_json::json!(codec::find_v1(&ies, codec::V1_IE_TEARDOWN_IND)
                        .and_then(|v| v.first().copied())
                        .map(|v| v & 0x01 == 1));
                Self::consult(
                    shared,
                    ctx,
                    &GTP_DELETE_SESSION_REQUEST_EVENT,
                    base,
                    connection_id,
                )
                .await;
            }
            other => {
                // Responses, End Marker, Error Indication and everything else: a server must
                // not answer a response, and there is nothing to decide about a notification.
                debug!(
                    "GTPv1 {} (type {other}) from {peer_addr} noted and not answered",
                    message_name
                );
            }
        }
    }

    fn add_v1_subscriber_fields(base: &mut serde_json::Value, ies: &[GtpV1Ie]) {
        // IMSI and MSISDN are subscriber identifiers. They come from the peer and are handed
        // to the model as digits; nothing here reads any real subscriber source.
        if let Some(v) = codec::find_v1(ies, codec::V1_IE_IMSI) {
            base["imsi"] = serde_json::json!(codec::decode_tbcd(v));
        }
        if let Some(v) = codec::find_v1(ies, codec::V1_IE_MSISDN) {
            // The MSISDN IE leads with a numbering-plan octet before the TBCD digits.
            let digits = if v.len() > 1 {
                codec::decode_tbcd(&v[1..])
            } else {
                String::new()
            };
            base["msisdn"] = serde_json::json!(digits);
        }
        if let Some(v) = codec::find_v1(ies, codec::V1_IE_ACCESS_POINT_NAME) {
            if let Some(apn) = codec::decode_apn(v) {
                base["apn"] = serde_json::json!(apn);
            }
        }
        if let Some(v) = codec::find_v1(ies, codec::V1_IE_END_USER_ADDRESS) {
            let (pdp_type, addr) = codec::decode_end_user_address(v);
            base["pdp_type"] = serde_json::json!(pdp_type);
            base["requested_address"] = serde_json::json!(addr.map(|a| a.to_string()));
        }
        if let Some(v) = codec::find_v1(ies, codec::V1_IE_RAT_TYPE) {
            if let Some(rat) = v.first() {
                base["rat_type"] = serde_json::json!(codec::rat_type_name(GtpVersion::V1, *rat));
            }
        }
        // The first GSN Address is the peer's control plane (TS 29.060 §7.3.1).
        if let Some(v) = codec::find_v1(ies, codec::V1_IE_GSN_ADDRESS) {
            if let Some(addr) = gsn_address(v) {
                base["peer_address"] = serde_json::json!(addr.to_string());
            }
        }
    }

    fn v1_ie_summary(ies: &[GtpV1Ie]) -> serde_json::Value {
        serde_json::Value::Array(
            ies.iter()
                .map(|ie| {
                    serde_json::json!({
                        "type": ie.ie_type,
                        "name": codec::v1_ie_name(ie.ie_type),
                        "length": ie.value.len(),
                    })
                })
                .collect(),
        )
    }

    fn gpdu_event_data(message: &GtpV1Message, peer_addr: SocketAddr) -> serde_json::Value {
        let mut data = serde_json::json!({
            "teid": message.header.teid,
            "sequence": message.header.sequence,
            "source_address": peer_addr.to_string(),
        });

        if let Some(inner) = codec::decode_inner_ip(&message.body) {
            data["inner_ip"] = serde_json::json!({
                "version": inner.version,
                "source": inner.source.to_string(),
                "destination": inner.destination.to_string(),
                "protocol": inner.protocol,
                "protocol_name": inner.protocol_name,
                "ttl": inner.ttl,
                "length": inner.length,
                "source_port": inner.source_port,
                "destination_port": inner.destination_port,
            });
            if let Some(offset) = inner.payload_offset {
                if offset < message.body.len() {
                    let payload = &message.body[offset..];
                    let printable = payload
                        .iter()
                        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace());
                    if printable {
                        data["payload"] =
                            serde_json::json!(String::from_utf8_lossy(payload).to_string());
                        data["payload_encoding"] = serde_json::json!("utf8");
                    } else {
                        data["payload"] = serde_json::json!(hex::encode(payload));
                        data["payload_encoding"] = serde_json::json!("hex");
                    }
                }
            }
        }

        data
    }

    // -----------------------------------------------------------------------
    // GTPv2-C
    // -----------------------------------------------------------------------

    async fn handle_v2(
        shared: Shared,
        data: Vec<u8>,
        peer_addr: SocketAddr,
        plane: Plane,
        connection_id: ConnectionId,
    ) {
        if plane == Plane::User {
            // GTP-U is version 1 only; there is no GTPv2 user plane.
            warn!("GTPv2 datagram arrived on the user plane from {peer_addr}; dropped");
            return;
        }

        let message = match GtpV2Message::decode(&data) {
            Ok(m) => m,
            Err(e) => {
                Log::new(Some(&shared.status_tx))
                    .warn(format!("GTPv2 dropped datagram from {peer_addr}: {e}"));
                return;
            }
        };

        let message_type = message.header.message_type;
        let message_name = codec::v2_message_name(message_type);
        let header_teid = message.header.teid.unwrap_or(0);

        let sender_fteid = message
            .find_instance(codec::V2_IE_FTEID, 0)
            .and_then(codec::decode_fteid);
        let bearer_id = message
            .find(codec::V2_IE_BEARER_CONTEXT)
            .and_then(|v| codec::parse_v2_grouped(v).ok())
            .and_then(|inner| {
                inner
                    .iter()
                    .find(|ie| ie.ie_type == codec::V2_IE_EBI)
                    .and_then(|ie| ie.value.first().copied())
            })
            .or_else(|| {
                message
                    .find(codec::V2_IE_EBI)
                    .and_then(|v| v.first().copied())
            })
            .map(|v| v & 0x0F);

        let ctx = RequestContext {
            version: GtpVersion::V2,
            message_type,
            message_name,
            sequence: message.header.sequence,
            default_response_teid: sender_fteid.map(|f| f.1).unwrap_or(header_teid),
            bearer_id,
            plane,
            peer_addr,
        };

        let mut base = serde_json::json!({
            "version": 2,
            "message": message_name,
            "sequence": message.header.sequence,
            "teid": header_teid,
            "source_address": peer_addr.to_string(),
            "information_elements": serde_json::Value::Array(
                message.ies.iter().map(|ie| serde_json::json!({
                    "type": ie.ie_type,
                    "name": codec::v2_ie_name(ie.ie_type),
                    "instance": ie.instance,
                    "length": ie.value.len(),
                })).collect(),
            ),
        });

        match message_type {
            codec::V2_ECHO_REQUEST => {
                base["plane"] = serde_json::json!(plane.as_str());
                Self::consult(shared, ctx, &GTP_ECHO_REQUEST_EVENT, base, connection_id).await;
            }
            codec::V2_CREATE_SESSION_REQUEST => {
                Self::add_v2_subscriber_fields(&mut base, &message, sender_fteid);
                base["nsapi"] = serde_json::json!(bearer_id);
                Self::consult(
                    shared,
                    ctx,
                    &GTP_CREATE_SESSION_REQUEST_EVENT,
                    base,
                    connection_id,
                )
                .await;
            }
            codec::V2_MODIFY_BEARER_REQUEST => {
                Self::add_v2_subscriber_fields(&mut base, &message, sender_fteid);
                base["nsapi"] = serde_json::json!(bearer_id);
                Self::consult(
                    shared,
                    ctx,
                    &GTP_UPDATE_CONTEXT_REQUEST_EVENT,
                    base,
                    connection_id,
                )
                .await;
            }
            codec::V2_DELETE_SESSION_REQUEST => {
                base["nsapi"] = serde_json::json!(bearer_id);
                Self::consult(
                    shared,
                    ctx,
                    &GTP_DELETE_SESSION_REQUEST_EVENT,
                    base,
                    connection_id,
                )
                .await;
            }
            other => debug!(
                "GTPv2 {} (type {other}) from {peer_addr} noted and not answered",
                message_name
            ),
        }
    }

    fn add_v2_subscriber_fields(
        base: &mut serde_json::Value,
        message: &GtpV2Message,
        sender_fteid: Option<(u8, u32, Option<IpAddr>)>,
    ) {
        if let Some(v) = message.find(codec::V2_IE_IMSI) {
            base["imsi"] = serde_json::json!(codec::decode_tbcd(v));
        }
        if let Some(v) = message.find(codec::V2_IE_MSISDN) {
            base["msisdn"] = serde_json::json!(codec::decode_tbcd(v));
        }
        if let Some(v) = message.find(codec::V2_IE_APN) {
            if let Some(apn) = codec::decode_apn(v) {
                base["apn"] = serde_json::json!(apn);
            }
        }
        if let Some(v) = message.find(codec::V2_IE_PAA) {
            let (pdn_type, addr) = codec::decode_paa(v);
            base["pdp_type"] = serde_json::json!(pdn_type);
            base["requested_address"] = serde_json::json!(addr.map(|a| a.to_string()));
        }
        if let Some(v) = message.find(codec::V2_IE_RAT_TYPE) {
            if let Some(rat) = v.first() {
                base["rat_type"] = serde_json::json!(codec::rat_type_name(GtpVersion::V2, *rat));
            }
        }
        if let Some((_, teid, addr)) = sender_fteid {
            base["control_teid"] = serde_json::json!(teid);
            base["peer_address"] = serde_json::json!(addr.map(|a| a.to_string()));
        }
        // The user-plane F-TEID lives inside the Bearer Context.
        if let Some(inner) = message
            .find(codec::V2_IE_BEARER_CONTEXT)
            .and_then(|v| codec::parse_v2_grouped(v).ok())
        {
            if let Some(fteid) = inner
                .iter()
                .find(|ie| ie.ie_type == codec::V2_IE_FTEID)
                .and_then(|ie| codec::decode_fteid(&ie.value))
            {
                base["data_teid"] = serde_json::json!(fteid.1);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Ask the model, then apply the fail-closed rule
    // -----------------------------------------------------------------------

    async fn consult(
        shared: Shared,
        ctx: RequestContext,
        event_type: &'static crate::protocol::EventType,
        event_data: serde_json::Value,
        connection_id: ConnectionId,
    ) {
        let protocol = GtpProtocol::new();
        let event = Event::new(event_type, event_data);

        let outcome = call_llm(
            &shared.llm_client,
            &shared.state,
            shared.server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await;

        let (decision, reply) = Self::decide(&shared, &ctx, outcome);

        let summary = format!(
            "GTP v{} {} seq={} from {} decision={}",
            ctx.version.as_number(),
            ctx.message_name,
            ctx.sequence,
            ctx.peer_addr,
            decision.as_str()
        );
        let log = Log::new(Some(&shared.status_tx));
        if decision.is_fail_closed() {
            log.error(format!(
                "{summary} (refused because no usable decision was produced)"
            ));
        } else {
            log.info(&summary);
        }

        if let Some((bytes, plane)) = reply {
            Self::send(&shared, &bytes, ctx.peer_addr, plane, connection_id).await;
        }
    }

    /// **The fail-closed rule.** Returns the decision and, if anything is to be sent, the
    /// bytes and the plane to send them on.
    ///
    /// - A usable model action is rendered verbatim.
    /// - No usable action, or an LLM error, on a *session* request produces a synthesised
    ///   response carrying a **refusing** cause, tagged `fail_closed_*` — never
    ///   `model_reject`, because a model that denies and a backend that is down must stay
    ///   distinguishable.
    /// - Echo Requests and G-PDUs get silence instead: every reply they define asserts
    ///   something positive (this node is healthy; here is a subscriber's traffic), and
    ///   inventing that during an outage is worse than saying nothing.
    fn decide(
        shared: &Shared,
        ctx: &RequestContext,
        outcome: Result<crate::llm::ExecutionResult>,
    ) -> (Decision, Option<(Vec<u8>, Plane)>) {
        let log = Log::new(Some(&shared.status_tx));

        let execution = match outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }
                result
            }
            Err(e) => {
                // The peer gets a category, the log gets the error. `WireFailure` is passed
                // the error only to classify it; nothing derived from it reaches the wire.
                let category = WireFailure::classify(&e);
                log.error(format!(
                    "GTP LLM call failed for {} from {} (category={}): {e}",
                    ctx.message_name,
                    ctx.peer_addr,
                    if category.is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    }
                ));
                return (
                    Decision::FailClosedLlmError,
                    Self::synthesised_refusal(ctx, category),
                );
            }
        };

        let mut first_error: Option<String> = None;
        for result in &execution.protocol_results {
            let ActionResult::Custom { name, data } = result else {
                continue;
            };
            match Self::render(shared, ctx, name, data) {
                Ok(Some(rendered)) => return rendered,
                Ok(None) => {}
                Err(e) => {
                    log.error(format!(
                        "GTP could not encode the model's {name} for {} from {}: {e}",
                        ctx.message_name, ctx.peer_addr
                    ));
                    first_error.get_or_insert(e.to_string());
                }
            }
        }

        if first_error.is_some() {
            return (
                Decision::FailClosedActionError,
                Self::synthesised_refusal(ctx, WireFailure::Unavailable),
            );
        }
        (
            Decision::FailClosedNoAction,
            Self::synthesised_refusal(ctx, WireFailure::Unavailable),
        )
    }

    /// Read the model's `sequence` override, refusing anything wider than the header field.
    ///
    /// Returns the request's own sequence when the model omitted it, which is the documented
    /// default and almost always what a response wants.
    fn checked_sequence(
        data: &serde_json::Value,
        version: GtpVersion,
        fallback: u32,
    ) -> Result<u32> {
        match data.get("sequence").and_then(|v| v.as_u64()) {
            Some(v) if v > version.max_sequence() as u64 => anyhow::bail!(
                "sequence {v} does not fit a GTPv{} header: the field is {} bits, so the \
                 largest value is {}. Copy the request's own sequence from the event, or \
                 omit 'sequence' entirely and the server echoes it for you.",
                version.as_number(),
                version.sequence_bits(),
                version.max_sequence()
            ),
            Some(v) => Ok(v as u32),
            None => Ok(fallback),
        }
    }

    /// Turn one `ActionResult::Custom` into wire bytes.
    ///
    /// `Ok(None)` means "this result is not one of ours"; `Err` means the model asked for
    /// something that cannot be encoded, which fails closed.
    #[allow(clippy::type_complexity)]
    fn render(
        shared: &Shared,
        ctx: &RequestContext,
        name: &str,
        data: &serde_json::Value,
    ) -> Result<Option<(Decision, Option<(Vec<u8>, Plane)>)>> {
        // Checked against the width of the field it is going into, not against `u32`.
        // `optional_u32` in actions.rs cannot do this: a sequence number is 16 bits in
        // GTPv1 and 24 in GTPv2, and only `mod.rs` knows which version is being answered.
        // Without the check `sequence as u16` turned a model's 70000 into 4464, which the
        // peer cannot match to any outstanding request — so the response is discarded and
        // the request times out, a failure that reads as a lost packet rather than a bad
        // field.
        let sequence = Self::checked_sequence(data, ctx.version, ctx.sequence)?;
        let teid = data
            .get("teid")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(ctx.default_response_teid);

        match name {
            actions::RESULT_NO_RESPONSE => Ok(Some((Decision::ModelSilent, None))),

            actions::RESULT_ECHO_RESPONSE => {
                let recovery = data.get("recovery").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                let bytes = match ctx.version {
                    GtpVersion::V1 => encode_v1(
                        codec::V1_ECHO_RESPONSE,
                        0,
                        sequence,
                        &[ie(codec::V1_IE_RECOVERY, vec![recovery])],
                    ),
                    GtpVersion::V2 => GtpV2Message {
                        header: GtpV2Header::without_teid(codec::V2_ECHO_RESPONSE, sequence),
                        ies: vec![GtpV2Ie::new(codec::V2_IE_RECOVERY, 0, vec![recovery])],
                    }
                    .encode(),
                };
                Ok(Some((Decision::ModelEcho, Some((bytes, ctx.plane)))))
            }

            actions::RESULT_CREATE_RESPONSE
            | actions::RESULT_UPDATE_RESPONSE
            | actions::RESULT_DELETE_RESPONSE => {
                let (cause, accepts) = cause_for(ctx.version, data)?;
                let decision = if accepts {
                    Decision::ModelAccept
                } else {
                    Decision::ModelReject
                };
                let bytes =
                    Self::encode_session_response(shared, ctx, name, sequence, teid, cause, data)?;
                Ok(Some((decision, Some((bytes, Plane::Control)))))
            }

            actions::RESULT_ERROR_INDICATION => {
                let target = data
                    .get("teid")
                    .and_then(|v| v.as_u64())
                    .context("gtp_error_indication result carries no teid")?
                    as u32;
                // TS 29.060 §7.3.7: the Error Indication carries the offending TEID in a
                // Tunnel Endpoint Identifier Data I IE and the sender's GSN Address, with a
                // header TEID of 0.
                let mut ies = vec![ie(codec::V1_IE_TEID_DATA_I, target.to_be_bytes().to_vec())];
                ies.push(gsn_address_ie(shared.advertised_user_ip()));
                let bytes = encode_v1(codec::V1_ERROR_INDICATION, 0, sequence, &ies);
                Ok(Some((
                    Decision::ModelErrorIndication,
                    Some((bytes, Plane::User)),
                )))
            }

            actions::RESULT_GPDU => {
                let target =
                    data.get("teid")
                        .and_then(|v| v.as_u64())
                        .context("gtp_gpdu result carries no teid")? as u32;
                let payload = hex::decode(
                    data.get("payload_hex")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default(),
                )
                .context("gtp_gpdu payload is not valid hex")?;
                if payload.len() > codec::MAX_GPDU_PAYLOAD_LEN {
                    anyhow::bail!(
                        "G-PDU payload is {} octets, over the {}-octet limit. GTP-U's Length \
                         field is 16 bits and the datagram has to fit one UDP packet, so a \
                         larger T-PDU would wrap the length field and then fail to send at \
                         all. A T-PDU is one IP packet — send several G-PDUs instead.",
                        payload.len(),
                        codec::MAX_GPDU_PAYLOAD_LEN
                    );
                }
                // A G-PDU is always GTPv1 whatever version the surrounding context is, so
                // re-check against the 16-bit field rather than reusing `sequence`, which
                // was checked against `ctx.version`.
                let header = match data.get("sequence") {
                    Some(serde_json::Value::Null) | None => {
                        GtpV1Header::new(codec::V1_G_PDU, target)
                    }
                    Some(_) => {
                        let seq = Self::checked_sequence(data, GtpVersion::V1, ctx.sequence)?;
                        GtpV1Header::with_sequence(codec::V1_G_PDU, target, seq as u16)
                    }
                };
                let bytes = GtpV1Message {
                    header,
                    body: payload,
                }
                .encode();
                Ok(Some((Decision::ModelGpdu, Some((bytes, Plane::User)))))
            }

            _ => Ok(None),
        }
    }

    fn encode_session_response(
        shared: &Shared,
        ctx: &RequestContext,
        result_name: &str,
        sequence: u32,
        teid: u32,
        cause: u8,
        data: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        let accepts = codec::cause_accepts(ctx.version, cause);
        let assigned = json_ip(data, "assigned_address")?;
        let control_teid = json_u32(data, "control_teid");
        let data_teid = json_u32(data, "data_teid");
        let charging_id = json_u32(data, "charging_id");
        let recovery = data
            .get("recovery")
            .and_then(|v| v.as_u64())
            .map(|v| v as u8);
        let dns: Vec<IpAddr> = data
            .get("dns_servers")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str())
                    .filter_map(|s| s.parse().ok())
                    .collect()
            })
            .unwrap_or_default();

        let is_delete = result_name == actions::RESULT_DELETE_RESPONSE;

        match ctx.version {
            GtpVersion::V1 => {
                let message_type = match result_name {
                    actions::RESULT_CREATE_RESPONSE => codec::V1_CREATE_PDP_CONTEXT_RESPONSE,
                    actions::RESULT_UPDATE_RESPONSE => codec::V1_UPDATE_PDP_CONTEXT_RESPONSE,
                    _ => codec::V1_DELETE_PDP_CONTEXT_RESPONSE,
                };

                // GTPv1 requires information elements in ascending type order.
                let mut ies = vec![ie(codec::V1_IE_CAUSE, vec![cause])];
                if !is_delete && accepts {
                    // Reordering Required is mandatory in a Create PDP Context Response.
                    // 0xFE = spare bits set, reordering not required.
                    ies.push(ie(codec::V1_IE_REORDERING_REQUIRED, vec![0xFE]));
                }
                if let Some(r) = recovery {
                    ies.push(ie(codec::V1_IE_RECOVERY, vec![r]));
                }
                if !is_delete {
                    if let Some(t) = data_teid {
                        ies.push(ie(codec::V1_IE_TEID_DATA_I, t.to_be_bytes().to_vec()));
                    }
                    if let Some(t) = control_teid {
                        ies.push(ie(
                            codec::V1_IE_TEID_CONTROL_PLANE,
                            t.to_be_bytes().to_vec(),
                        ));
                    }
                    if let Some(c) = charging_id {
                        ies.push(ie(codec::V1_IE_CHARGING_ID, c.to_be_bytes().to_vec()));
                    }
                    if let Some(addr) = assigned {
                        ies.push(ie(
                            codec::V1_IE_END_USER_ADDRESS,
                            codec::encode_end_user_address(Some(addr)),
                        ));
                    }
                }
                if !dns.is_empty() {
                    ies.push(ie(
                        codec::V1_IE_PROTOCOL_CONFIG_OPTIONS,
                        codec::encode_pco_dns(&dns),
                    ));
                }
                if !is_delete && accepts {
                    ies.push(gsn_address_ie(shared.advertised_control_ip()));
                    ies.push(gsn_address_ie(shared.advertised_user_ip()));
                }

                Ok(encode_v1(message_type, teid, sequence, &ies))
            }
            GtpVersion::V2 => {
                let message_type = match result_name {
                    actions::RESULT_CREATE_RESPONSE => codec::V2_CREATE_SESSION_RESPONSE,
                    actions::RESULT_UPDATE_RESPONSE => codec::V2_MODIFY_BEARER_RESPONSE,
                    _ => codec::V2_DELETE_SESSION_RESPONSE,
                };

                // TS 29.274 §8.4: the Cause IE is two octets — the value, then a flags octet.
                let mut ies = vec![GtpV2Ie::new(codec::V2_IE_CAUSE, 0, vec![cause, 0])];
                if let Some(r) = recovery {
                    ies.push(GtpV2Ie::new(codec::V2_IE_RECOVERY, 0, vec![r]));
                }
                if !is_delete {
                    if !dns.is_empty() {
                        ies.push(GtpV2Ie::new(
                            codec::V2_IE_PCO,
                            0,
                            codec::encode_pco_dns(&dns),
                        ));
                    }
                    if let Some(addr) = assigned {
                        ies.push(GtpV2Ie::new(codec::V2_IE_PAA, 0, codec::encode_paa(addr)));
                    }
                    if let Some(t) = control_teid {
                        // Interface type 7: PGW S5/S8 GTP-C.
                        ies.push(GtpV2Ie::new(
                            codec::V2_IE_FTEID,
                            1,
                            codec::encode_fteid(7, t, shared.advertised_control_ip()),
                        ));
                    }
                    if let Some(t) = data_teid {
                        // Bearer Context created: EBI, Cause, and the S5/S8-U PGW F-TEID
                        // (interface type 5) the peer must send user traffic to.
                        let inner = vec![
                            GtpV2Ie::new(codec::V2_IE_EBI, 0, vec![ctx.bearer_id.unwrap_or(5)]),
                            GtpV2Ie::new(codec::V2_IE_CAUSE, 0, vec![cause, 0]),
                            GtpV2Ie::new(
                                codec::V2_IE_FTEID,
                                2,
                                codec::encode_fteid(5, t, shared.advertised_user_ip()),
                            ),
                        ];
                        ies.push(GtpV2Ie::new(
                            codec::V2_IE_BEARER_CONTEXT,
                            0,
                            codec::encode_v2_grouped(&inner),
                        ));
                    }
                }

                Ok(GtpV2Message {
                    header: GtpV2Header::new(message_type, teid, sequence),
                    ies,
                }
                .encode())
            }
        }
    }

    /// The refusal sent when the model did not decide.
    ///
    /// Deliberately *not* the same shape as a model refusal: it carries a cause only, no
    /// address and no TEIDs, so the two denial paths differ on the wire as well as in the
    /// log. `Overloaded` maps to "no resources available", which a peer may retry;
    /// `Unavailable` maps to "system failure", which it should not.
    fn synthesised_refusal(
        ctx: &RequestContext,
        category: WireFailure,
    ) -> Option<(Vec<u8>, Plane)> {
        let response_type = response_type(ctx.version, ctx.message_type)?;

        // Echo and the user plane assert something positive, so there is nothing safe to
        // synthesise: stay silent and let the peer draw its own conclusion.
        let is_session_response = match ctx.version {
            GtpVersion::V1 => matches!(
                response_type,
                codec::V1_CREATE_PDP_CONTEXT_RESPONSE
                    | codec::V1_UPDATE_PDP_CONTEXT_RESPONSE
                    | codec::V1_DELETE_PDP_CONTEXT_RESPONSE
            ),
            GtpVersion::V2 => matches!(
                response_type,
                codec::V2_CREATE_SESSION_RESPONSE
                    | codec::V2_MODIFY_BEARER_RESPONSE
                    | codec::V2_DELETE_SESSION_RESPONSE
            ),
        };
        if !is_session_response {
            return None;
        }

        let name = if category.is_overloaded() {
            "no_resources_available"
        } else {
            "system_failure"
        };
        let cause = codec::cause_by_name(name)?;
        debug_assert!(!cause.accepts, "a fail-closed cause must never accept");

        let bytes = match ctx.version {
            GtpVersion::V1 => encode_v1(
                response_type,
                ctx.default_response_teid,
                ctx.sequence,
                &[ie(codec::V1_IE_CAUSE, vec![cause.v1])],
            ),
            GtpVersion::V2 => GtpV2Message {
                header: GtpV2Header::new(response_type, ctx.default_response_teid, ctx.sequence),
                ies: vec![GtpV2Ie::new(codec::V2_IE_CAUSE, 0, vec![cause.v2, 0])],
            }
            .encode(),
        };
        Some((bytes, Plane::Control))
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    async fn record_connection(
        shared: &Shared,
        peer_addr: SocketAddr,
        plane: Plane,
        bytes: usize,
    ) -> ConnectionId {
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };

        let connection_id = ConnectionId::new(shared.state.get_next_unified_id().await);
        let now = std::time::Instant::now();
        shared
            .state
            .add_connection_to_server(
                shared.server_id,
                ServerConnectionState {
                    id: connection_id,
                    remote_addr: peer_addr,
                    local_addr: shared.local_addr(plane),
                    bytes_sent: 0,
                    bytes_received: bytes as u64,
                    packets_sent: 0,
                    packets_received: 1,
                    last_activity: now,
                    status: ConnectionStatus::Active,
                    status_changed_at: now,
                    protocol_info: ProtocolConnectionInfo::empty(),
                },
            )
            .await;
        let _ = shared.status_tx.send("__UPDATE_UI__".to_string());
        connection_id
    }

    async fn send(
        shared: &Shared,
        bytes: &[u8],
        peer_addr: SocketAddr,
        plane: Plane,
        connection_id: ConnectionId,
    ) {
        if plane == Plane::User && shared.user_socket.is_none() {
            warn!(
                "GTP wanted to send {} octets on the user plane to {peer_addr}, but the user \
                 plane is disabled; sending on the control plane instead",
                bytes.len()
            );
        }
        let socket = shared.socket_for(plane);
        if let Err(e) = socket.send_to(bytes, peer_addr).await {
            Log::new(Some(&shared.status_tx)).error(format!("GTP send to {peer_addr} failed: {e}"));
            return;
        }

        shared
            .state
            .update_connection_stats(
                shared.server_id,
                connection_id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;

        let log = Log::new(Some(&shared.status_tx));
        log.debug(format!(
            "GTP sent {} octets to {peer_addr} on the {} plane",
            bytes.len(),
            plane.as_str()
        ));
        log.trace(format!("GTP sent (hex): {}", hex::encode(bytes)));
        let _ = shared
            .status_tx
            .send(format!("→ GTP response to {peer_addr}"));
    }
}

// ===========================================================================
// Small helpers
// ===========================================================================

fn ie(ie_type: u8, value: Vec<u8>) -> GtpV1Ie {
    GtpV1Ie { ie_type, value }
}

fn gsn_address_ie(addr: IpAddr) -> GtpV1Ie {
    let value = match addr {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    ie(codec::V1_IE_GSN_ADDRESS, value)
}

/// A GSN Address IE is a bare 4- or 16-octet address (TS 29.060 §7.7.32).
fn gsn_address(value: &[u8]) -> Option<IpAddr> {
    match value.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(
            value[0], value[1], value[2], value[3],
        ))),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(value);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

/// Every GTP-C signalling message carries a sequence number, so the S flag is always set —
/// and with it, by the all-or-nothing rule, all four optional octets.
fn encode_v1(message_type: u8, teid: u32, sequence: u32, ies: &[GtpV1Ie]) -> Vec<u8> {
    GtpV1Message {
        header: GtpV1Header::with_sequence(message_type, teid, sequence as u16),
        body: codec::encode_v1_ies(ies),
    }
    .encode()
}

/// Which response answers which request.
fn response_type(version: GtpVersion, request_type: u8) -> Option<u8> {
    Some(match (version, request_type) {
        (GtpVersion::V1, codec::V1_ECHO_REQUEST) => codec::V1_ECHO_RESPONSE,
        (GtpVersion::V1, codec::V1_CREATE_PDP_CONTEXT_REQUEST) => {
            codec::V1_CREATE_PDP_CONTEXT_RESPONSE
        }
        (GtpVersion::V1, codec::V1_UPDATE_PDP_CONTEXT_REQUEST) => {
            codec::V1_UPDATE_PDP_CONTEXT_RESPONSE
        }
        (GtpVersion::V1, codec::V1_DELETE_PDP_CONTEXT_REQUEST) => {
            codec::V1_DELETE_PDP_CONTEXT_RESPONSE
        }
        (GtpVersion::V2, codec::V2_ECHO_REQUEST) => codec::V2_ECHO_RESPONSE,
        (GtpVersion::V2, codec::V2_CREATE_SESSION_REQUEST) => codec::V2_CREATE_SESSION_RESPONSE,
        (GtpVersion::V2, codec::V2_MODIFY_BEARER_REQUEST) => codec::V2_MODIFY_BEARER_RESPONSE,
        (GtpVersion::V2, codec::V2_DELETE_SESSION_REQUEST) => codec::V2_DELETE_SESSION_RESPONSE,
        _ => return None,
    })
}

/// Pick the cause code for this version out of the normalised action result.
fn cause_for(version: GtpVersion, data: &serde_json::Value) -> Result<(u8, bool)> {
    let cause = data
        .get("cause")
        .context("session response result carries no cause")?;
    let key = match version {
        GtpVersion::V1 => "v1",
        GtpVersion::V2 => "v2",
    };
    let value = cause
        .get(key)
        .and_then(|v| v.as_u64())
        .with_context(|| format!("session response cause has no {key} code"))?
        as u8;
    Ok((value, codec::cause_accepts(version, value)))
}

fn json_u32(data: &serde_json::Value, key: &str) -> Option<u32> {
    data.get(key).and_then(|v| v.as_u64()).map(|v| v as u32)
}

fn json_ip(data: &serde_json::Value, key: &str) -> Result<Option<IpAddr>> {
    match data.get(key).and_then(|v| v.as_str()) {
        None => Ok(None),
        Some(s) => s
            .parse::<IpAddr>()
            .map(Some)
            .with_context(|| format!("'{key}' is not an IP address: {s:?}")),
    }
}
