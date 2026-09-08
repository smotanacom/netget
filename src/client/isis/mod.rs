//! IS-IS (Intermediate System to Intermediate System) client implementation
//!
//! This module provides functionality to capture and analyze IS-IS PDUs at Layer 2.
//! It uses libpcap to capture raw Ethernet frames containing IS-IS traffic.

pub mod actions;

pub use actions::IsisClientProtocol;

use anyhow::{Context, Result};
use pcap::{Capture, Device};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

use crate::client::isis::actions::ISIS_PDU_RECEIVED_EVENT;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// IS-IS PDU types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IsisPduType {
    L1LanHello = 15,
    L2LanHello = 16,
    P2PHello = 17,
    L1Lsp = 18,
    L2Lsp = 20,
    L1Csnp = 24,
    L2Csnp = 25,
    L1Psnp = 26,
    L2Psnp = 27,
    Unknown = 0,
}

impl From<u8> for IsisPduType {
    fn from(value: u8) -> Self {
        match value {
            15 => IsisPduType::L1LanHello,
            16 => IsisPduType::L2LanHello,
            17 => IsisPduType::P2PHello,
            18 => IsisPduType::L1Lsp,
            20 => IsisPduType::L2Lsp,
            24 => IsisPduType::L1Csnp,
            25 => IsisPduType::L2Csnp,
            26 => IsisPduType::L1Psnp,
            27 => IsisPduType::L2Psnp,
            _ => IsisPduType::Unknown,
        }
    }
}

impl IsisPduType {
    pub fn as_str(&self) -> &'static str {
        match self {
            IsisPduType::L1LanHello => "L1 LAN Hello",
            IsisPduType::L2LanHello => "L2 LAN Hello",
            IsisPduType::P2PHello => "P2P Hello",
            IsisPduType::L1Lsp => "L1 LSP",
            IsisPduType::L2Lsp => "L2 LSP",
            IsisPduType::L1Csnp => "L1 CSNP",
            IsisPduType::L2Csnp => "L2 CSNP",
            IsisPduType::L1Psnp => "L1 PSNP",
            IsisPduType::L2Psnp => "L2 PSNP",
            IsisPduType::Unknown => "Unknown",
        }
    }
}

/// Basic IS-IS PDU header structure
#[derive(Debug)]
pub struct IsisPduHeader {
    pub pdu_type: IsisPduType,
    pub version: u8,
    pub id_length: u8,
    pub pdu_length: u16,
}

/// Parse basic IS-IS PDU header from raw bytes
fn parse_isis_header(data: &[u8]) -> Option<IsisPduHeader> {
    if data.len() < 8 {
        return None;
    }

    // IS-IS header:
    // - Byte 0: Intradomain Routing Protocol Discriminator (0x83)
    // - Byte 1: Length Indicator
    // - Byte 2: Version/Protocol ID Extension
    // - Byte 3: ID Length
    // - Byte 4: PDU Type
    // - Byte 5: Version
    // - Byte 6: Reserved
    // - Byte 7: Maximum Area Addresses

    if data[0] != 0x83 {
        return None; // Not IS-IS
    }

    let pdu_type = IsisPduType::from(data[4]);
    let version = data[5];
    let id_length = data[3];

    // Calculate PDU length from length indicator
    let length_indicator = data[1] as u16;
    let pdu_length = length_indicator + 1; // Length includes the header

    Some(IsisPduHeader {
        pdu_type,
        version,
        id_length,
        pdu_length,
    })
}

/// IS-IS client that captures IS-IS PDUs from network interface
pub struct IsisClient;

impl IsisClient {
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

    /// Connect to network interface and capture IS-IS PDUs with LLM integration
    pub async fn connect_with_llm_actions(
        interface_name: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        info!(
            "ISIS client {} starting capture on interface: {}",
            client_id, interface_name
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] ISIS client {} capturing on {}",
            client_id, interface_name
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        let protocol = Arc::new(crate::client::isis::actions::IsisClientProtocol::new());

        // Command channel for injected actions (the dashboard's [ send ]).
        // This client makes no connected-event LLM call, so there is nothing to race here -
        // but it is still registered before the pcap task starts, so [ send ] is live from
        // the moment the client exists.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            protocol.clone(),
            client_id,
            app_state.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // pcap is blocking, so we run it in a blocking task.
        //
        // `JoinHandle::abort()` cannot interrupt a thread parked in `next_packet()`, so the
        // capture loop is stopped cooperatively: it polls this flag every iteration, and the
        // task registered with `register_client_task` below trips it when `remove_client`
        // aborts it. The capture opens pcap with `.timeout(1000)`, so a stop is observed
        // within ~1s. See `crate::utils::shutdown`, and the identical arrangement in
        // `crate::server::isis`.
        let stop = crate::utils::StopSignal::new();
        let stop_in_loop = stop.clone();
        // Retained by this function; the capture task takes ownership of `app_state`.
        let app_state_reg = app_state.clone();

        let interface_clone = interface_name.clone();
        let handle_state = app_state.clone();
        let handle_status_tx = status_tx.clone();
        tokio::task::spawn_blocking(move || {
            // Find device
            let device = match Self::find_device(&interface_clone) {
                Ok(d) => d,
                Err(e) => {
                    error!("ISIS client {} failed to find device: {}", client_id, e);
                    let runtime = tokio::runtime::Handle::current();
                    let _ = runtime.block_on(
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string())),
                    );
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                    return;
                }
            };

            // Open capture in promiscuous mode
            let mut cap = match Capture::from_device(device)
                .map(|c| c.promisc(true).snaplen(65535).timeout(1000))
                .and_then(|c| c.open())
            {
                Ok(c) => c,
                Err(e) => {
                    error!("ISIS client {} failed to open capture: {}", client_id, e);
                    let runtime = tokio::runtime::Handle::current();
                    let _ = runtime.block_on(
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string())),
                    );
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                    return;
                }
            };

            // Apply filter for IS-IS traffic
            // IS-IS uses LLC/SNAP with DSAP=0xFE, SSAP=0xFE
            if let Err(e) = cap.filter("ether proto 0xfefe or ether[14:2] = 0xfefe", true) {
                error!("ISIS client {} failed to apply filter: {}", client_id, e);
            }

            info!("ISIS client {} successfully opened capture", client_id);

            let mut memory = String::new();

            // Capture loop
            loop {
                // Checked before the blocking read as well as after it, so a stop arriving
                // while the loop is between packets is not missed.
                if stop_in_loop.is_stopped() {
                    info!(
                        "ISIS client {} stopping capture (client removed)",
                        client_id
                    );
                    break;
                }

                match cap.next_packet() {
                    Ok(packet) => {
                        let data = packet.data;
                        trace!("ISIS client {} captured {} bytes", client_id, data.len());

                        // Skip Ethernet header (14 bytes) and LLC/SNAP header (8 bytes)
                        // Ethernet: dest MAC (6) + src MAC (6) + ethertype (2) = 14 bytes
                        // LLC/SNAP: DSAP (1) + SSAP (1) + Control (1) + OUI (3) + PID (2) = 8 bytes
                        let isis_offset = 14 + 8;

                        if data.len() > isis_offset {
                            let isis_data = &data[isis_offset..];

                            // Parse IS-IS header
                            if let Some(header) = parse_isis_header(isis_data) {
                                debug!(
                                    "ISIS client {} captured PDU: type={}, version={}, length={}",
                                    client_id,
                                    header.pdu_type.as_str(),
                                    header.version,
                                    header.pdu_length
                                );

                                // Get instruction
                                let runtime = tokio::runtime::Handle::current();
                                let instruction = match runtime
                                    .block_on(app_state.get_instruction_for_client(client_id))
                                {
                                    Some(i) => i,
                                    None => continue,
                                };

                                // Call LLM with captured PDU
                                let event = Event::new(
                                    &ISIS_PDU_RECEIVED_EVENT,
                                    serde_json::json!({
                                        "pdu_type": header.pdu_type.as_str(),
                                        "pdu_type_code": header.pdu_type as u8,
                                        "version": header.version,
                                        "pdu_length": header.pdu_length,
                                        "raw_pdu_hex": hex::encode(isis_data),
                                        "raw_frame_hex": hex::encode(data),
                                    }),
                                );

                                // Call LLM (blocking version)
                                let runtime = tokio::runtime::Handle::current();
                                match runtime.block_on(call_llm_for_client(
                                    &llm_client,
                                    &app_state,
                                    client_id.to_string(),
                                    &instruction,
                                    &memory,
                                    Some(&event),
                                    protocol.as_ref(),
                                    &status_tx,
                                )) {
                                    Ok(ClientLlmResult {
                                        actions,
                                        memory_updates,
                                    }) => {
                                        // Update memory
                                        if let Some(mem) = memory_updates {
                                            memory = mem;
                                        }

                                        // IS-IS is a receive-only sniffer, so most of its
                                        // vocabulary really is interpretation: analyze_topology
                                        // and wait_for_more put nothing on the wire, and there
                                        // is no transmit handle to put it on.
                                        //
                                        // `stop_capture` is the exception and was being thrown
                                        // away with the rest, so a model that decided it had
                                        // seen enough could not stop the capture -- the client
                                        // ran until someone removed it. The capture loop polls
                                        // client status, so marking it Disconnected ends it.
                                        use crate::llm::actions::client_trait::{
                                            Client, ClientActionResult,
                                        };
                                        for action in &actions {
                                            let stop =
                                                matches!(
                                                    protocol.execute_action(action.clone()),
                                                    Ok(ClientActionResult::Disconnect)
                                                ) || action.get("type").and_then(|v| v.as_str())
                                                    == Some("stop_capture");
                                            if stop {
                                                info!(
                                                    "IS-IS client {} stopping capture on the \
                                                     model's request",
                                                    client_id
                                                );
                                                // This whole closure runs on the blocking
                                                // pool (pcap is a blocking API), so the
                                                // status update goes through the same
                                                // `block_on` bridge the LLM call above uses.
                                                runtime.block_on(app_state.update_client_status(
                                                    client_id,
                                                    ClientStatus::Disconnected,
                                                ));
                                                let _ = status_tx.send("__UPDATE_UI__".to_string());
                                                break;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("ISIS client {} LLM error: {}", client_id, e);
                                    }
                                }
                            } else {
                                trace!("ISIS client {} skipped non-ISIS packet", client_id);
                            }
                        }
                    }
                    Err(pcap::Error::TimeoutExpired) => {
                        // Timeout is expected, continue
                        continue;
                    }
                    Err(e) => {
                        error!("ISIS client {} capture error: {}", client_id, e);
                        let runtime = tokio::runtime::Handle::current();
                        let _ =
                            runtime.block_on(app_state.update_client_status(
                                client_id,
                                ClientStatus::Error(e.to_string()),
                            ));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                }

                // Check if client is still active.
                //
                // The `None` arm is the one that matters: `remove_client` *deletes* the
                // entry rather than marking it `Disconnected`, so a version of this that
                // only inspected `Some(client)` kept capturing — and kept calling the LLM —
                // forever after the client was removed. The `StopSignal` above covers the
                // same case; this is the cheap second check on the path that already reads
                // the client.
                let runtime = tokio::runtime::Handle::current();
                match runtime.block_on(app_state.get_client(client_id)) {
                    Some(client)
                        if matches!(
                            client.status,
                            ClientStatus::Disconnected | ClientStatus::Error(_)
                        ) =>
                    {
                        info!("ISIS client {} stopping capture", client_id);
                        break;
                    }
                    Some(_) => {}
                    None => {
                        info!("ISIS client {} stopping capture (client gone)", client_id);
                        break;
                    }
                }
            }

            // The capture is gone: drop the command handle so the dashboard stops offering
            // [ send ] on a dead client. This also closes the command channel, which ends
            // `command_loop`.
            let runtime = tokio::runtime::Handle::current();
            runtime.block_on(handle_state.remove_client_handle(client_id));
            let _ = handle_status_tx.send("__UPDATE_UI__".to_string());
        });

        // The blocking capture task's own `JoinHandle` is deliberately not registered:
        // aborting it would do nothing, because Tokio cannot unwind a thread parked in
        // `next_packet()`. Registering the parked task instead makes `remove_client` trip
        // the flag the loop polls, which is what actually stops it.
        app_state_reg
            .register_client_task(client_id, stop.park_task())
            .await;

        // Return a dummy SocketAddr (pcap doesn't have socket addresses)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Drain injected commands until the channel closes (client removed, or the capture
    /// exited and dropped the handle) or an injected `stop_capture` ends the session.
    ///
    /// **This client can never report `Sent`, and that is not a gap in the wiring.** The
    /// IS-IS client is a receive-only libpcap sniffer: it has no transmit handle, and its
    /// whole vocabulary is `analyze_topology` / `wait_for_more` (both `WaitForMore` - the
    /// analysis happens in the LLM's memory) and `stop_capture` (`Disconnect`). An injected
    /// action is executed through the protocol's own `execute_action`, exactly as an
    /// LLM-produced one is, and the honest outcome for everything except `stop_capture` is
    /// `Executed` with the reason. `stop_capture` is real work: the capture loop polls the
    /// client's status every iteration, so marking the client disconnected here stops it
    /// within one pcap timeout (1s).
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        protocol: Arc<crate::client::isis::actions::IsisClientProtocol>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};
        use crate::llm::actions::protocol_trait::Protocol;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.as_ref().execute_action(action.clone()) {
                Err(e) => ClientSendOutcome::Rejected {
                    error: e.to_string(),
                },
                Ok(ClientActionResult::Disconnect) => ClientSendOutcome::Disconnected,
                Ok(ClientActionResult::WaitForMore) => ClientSendOutcome::Executed {
                    detail: "executed: the IS-IS client is a receive-only capture, so no PDU \
                             is transmitted; the analysis lives in the LLM's memory"
                        .to_string(),
                },
                Ok(other) => ClientSendOutcome::Executed {
                    detail: format!(
                        "{other:?}: the IS-IS client is a receive-only capture and transmits \
                         no PDUs"
                    ),
                },
            };

            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null)],
                )
                .await;

            let disconnect = matches!(outcome, ClientSendOutcome::Disconnected);
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, Ok(outcome));

            if disconnect {
                info!(
                    "ISIS client {} stopping capture on injected action",
                    client_id
                );
                app_state.remove_client_handle(client_id).await;
                // The capture loop checks this on every iteration and breaks out.
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                break;
            }
        }
    }
}
