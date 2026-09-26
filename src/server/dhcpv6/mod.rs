//! DHCPv6 (RFC 8415) server.
//!
//! One UDP socket on port 547. Every datagram is decoded, turned into the event for its message
//! type, and answered by whatever the model (or a script, or a static handler) says — or by
//! nothing at all, which for this protocol is a first-class answer rather than a failure mode.
//! See `CLAUDE.md` in this directory.

pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::{anyhow, Result};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType};
use crate::server::Dhcpv6Protocol;
use crate::state::app_state::AppState;
use actions::{
    describe_duid, Dhcpv6RequestContext, DHCPV6_INFORMATION_REQUEST_EVENT, DHCPV6_REBIND_EVENT,
    DHCPV6_RELEASE_EVENT, DHCPV6_RENEW_EVENT, DHCPV6_REQUEST_EVENT, DHCPV6_SOLICIT_EVENT,
};

use dhcproto::v6;
use dhcproto::{Decodable, Decoder};

/// `All_DHCP_Relay_Agents_and_Servers` (RFC 8415 §7.1) — where clients send.
pub const ALL_DHCP_RELAY_AGENTS_AND_SERVERS: Ipv6Addr =
    Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0x0001, 0x0002);

/// A decoded client message plus everything the event publishes about it.
struct ParsedMessage {
    context: Dhcpv6RequestContext,
    event_data: serde_json::Value,
}

/// DHCPv6 server that forwards client messages to the LLM.
pub struct Dhcpv6Server;

impl Dhcpv6Server {
    /// Bind and start serving. Returns the bound address, or `Err` if the socket could not be
    /// opened — `server_startup` turns that into `ServerStatus::Error` rather than a server
    /// that sits in `Running` having bound nothing.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        multicast_interface_index: u32,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        // DHCPv6 has no IPv4 form. Binding one would produce a server that can never be
        // reached by a client, so say so instead of listening on the wrong family.
        let bind_ip = match listen_addr.ip() {
            IpAddr::V6(ip) => ip,
            IpAddr::V4(ip) => {
                return Err(anyhow!(
                    "DHCPv6 is IPv6-only and cannot be served on the IPv4 address {ip}. Use the \
                     default host \"::1\", or \"::\" to serve a real link"
                ))
            }
        };

        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;

        // Real clients multicast to FF02::1:2. Only worth attempting when we are bound to the
        // unspecified address: joining a group on a loopback-only socket is meaningless, and on
        // most systems fails. A failure is logged and never fatal — unicast still works, which
        // is what every test and every relayed deployment uses.
        if bind_ip.is_unspecified() {
            match socket.join_multicast_v6(
                &ALL_DHCP_RELAY_AGENTS_AND_SERVERS,
                multicast_interface_index,
            ) {
                Ok(()) => Log::new(Some(&status_tx)).info(format!(
                    "DHCPv6 joined {} on interface index {}",
                    ALL_DHCP_RELAY_AGENTS_AND_SERVERS, multicast_interface_index
                )),
                Err(e) => Log::new(Some(&status_tx)).warn(format!(
                    "DHCPv6 could not join {} on interface index {}: {}. Unicast and relayed \
                     traffic still reach this server; clients multicasting to the group will not",
                    ALL_DHCP_RELAY_AGENTS_AND_SERVERS, multicast_interface_index, e
                )),
            }
        }

        Log::new(Some(&status_tx)).info(format!("DHCPv6 server listening on {}", local_addr));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 1500];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();

                        Log::new(Some(&status_tx))
                            .debug(format!("DHCPv6 received {} bytes from {}", n, peer_addr));
                        Log::new(Some(&status_tx))
                            .trace(format!("DHCPv6 data (hex): {}", hex::encode(&data)));

                        // A datagram that does not decode never reaches the model: the event
                        // would carry nothing usable and no reply could be built from it, since
                        // there would be no transaction id to echo. Same for a message type
                        // this server has no event for — spending an LLM round trip on one
                        // buys an answer that cannot be sent.
                        let Some(parsed) = Self::parse_message(&data, peer_addr) else {
                            Log::new(Some(&status_tx)).warn(format!(
                                "Dropping datagram ({} bytes) from {}: not a DHCPv6 client \
                                 message this server answers",
                                n, peer_addr
                            ));
                            continue;
                        };

                        let Some(event_type) = Self::event_for(parsed.context.msg_type) else {
                            Log::new(Some(&status_tx)).warn(format!(
                                "Dropping DHCPv6 {:?} from {}: no event is defined for this \
                                 message type, so no reply can be produced",
                                parsed.context.msg_type, peer_addr
                            ));
                            continue;
                        };

                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

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

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();
                        let ParsedMessage {
                            context,
                            event_data,
                        } = parsed;

                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner.spawn_server_task(server_id, async move {
                            // One protocol instance per datagram. The instance carries the
                            // transaction id, client DUID and IAIDs used to build the reply, so
                            // two clients whose LLM calls overlap can never echo each other's.
                            let protocol = Dhcpv6Protocol::new();
                            let message_type = format!("{:?}", context.msg_type);
                            protocol.set_request_context(context);

                            let event = Event::new(event_type, event_data);
                            let log = Log::new(Some(&status_clone));

                            log.debug(format!(
                                "DHCPv6 calling LLM for {} from {}",
                                message_type, peer_addr
                            ));

                            // ── What a failed message is answered with ─────────────────────────
                            //
                            // A DHCPv6 reply writes an address, its lifetimes, a delegated prefix
                            // and the client's resolvers into the host's network stack, so nothing
                            // positive is ever built here. Two Status Codes look like "no" and are
                            // statements about a *lease* (RFC 8415 §21.13): NoAddrsAvail says this
                            // link has no addresses, NoBinding that the client's lease does not
                            // exist. Neither is sent on a failure — both would be false.
                            //
                            // UnspecFail is different: "failure, reason unspecified", and RFC 8415
                            // §18.2.10 defines the client's handling of a REPLY carrying it — the
                            // server "was unable to process the client's message", and a client
                            // that retries MUST rate-limit. It is the protocol's own 500. So every
                            // message answered by a REPLY (REQUEST, RENEW, REBIND, RELEASE,
                            // INFORMATION-REQUEST) gets one on a fail-closed outcome.
                            //
                            // SOLICIT is answered by an ADVERTISE, which has no failure form that
                            // is not a lease statement, and RFC 8415 §18.3.1 lets a server discard
                            // a SOLICIT; silence there sends the client to another server or to
                            // its own retransmission timer.
                            //
                            // Every outcome is logged with a stable `decision=` token (the
                            // src/server/radius convention): grep `decision=fail_closed_` for every
                            // message NetGet could not decide, whichever of the two it received.
                            let mut failure = crate::utils::WireFailure::Unavailable;
                            let decision = match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None,
                                &event,
                                &protocol,
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    for message in &execution_result.messages {
                                        log.info(message.to_string());
                                    }

                                    let had_actions = !execution_result.raw_actions.is_empty();
                                    let mut sent = 0usize;

                                    for protocol_result in execution_result.protocol_results {
                                        if let Some(output) =
                                            protocol_result.get_all_output().first()
                                        {
                                            let _ = socket_clone.send_to(output, peer_addr).await;
                                            sent += 1;

                                            log.debug(format!(
                                                "DHCPv6 sent {} bytes to {}",
                                                output.len(),
                                                peer_addr
                                            ));
                                            log.trace(format!(
                                                "DHCPv6 sent (hex): {}",
                                                hex::encode(output)
                                            ));
                                            log.info(format!(
                                                "DHCPv6 response to {} ({} bytes)",
                                                peer_addr,
                                                output.len()
                                            ));
                                        } else {
                                            log.debug("DHCPv6 protocol result has no output data");
                                        }
                                    }

                                    if sent > 0 {
                                        "model_reply"
                                    } else if had_actions {
                                        // The model answered and chose to send nothing
                                        // (`no_response`). Deliberate silence, not a failure.
                                        "model_reject"
                                    } else {
                                        // No action at all: nothing on the wire, same as an
                                        // error, but a different cause.
                                        "fail_closed_no_action"
                                    }
                                }
                                Err(e) => {
                                    // DHCPv6 cannot express "retry later" on the wire, so an
                                    // overloaded backend and a dead one look identical to the
                                    // client. Keep the distinction where it is actionable —
                                    // next to the decision token, in the log.
                                    let category =
                                        match crate::utils::wire_failure::WireFailure::classify(&e)
                                        {
                                            crate::utils::wire_failure::WireFailure::Overloaded => {
                                                "overloaded"
                                            }
                                            crate::utils::wire_failure::WireFailure::Unavailable => {
                                                "unavailable"
                                            }
                                        };
                                    log.error(format!(
                                        "DHCPv6 LLM call failed for {} (category={}): {}",
                                        peer_addr, category, e
                                    ));
                                    failure = crate::utils::WireFailure::classify(&e);
                                    "fail_closed_llm_error"
                                }
                            };

                            // A message whose answer is a REPLY gets the protocol's own "the
                            // server failed" — see `Dhcpv6Protocol::server_failure_reply`.
                            // SOLICIT stays silent.
                            let mut unspec_fail_sent = false;
                            if decision.starts_with("fail_closed_") {
                                match protocol.server_failure_reply(failure) {
                                    Ok(Some(reply)) => {
                                        if socket_clone.send_to(&reply, peer_addr).await.is_ok() {
                                            unspec_fail_sent = true;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => log.error(format!(
                                        "DHCPv6 could not build the UnspecFail reply for {}: {}",
                                        peer_addr, e
                                    )),
                                }
                            }

                            log.info(format!(
                                "DHCPv6 {} from {} decision={}",
                                message_type, peer_addr, decision
                            ));

                            if unspec_fail_sent {
                                log.warn(format!(
                                    "DHCPv6 answered {} with a REPLY carrying Status Code \
                                     UnspecFail (no usable decision was produced; RFC 8415 has \
                                     the client rate-limit any retry)",
                                    peer_addr
                                ));
                            } else if decision.starts_with("fail_closed_") {
                                log.warn(format!(
                                    "DHCPv6 sent no reply to {} (no usable decision was \
                                     produced; the client will retransmit)",
                                    peer_addr
                                ));
                            }
                        }).await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("DHCPv6 receive error: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the recv loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Map a client message type onto the event it raises.
    ///
    /// `None` means this server has nothing to say about the message and drops it. CONFIRM and
    /// DECLINE land here deliberately: both are questions about a binding, and NetGet keeps no
    /// bindings, so there is no honest answer to give. RFC 8415 §18.3.3 requires exactly this of
    /// a server that cannot tell whether the client's address is appropriate — "the server MUST
    /// NOT send a Reply". RECONFIGURE and ADVERTISE/REPLY are server-to-client messages, and
    /// RELAY-FORW is a relay envelope this server does not unwrap.
    fn event_for(msg_type: v6::MessageType) -> Option<&'static EventType> {
        match msg_type {
            v6::MessageType::Solicit => Some(&DHCPV6_SOLICIT_EVENT),
            v6::MessageType::Request => Some(&DHCPV6_REQUEST_EVENT),
            v6::MessageType::Renew => Some(&DHCPV6_RENEW_EVENT),
            v6::MessageType::Rebind => Some(&DHCPV6_REBIND_EVENT),
            v6::MessageType::Release => Some(&DHCPV6_RELEASE_EVENT),
            v6::MessageType::InformationRequest => Some(&DHCPV6_INFORMATION_REQUEST_EVENT),
            _ => None,
        }
    }

    /// Decode a datagram into the reply context and the event payload.
    ///
    /// `None` means it is not a usable DHCPv6 client message.
    fn parse_message(data: &[u8], peer_addr: SocketAddr) -> Option<ParsedMessage> {
        let msg = match v6::Message::decode(&mut Decoder::new(data)) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("Failed to parse DHCPv6 message: {}", e);
                return None;
            }
        };

        let opts = msg.opts();

        let client_duid = match opts.get(v6::OptionCode::ClientId) {
            Some(v6::DhcpOption::ClientId(duid)) => Some(duid.clone()),
            _ => None,
        };

        // The server the client addressed (REQUEST, RENEW, RELEASE carry it; REBIND and
        // SOLICIT do not). Kept only so a server-built failure reply can name the same server.
        let addressed_server_duid = match opts.get(v6::OptionCode::ServerId) {
            Some(v6::DhcpOption::ServerId(duid)) => Some(duid.clone()),
            _ => None,
        };

        // The Option Request Option is a list of option codes. Publish the names: a code number
        // is something the model has to look up, a name is something it can act on.
        let requested_options: Vec<String> = match opts.get(v6::OptionCode::ORO) {
            Some(v6::DhcpOption::ORO(oro)) => {
                oro.opts.iter().map(|code| format!("{:?}", code)).collect()
            }
            _ => Vec::new(),
        };

        let rapid_commit = opts.get(v6::OptionCode::RapidCommit).is_some();

        let (ia_na_id, client_addresses) = match opts.get(v6::OptionCode::IANA) {
            Some(v6::DhcpOption::IANA(iana)) => {
                let addrs: Vec<serde_json::Value> = iana
                    .opts
                    .iter()
                    .filter_map(|opt| match opt {
                        v6::DhcpOption::IAAddr(ia) => Some(serde_json::json!({
                            "address": ia.addr.to_string(),
                            "preferred_lifetime": ia.preferred_life,
                            "valid_lifetime": ia.valid_life,
                        })),
                        _ => None,
                    })
                    .collect();
                (Some(iana.id), addrs)
            }
            _ => (None, Vec::new()),
        };

        let (ia_pd_id, client_prefixes) = match opts.get(v6::OptionCode::IAPD) {
            Some(v6::DhcpOption::IAPD(iapd)) => {
                let prefixes: Vec<serde_json::Value> = iapd
                    .opts
                    .iter()
                    .filter_map(|opt| match opt {
                        v6::DhcpOption::IAPrefix(p) => Some(serde_json::json!({
                            "prefix": p.prefix_ip.to_string(),
                            "prefix_length": p.prefix_len,
                            "preferred_lifetime": p.preferred_lifetime,
                            "valid_lifetime": p.valid_lifetime,
                        })),
                        _ => None,
                    })
                    .collect();
                (Some(iapd.id), prefixes)
            }
            _ => (None, Vec::new()),
        };

        let mut event_data = serde_json::json!({
            "transaction_id": msg.xid_num(),
            "requested_options": requested_options,
            "source_address": peer_addr.ip().to_string(),
            "source_port": peer_addr.port(),
        });

        if let Some(duid) = client_duid.as_deref() {
            event_data["client_duid"] = describe_duid(duid);
        }
        if let Some(id) = ia_na_id {
            event_data["ia_id"] = serde_json::json!(id);
        }
        if let Some(id) = ia_pd_id {
            event_data["ia_pd_id"] = serde_json::json!(id);
        }
        if !client_addresses.is_empty() {
            event_data["client_addresses"] = serde_json::json!(client_addresses);
        }
        if !client_prefixes.is_empty() {
            event_data["client_prefixes"] = serde_json::json!(client_prefixes);
        }
        if msg.msg_type() == v6::MessageType::Solicit {
            event_data["rapid_commit"] = serde_json::json!(rapid_commit);
        }

        Some(ParsedMessage {
            context: Dhcpv6RequestContext {
                xid: msg.xid(),
                msg_type: msg.msg_type(),
                client_duid,
                addressed_server_duid,
                ia_na_id,
                ia_pd_id,
                rapid_commit,
            },
            event_data,
        })
    }
}
