//! ARP (Address Resolution Protocol) server implementation
//!
//! This module provides functionality to capture and respond to ARP requests at the data link layer.
//! It uses libpcap via pnet to interact with network interfaces and handle ARP packets.

pub mod actions;

use anyhow::{Context, Result};
use pcap::{Capture, Device};
use pnet::packet::arp::{ArpHardwareTypes, ArpOperations, ArpPacket, MutableArpPacket};
use pnet::packet::ethernet::{EtherTypes, EthernetPacket, MutableEthernetPacket};
use pnet::packet::Packet;
use pnet::util::MacAddr;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::{console_error, console_info, console_trace};
use actions::{ArpProtocol, ARP_REQUEST_RECEIVED_EVENT};

/// Get LLM context and output format instructions for ARP stack
pub fn get_llm_protocol_prompt() -> (&'static str, &'static str) {
    let context = r#"You are handling ARP (Address Resolution Protocol) requests at Layer 2.
ARP is used to map IP addresses to MAC addresses on local networks.
You can respond to ARP requests (who has IP X.X.X.X?) with ARP replies containing MAC addresses.
Common use cases: network reconnaissance detection, ARP spoofing simulation, custom IP-to-MAC mappings."#;

    let output_format = r#"IMPORTANT: Respond with a JSON object:
{
  "output": "Ethernet frame containing ARP reply as hex (null if no response)",
  "message": null  // Optional message for user
}"#;

    (context, output_format)
}

/// What happened to one captured ARP packet, as a stable log token.
///
/// ARP is in the deliberately-silent class: there is no error frame, and the only reply the
/// protocol defines is the positive assertion "this MAC owns that IP", so a fabricated reply
/// would poison the requester's neighbour cache. Every failure mode therefore looks identical
/// on the wire — nothing is sent — and the **log** is the only place the distinction survives.
/// The tokens mirror `src/server/radius/`'s so `decision=fail_closed_` finds every packet the
/// model did not actually answer, across protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// A reply was built from the model's actions and handed to the injection thread.
    ModelReply,
    /// The model ran an action that deliberately produces no frame (`ignore_arp`).
    ModelReject,
    /// No operator policy at all (no instruction, no handler): the static default, and the
    /// only one of these that is not a failure. No LLM round-trip was taken.
    StaticNoPolicy,
    /// The model replied, but with no actions. Not the same as an explicit `ignore_arp`.
    FailClosedNoAction,
    /// Actions were produced but every one of them failed to execute.
    FailClosedActionError,
    /// The LLM call itself failed (backend down, rate-limited, unparseable after retries).
    FailClosedLlmError,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::ModelReply => "model_reply",
            Decision::ModelReject => "model_reject",
            Decision::StaticNoPolicy => "static_no_policy",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedActionError => "fail_closed_action_error",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
        }
    }

    pub fn is_fail_closed(&self) -> bool {
        matches!(
            self,
            Decision::FailClosedNoAction
                | Decision::FailClosedActionError
                | Decision::FailClosedLlmError
        )
    }
}

/// Log one packet's outcome to both sinks, loudly when it was a fail-closed drop.
fn log_decision(
    status_tx: &mpsc::UnboundedSender<String>,
    decision: Decision,
    operation: &str,
    sender_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
) {
    let summary = format!(
        "ARP {} from {} for {} decision={}",
        operation,
        sender_ip,
        target_ip,
        decision.as_str()
    );
    if decision.is_fail_closed() {
        error!("{} (nothing sent: no usable answer was produced)", summary);
        let _ = status_tx.send(format!(
            "✗ {} (nothing sent: no usable answer was produced)",
            summary
        ));
    } else {
        info!("{}", summary);
        let _ = status_tx.send(format!("→ {}", summary));
    }
}

/// ARP server that captures and responds to ARP requests
pub struct ArpServer;

impl ArpServer {
    /// List available network interfaces
    pub fn list_devices() -> Result<Vec<Device>> {
        Device::list().context("Failed to list network devices")
    }

    /// Find a device by name
    pub fn find_device(name: &str) -> Result<Device> {
        let devices = Self::list_devices()?;
        devices
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| anyhow::anyhow!("Device '{}' not found", name))
    }

    /// Spawn ARP server with integrated LLM handling (async wrapper for blocking pcap)
    pub async fn spawn_with_llm(
        interface: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<String> {
        console_info!(
            status_tx,
            "Starting ARP capture on interface: {}",
            interface
        );

        // Retained by this function; the capture task takes ownership of `status_tx`.
        let status_tx_ready = status_tx.clone();

        let protocol = Arc::new(ArpProtocol::new());

        // ARP/pcap is blocking, so we run it in a blocking task.
        //
        // Opening the pcap handle is the step that requires privileges (root, or read/write
        // access to /dev/bpf* on macOS and the BSDs, or CAP_NET_RAW on Linux). It therefore
        // MUST NOT be fire-and-forget: we hand the outcome back over a oneshot and only return
        // Ok once the capture is genuinely live, so a failure surfaces as ServerStatus::Error
        // instead of a server that reports Running while capturing nothing.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

        // `JoinHandle::abort()` cannot interrupt a thread parked in `next_packet()`, so the
        // capture loop is stopped cooperatively: it polls this flag every iteration, and the
        // task registered with `register_server_task` below trips it when `stop_server` aborts
        // it. Without this the capture kept running (and kept calling the LLM) after the server
        // was stopped, until the process exited. See `crate::utils::shutdown`.
        let stop = crate::utils::StopSignal::new();
        let stop_in_loop = stop.clone();
        // Retained by this function; the capture task takes ownership of `app_state`.
        let app_state_reg = app_state.clone();

        let interface_clone = interface.clone();
        let protocol_clone = protocol.clone();
        tokio::task::spawn_blocking(move || {
            let open_captures = || -> Result<(Capture<pcap::Active>, Capture<pcap::Active>)> {
                let device = Self::find_device(&interface_clone)
                    .with_context(|| format!("no such capture device '{}'", interface_clone))?;

                // Open capture for receiving
                let mut cap_rx = Capture::from_device(device.clone())
                    .map(|c| c.promisc(true).snaplen(65535).timeout(1000))
                    .and_then(|c| c.open())
                    .with_context(|| {
                        format!(
                            "failed to open pcap capture on '{}' (needs root, or \
                             read access to /dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux)",
                            interface_clone
                        )
                    })?;

                // Apply ARP filter to receiving capture
                cap_rx
                    .filter("arp", true)
                    .context("failed to apply the 'arp' BPF filter")?;

                // Open capture for sending (separate instance)
                let cap_tx = Capture::from_device(device)
                    .map(|c| c.promisc(true).snaplen(65535).timeout(1000))
                    .and_then(|c| c.open())
                    .with_context(|| {
                        format!(
                            "failed to open pcap injection handle on '{}'",
                            interface_clone
                        )
                    })?;

                Ok((cap_rx, cap_tx))
            };

            let (mut cap_rx, mut cap_tx) = match open_captures() {
                Ok(handles) => {
                    let _ = ready_tx.send(Ok(()));
                    handles
                }
                Err(e) => {
                    console_error!(status_tx, "ARP capture startup failed: {:#}", e);
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            let runtime = tokio::runtime::Handle::current();

            // Channel for sending packets from async tasks back to blocking thread
            let (packet_tx, packet_rx) = std::sync::mpsc::channel::<Vec<u8>>();

            // Spawn a task to handle packet injection
            std::thread::spawn(move || {
                while let Ok(packet) = packet_rx.recv() {
                    // This will block, but that's OK - we're in a dedicated thread
                    if let Err(e) = cap_tx.sendpacket(packet) {
                        error!("Failed to send ARP packet: {}", e);
                    }
                }
            });

            // Capture loop. The pcap read timeout (1000ms, set above) is what bounds how long
            // a stop takes to be noticed on an idle interface.
            loop {
                if stop_in_loop.is_stopped() {
                    console_info!(status_tx, "ARP capture on {} stopping", interface_clone);
                    break;
                }
                match cap_rx.next_packet() {
                    Ok(packet) => {
                        let data = packet.data.to_vec();

                        // Parse Ethernet frame
                        let eth_packet = match EthernetPacket::new(&data) {
                            Some(p) => p,
                            None => {
                                debug!("Failed to parse Ethernet packet");
                                continue;
                            }
                        };

                        // Check if it's an ARP packet
                        if eth_packet.get_ethertype() != EtherTypes::Arp {
                            continue;
                        }

                        // Parse ARP packet
                        let arp_packet = match ArpPacket::new(eth_packet.payload()) {
                            Some(p) => p,
                            None => {
                                debug!("Failed to parse ARP packet");
                                continue;
                            }
                        };

                        // Extract ARP information
                        let operation = arp_packet.get_operation();
                        let sender_mac = arp_packet.get_sender_hw_addr();
                        let sender_ip = arp_packet.get_sender_proto_addr();
                        let target_mac = arp_packet.get_target_hw_addr();
                        let target_ip = arp_packet.get_target_proto_addr();

                        // DEBUG: Log summary
                        debug!(
                            "ARP {} from {} ({}) for {} ({})",
                            operation_to_string(operation),
                            sender_mac,
                            sender_ip,
                            target_mac,
                            target_ip
                        );
                        let _ = status_tx.send(format!(
                            "[DEBUG] ARP {} from {} ({}) for {} ({})",
                            operation_to_string(operation),
                            sender_mac,
                            sender_ip,
                            target_mac,
                            target_ip
                        ));

                        // TRACE: Log full packet
                        let hex_str = hex::encode(&data);
                        console_trace!(status_tx, "ARP packet (hex): {}", hex_str);

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_task_clone = protocol_clone.clone();
                        let packet_tx_clone = packet_tx.clone();

                        // Spawn async task to handle packet with LLM
                        runtime.spawn(async move {
                            // Build event data
                            let event = Event::new(
                                &ARP_REQUEST_RECEIVED_EVENT,
                                serde_json::json!({
                                    "operation": operation_to_string(operation),
                                    "sender_mac": sender_mac.to_string(),
                                    "sender_ip": sender_ip.to_string(),
                                    "target_mac": target_mac.to_string(),
                                    "target_ip": target_ip.to_string(),
                                    "packet_hex": hex::encode(&data)
                                }),
                            );

                            // An ARP reply is NOT wire-determined: the MAC advertised as owning
                            // the queried IP is a chosen answer (the whole point is deciding
                            // which MAC to claim — spoofing, honeypot, custom mapping), exactly
                            // the kind of policy DNS/DHCP leave to the model. There is no
                            // mechanical reply to synthesise from the request. So with no operator
                            // policy — no server instruction and no per-event handler — the
                            // spec-safe default is to answer nothing (we have no MAC to claim),
                            // WITHOUT burning an LLM round-trip per captured packet. The model is
                            // consulted only when the operator opts in with the mapping to serve.
                            if !operator_wants_dynamic(
                                &state_clone,
                                server_id,
                                &event.event_type.id,
                            )
                            .await
                            {
                                debug!(
                                    "ARP {} from {} for {} decision={}: no operator policy configured (no instruction or handler), no MAC to advertise and no LLM call",
                                    operation_to_string(operation),
                                    sender_ip,
                                    target_ip,
                                    Decision::StaticNoPolicy.as_str()
                                );
                                let _ = status_clone.send(format!(
                                    "ARP {} from {} for {} decision={}",
                                    operation_to_string(operation),
                                    sender_ip,
                                    target_ip,
                                    Decision::StaticNoPolicy.as_str()
                                ));
                                return;
                            }

                            debug!(
                                "ARP calling LLM for {} packet",
                                operation_to_string(operation)
                            );
                            let _ = status_clone.send(format!(
                                "[DEBUG] ARP calling LLM for {} packet",
                                operation_to_string(operation)
                            ));

                            match call_llm(
                                &llm_clone,
                                &state_clone,
                                server_id,
                                None,
                                &event,
                                protocol_task_clone.as_ref(),
                            )
                            .await
                            {
                                Ok(execution_result) => {
                                    for message in &execution_result.messages {
                                        info!("{}", message);
                                        let _ = status_clone.send(format!("[INFO] {}", message));
                                    }

                                    debug!(
                                        "ARP got {} protocol results",
                                        execution_result.protocol_results.len()
                                    );

                                    // Send ARP replies if any via channel
                                    let mut queued = 0usize;
                                    for protocol_result in &execution_result.protocol_results {
                                        if let Some(output_data) =
                                            protocol_result.get_all_output().first()
                                        {
                                            // Send packet via channel to injection thread
                                            if packet_tx_clone.send(output_data.clone()).is_ok() {
                                                queued += 1;
                                                debug!(
                                                    "ARP queued {} bytes for sending",
                                                    output_data.len()
                                                );
                                                trace!(
                                                    "ARP reply (hex): {}",
                                                    hex::encode(output_data)
                                                );
                                            } else {
                                                error!("Failed to queue ARP reply");
                                                let _ = status_clone.send(
                                                    "[ERROR] Failed to queue ARP reply".to_string(),
                                                );
                                            }
                                        }
                                    }

                                    let decision = if queued > 0 {
                                        Decision::ModelReply
                                    } else if execution_result.raw_actions.is_empty() {
                                        // The model answered, but with no action at all. On a
                                        // protocol whose only reply asserts "this MAC owns that
                                        // IP", silence is the only safe reading of silence.
                                        Decision::FailClosedNoAction
                                    } else if !execution_result.failures.is_empty() {
                                        Decision::FailClosedActionError
                                    } else {
                                        // Actions ran and deliberately produced no frame —
                                        // `ignore_arp`. Distinct from the model saying nothing.
                                        Decision::ModelReject
                                    };
                                    log_decision(
                                        &status_clone,
                                        decision,
                                        operation_to_string(operation),
                                        sender_ip,
                                        target_ip,
                                    );
                                }
                                Err(e) => {
                                    // Deliberate silence: a fabricated ARP reply would write a
                                    // MAC we did not choose into the requester's neighbour
                                    // cache. The error text stays in the log and the operator
                                    // status stream; nothing goes on the wire.
                                    error!(
                                        "ARP LLM call failed for {} from {} for {}: {}",
                                        operation_to_string(operation),
                                        sender_ip,
                                        target_ip,
                                        e
                                    );
                                    log_decision(
                                        &status_clone,
                                        Decision::FailClosedLlmError,
                                        operation_to_string(operation),
                                        sender_ip,
                                        target_ip,
                                    );
                                }
                            }
                        });
                    }
                    Err(pcap::Error::TimeoutExpired) => {
                        // Normal timeout, continue
                        continue;
                    }
                    Err(e) => {
                        console_error!(status_tx, "Packet capture error: {}", e);
                        break;
                    }
                }
            }

            // Dropping the last `packet_tx` makes the injection thread's `recv()` fail, so it
            // exits and closes the second pcap handle with it. Explicit because the thread is
            // detached and this channel is the only thing that can end it.
            drop(packet_tx);
            drop(cap_rx);
        });

        // Wait for the blocking task to report whether the capture actually came up.
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "ARP capture task on '{}' exited before signalling readiness",
                    interface
                ))
            }
        }

        // Only now that the capture is genuinely live: `stop_server` aborts this parked task,
        // which trips `stop` and ends the blocking loop above.
        app_state_reg
            .register_server_task(server_id, stop.park_task())
            .await;

        console_info!(status_tx_ready, "ARP capture active on {}", interface);

        Ok(interface)
    }

    /// Helper function to build an ARP reply packet
    pub fn build_arp_reply(
        sender_mac: MacAddr,
        sender_ip: Ipv4Addr,
        target_mac: MacAddr,
        target_ip: Ipv4Addr,
    ) -> Vec<u8> {
        // Ethernet header (14 bytes) + ARP packet (28 bytes) = 42 bytes
        let mut eth_buffer = vec![0u8; 42];

        // Build Ethernet frame
        {
            let mut eth_packet = MutableEthernetPacket::new(&mut eth_buffer).unwrap();
            eth_packet.set_destination(target_mac);
            eth_packet.set_source(sender_mac);
            eth_packet.set_ethertype(EtherTypes::Arp);

            // Build ARP packet
            let mut arp_buffer = vec![0u8; 28];
            {
                let mut arp_packet = MutableArpPacket::new(&mut arp_buffer).unwrap();
                arp_packet.set_hardware_type(ArpHardwareTypes::Ethernet);
                arp_packet.set_protocol_type(EtherTypes::Ipv4);
                arp_packet.set_hw_addr_len(6);
                arp_packet.set_proto_addr_len(4);
                arp_packet.set_operation(ArpOperations::Reply);
                arp_packet.set_sender_hw_addr(sender_mac);
                arp_packet.set_sender_proto_addr(sender_ip);
                arp_packet.set_target_hw_addr(target_mac);
                arp_packet.set_target_proto_addr(target_ip);
            }

            eth_packet.set_payload(&arp_buffer);
        }

        eth_buffer
    }
}

/// Convert ARP operation to human-readable string
fn operation_to_string(op: pnet::packet::arp::ArpOperation) -> &'static str {
    match op {
        ArpOperations::Request => "REQUEST",
        ArpOperations::Reply => "REPLY",
        _ => "UNKNOWN",
    }
}

/// Returns true if the operator opted into dynamic (LLM- or handler-driven) responses for this
/// server: either a non-empty server instruction was given, or an event handler is configured
/// for `event_id`. When false the protocol applies its static default and never consults the
/// model — for a policy protocol like ARP that default is to answer nothing, because with no
/// configured mapping there is no MAC it can legitimately claim.
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
