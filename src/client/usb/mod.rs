//! USB client implementation
pub mod actions;

pub use actions::UsbClientProtocol;

use anyhow::{anyhow, Context, Result};
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, RequestBuffer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::usb::actions::{
    USB_BULK_DATA_RECEIVED_EVENT, USB_CONTROL_RESPONSE_EVENT, USB_DEVICE_OPENED_EVENT,
    USB_INTERRUPT_DATA_RECEIVED_EVENT,
};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// Read a byte-wide field out of an already-validated action payload, refusing a wrapped value.
///
/// The payload is built by `UsbClientProtocol::execute_action`, so in the ordinary case every
/// field is present and in range. This exists because the read used to be `.unwrap() as u8`,
/// and this code runs inside `tokio::spawn`: a panic there is swallowed, the client task dies,
/// and the client goes on reporting `Connected` with nothing in the log to explain it.
fn byte_of(data: &serde_json::Value, field: &str) -> Option<u8> {
    data.get(field)
        .and_then(|v| v.as_u64())
        .and_then(|v| u8::try_from(v).ok())
}

/// The 16-bit counterpart of [`byte_of`].
fn word_of(data: &serde_json::Value, field: &str) -> Option<u16> {
    data.get(field)
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok())
}

/// Read a transfer length, re-applying the same bound `execute_action` enforces.
///
/// Checked twice on purpose: this value reaches `RequestBuffer::new`, which allocates it
/// outright, and the two readers are far enough apart that a future caller could reach this one
/// without passing the first.
fn length_of(data: &serde_json::Value) -> Option<usize> {
    let value = data.get("length").and_then(|v| v.as_u64())?;
    if value > crate::client::usb::actions::MAX_TRANSFER_BYTES as u64 {
        return None;
    }
    Some(value as usize)
}

/// Connection state for LLM processing
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum ConnectionState {
    Idle,
    Processing,
    Accumulating,
}

/// Per-client data for LLM handling
struct ClientData {
    #[allow(dead_code)]
    state: ConnectionState,
    #[allow(dead_code)]
    queued_events: Vec<Event>,
    memory: String,
}

/// What applying one action to the claimed USB interface actually did.
///
/// `Sent` is reported only for an OUT transfer whose completion status came back
/// `Ok` — those bytes really reached the device. Everything else (an IN transfer,
/// a no-op, a failed transfer) reports `Executed` with a specific reason, because
/// nothing left the host.
pub enum UsbApplied {
    /// This many payload bytes were delivered to the device.
    Sent(usize),
    /// The action ran but wrote nothing; the string says what it did.
    Executed(String),
    /// The device was detached.
    Disconnected,
}

/// USB device information parsed from remote_addr or instruction
#[derive(Debug, Clone)]
struct UsbDeviceInfo {
    vendor_id: u16,
    product_id: u16,
    interface_number: u8,
}

impl UsbDeviceInfo {
    /// Parse USB device info from string like "vid:1234,pid:5678" or "1234:5678"
    fn from_string(s: &str) -> Result<Self> {
        // Try formats:
        // - "vid:1234,pid:5678,interface:0"
        // - "1234:5678:0"
        // - "0x1234:0x5678:0"

        let parts: Vec<&str> = s.split(',').collect();

        let mut vendor_id: Option<u16> = None;
        let mut product_id: Option<u16> = None;
        let mut interface_number: u8 = 0; // Default to interface 0

        if parts.len() == 1 {
            // Colon-separated format: "1234:5678" or "1234:5678:0"
            let colon_parts: Vec<&str> = s.split(':').collect();
            if colon_parts.len() >= 2 {
                vendor_id = Some(Self::parse_hex_u16(colon_parts[0])?);
                product_id = Some(Self::parse_hex_u16(colon_parts[1])?);
                if colon_parts.len() >= 3 {
                    interface_number = colon_parts[2].parse()?;
                }
            }
        } else {
            // Comma-separated key:value format
            for part in parts {
                let kv: Vec<&str> = part.trim().split(':').collect();
                if kv.len() == 2 {
                    match kv[0].trim().to_lowercase().as_str() {
                        "vid" | "vendor" => vendor_id = Some(Self::parse_hex_u16(kv[1].trim())?),
                        "pid" | "product" => product_id = Some(Self::parse_hex_u16(kv[1].trim())?),
                        "interface" | "if" => interface_number = kv[1].trim().parse()?,
                        _ => {}
                    }
                }
            }
        }

        Ok(UsbDeviceInfo {
            vendor_id: vendor_id.ok_or_else(|| anyhow!("Missing vendor_id"))?,
            product_id: product_id.ok_or_else(|| anyhow!("Missing product_id"))?,
            interface_number,
        })
    }

    /// Parse hex string with optional 0x prefix
    fn parse_hex_u16(s: &str) -> Result<u16> {
        let s = s.trim();
        if s.starts_with("0x") || s.starts_with("0X") {
            u16::from_str_radix(&s[2..], 16).context("Invalid hex number")
        } else {
            // Try hex first, then decimal
            u16::from_str_radix(s, 16)
                .or_else(|_| s.parse::<u16>())
                .context("Invalid number")
        }
    }
}

/// USB client that connects to a USB device
pub struct UsbClient;

impl UsbClient {
    /// Connect to a USB device with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Parse USB device info
        let device_info = UsbDeviceInfo::from_string(&remote_addr)
            .context("Failed to parse USB device info from remote_addr")?;

        info!(
            "USB client {} connecting to device VID:{:04x} PID:{:04x}",
            client_id, device_info.vendor_id, device_info.product_id
        );

        // Find and open USB device
        let device_info_clone = device_info.clone();
        let device = tokio::task::spawn_blocking(move || {
            let devices = nusb::list_devices().context("Failed to list USB devices")?;

            for dev_info in devices {
                if dev_info.vendor_id() == device_info_clone.vendor_id
                    && dev_info.product_id() == device_info_clone.product_id
                {
                    return dev_info.open().context("Failed to open USB device");
                }
            }

            Err(anyhow!(
                "USB device VID:{:04x} PID:{:04x} not found",
                device_info_clone.vendor_id,
                device_info_clone.product_id
            ))
        })
        .await??;

        // Get manufacturer/product strings
        // Use language ID 0x0409 (English - United States)
        const LANG_EN_US: u16 = 0x0409;

        let (manufacturer, product) = tokio::task::spawn_blocking({
            let device = device.clone();
            move || -> Result<(Option<String>, Option<String>)> {
                // Try to get manufacturer string (index 1 is common)
                let manufacturer = device
                    .get_string_descriptor(1, LANG_EN_US, Duration::from_secs(1))
                    .ok();

                // Try to get product string (index 2 is common)
                let product = device
                    .get_string_descriptor(2, LANG_EN_US, Duration::from_secs(1))
                    .ok();

                Ok((manufacturer, product))
            }
        })
        .await??;

        info!(
            "USB client {} opened device: {} {}",
            client_id,
            manufacturer.as_deref().unwrap_or("Unknown"),
            product.as_deref().unwrap_or("Unknown")
        );

        // Claim interface
        let interface = tokio::task::spawn_blocking({
            let device = device.clone();
            let interface_num = device_info.interface_number;
            move || {
                device
                    .claim_interface(interface_num)
                    .context(format!("Failed to claim interface {}", interface_num))
            }
        })
        .await??;

        info!(
            "USB client {} claimed interface {}",
            client_id, device_info.interface_number
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] USB client {} connected to VID:{:04x} PID:{:04x}",
            client_id, device_info.vendor_id, device_info.product_id
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_events: Vec::new(),
            memory: String::new(),
        }));

        // Create a fake socket address (USB doesn't use sockets)
        // We'll use the vendor/product IDs encoded in the address
        let fake_addr: SocketAddr = format!(
            "127.{}.{}.{}:{}",
            (device_info.vendor_id >> 8) & 0xFF,
            device_info.vendor_id & 0xFF,
            (device_info.product_id >> 8) & 0xFF,
            device_info.product_id & 0xFF
        )
        .parse()
        .unwrap();

        // Send initial connected event
        let protocol = Arc::new(UsbClientProtocol::new());

        // Command channel for injected actions (the dashboard's [ send ]).
        // Registered BEFORE the connected-event LLM call below: a dashboard-created
        // client defaults to a `*` -> manual rule, so that call can park for minutes
        // waiting for a human, and the operator must be able to reach the device while
        // it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn({
            let protocol = protocol.clone();
            let interface = interface.clone();
            let app_state = app_state.clone();
            let llm_client = llm_client.clone();
            let status_tx = status_tx.clone();
            let client_data = client_data.clone();
            async move {
                Self::command_loop(
                    command_rx,
                    protocol,
                    interface,
                    client_id,
                    app_state,
                    llm_client,
                    status_tx,
                    client_data,
                )
                .await;
            }
        });
        app_state.register_client_task(client_id, cmd_task).await;
        let event = Event::new(
            &USB_DEVICE_OPENED_EVENT,
            serde_json::json!({
                "vendor_id": format!("{:04x}", device_info.vendor_id),
                "product_id": format!("{:04x}", device_info.product_id),
                "manufacturer": manufacturer.unwrap_or_else(|| "Unknown".to_string()),
                "product": product.unwrap_or_else(|| "Unknown".to_string()),
            }),
        );

        // Call LLM with connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &client_data.lock().await.memory,
                Some(&event),
                protocol.as_ref(),
                &status_tx,
            )
            .await
            {
                Ok(ClientLlmResult {
                    actions,
                    memory_updates,
                }) => {
                    // Update memory
                    if let Some(mem) = memory_updates {
                        client_data.lock().await.memory = mem;
                    }

                    // Execute initial actions
                    Self::execute_actions(
                        actions,
                        protocol.clone(),
                        &interface,
                        client_id,
                        &app_state,
                        &llm_client,
                        &status_tx,
                        &client_data,
                    )
                    .await;
                }
                Err(e) => {
                    error!("LLM error for USB client {}: {}", client_id, e);
                }
            }
        }

        // The command loop is now the task that keeps this client alive: USB devices
        // don't push data at us, so there is nothing else to wait on, and an idle
        // `sleep(60)` loop kept the client alive while being unable to do anything.
        // It ends when the channel closes (the client was removed) or an injected
        // `detach_device` disconnects.

        Ok(fake_addr)
    }

    /// Drain injected commands until the channel closes (the client was removed,
    /// which drops the handle) or an injected `detach_device` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot serve this
    /// client: it owns no socket, and every USB verb yields a
    /// `ClientActionResult::Custom` that only nusb can carry out. So the action goes
    /// through [`Self::apply_usb_result`] — the exact function the LLM path uses,
    /// including the follow-up events — and the outcome is recorded and replied the
    /// way the generic arm does it.
    #[allow(clippy::too_many_arguments)]
    async fn command_loop(
        mut command_rx: tokio::sync::mpsc::Receiver<crate::state::client_handles::ClientCommand>,
        protocol: Arc<UsbClientProtocol>,
        interface: nusb::Interface,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
        client_data: Arc<Mutex<ClientData>>,
    ) {
        use crate::llm::actions::client_trait::Client;
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::client_handles::ClientSendOutcome;
        use crate::state::AccessLogOwner;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();

            // `execute_action` is the only step that can fail before the device is
            // touched, so its error is a rejection (unknown verb / bad params) rather
            // than a transfer failure.
            let outcome = match protocol.as_ref().execute_action(action.clone()) {
                Err(e) => ClientSendOutcome::Rejected {
                    error: e.to_string(),
                },
                Ok(result) => match Self::apply_usb_result(
                    result,
                    protocol.clone(),
                    &interface,
                    client_id,
                    &app_state,
                    &llm_client,
                    &status_tx,
                    &client_data,
                )
                .await
                {
                    UsbApplied::Sent(bytes_sent) => ClientSendOutcome::Sent { bytes_sent },
                    UsbApplied::Executed(detail) => ClientSendOutcome::Executed { detail },
                    UsbApplied::Disconnected => ClientSendOutcome::Disconnected,
                },
            };

            let outcome_json = serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null);
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![outcome_json],
                )
                .await;

            let disconnect = matches!(outcome, ClientSendOutcome::Disconnected);
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, Ok(outcome));

            if disconnect {
                break;
            }
        }

        // Every exit path lands here: drop the command handle so the dashboard stops
        // offering [ send ] on a detached device and a late send fails fast.
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Execute USB actions from LLM
    #[allow(clippy::too_many_arguments)]
    async fn execute_actions(
        actions: Vec<serde_json::Value>,
        protocol: Arc<UsbClientProtocol>,
        interface: &nusb::Interface,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
    ) {
        use crate::llm::actions::client_trait::Client;

        for action in actions {
            match protocol.as_ref().execute_action(action) {
                Ok(result) => {
                    if matches!(
                        Self::apply_usb_result(
                            result,
                            protocol.clone(),
                            interface,
                            client_id,
                            app_state,
                            llm_client,
                            status_tx,
                            client_data,
                        )
                        .await,
                        UsbApplied::Disconnected
                    ) {
                        break;
                    }
                }
                Err(e) => {
                    error!("USB client {} action error: {}", client_id, e);
                }
            }
        }
    }

    /// Carry one already-decoded action out against the claimed interface. Shared by
    /// the connected-event path, the follow-up event path and injected commands, so
    /// the nusb transfer calls exist exactly once.
    #[allow(clippy::too_many_arguments)]
    async fn apply_usb_result(
        action_result: crate::llm::actions::client_trait::ClientActionResult,
        protocol: Arc<UsbClientProtocol>,
        interface: &nusb::Interface,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
    ) -> UsbApplied {
        {
            match action_result {
                crate::llm::actions::client_trait::ClientActionResult::Custom { name, data } => {
                    match name.as_str() {
                        "control_transfer" => {
                            // Read, never unwrapped. These fields arrive from
                            // `UsbClientProtocol::execute_action`, which validates them --
                            // but this runs inside `tokio::spawn`, where a panic is swallowed:
                            // the client task would die, the client would stay `Connected`,
                            // and nothing would say why. A malformed field is now an error the
                            // model is told about.
                            let (request_type, request, value, index) = match (
                                byte_of(&data, "request_type"),
                                byte_of(&data, "request"),
                                word_of(&data, "value"),
                                word_of(&data, "index"),
                            ) {
                                (Some(rt), Some(r), Some(v), Some(i)) => (rt, r, v, i),
                                _ => {
                                    error!(
                                        "USB client {} control_transfer has malformed fields: {}",
                                        client_id, data
                                    );
                                    return UsbApplied::Executed(
                                        "control_transfer: request_type, request, value and \
                                         index must each be an integer in range"
                                            .to_string(),
                                    );
                                }
                            };
                            let out_data = data["data"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                                        .collect::<Vec<u8>>()
                                })
                                .unwrap_or_default();
                            let length = length_of(&data).unwrap_or(0);

                            trace!(
                                "USB client {} control transfer: type={:02x} req={:02x} val={:04x} idx={:04x}",
                                client_id, request_type, request, value, index
                            );

                            // Execute control transfer. The completion's `status` is
                            // checked on BOTH directions: the OUT branch used to
                            // `let _ =` the completion, so a stalled or refused
                            // control-OUT was indistinguishable from a delivered one
                            // and could not be reported truthfully.
                            let interface_clone = interface.clone();
                            let is_in = out_data.is_empty() && length > 0;
                            let out_len = out_data.len();
                            let result: std::result::Result<Vec<u8>, String> = if is_in {
                                // IN transfer
                                let control_in = ControlIn {
                                    control_type: ControlType::Vendor,
                                    recipient: Recipient::Device,
                                    request,
                                    value,
                                    index,
                                    length: length as u16,
                                };
                                let completion = interface_clone.control_in(control_in).await;
                                match completion.status {
                                    Ok(()) => Ok(completion.data.to_vec()),
                                    Err(e) => Err(e.to_string()),
                                }
                            } else {
                                // OUT transfer
                                let control_out = ControlOut {
                                    control_type: ControlType::Vendor,
                                    recipient: Recipient::Device,
                                    request,
                                    value,
                                    index,
                                    data: &out_data,
                                };
                                let completion = interface_clone.control_out(control_out).await;
                                match completion.status {
                                    Ok(()) => Ok(Vec::new()),
                                    Err(e) => Err(e.to_string()),
                                }
                            };

                            match result {
                                Ok(response_data) if !response_data.is_empty() => {
                                    let received = response_data.len();
                                    debug!(
                                        "USB client {} control transfer received {} bytes",
                                        client_id, received
                                    );

                                    // Send event to LLM
                                    let event = Event::new(
                                        &USB_CONTROL_RESPONSE_EVENT,
                                        serde_json::json!({
                                            "data_hex": hex::encode(&response_data),
                                            "data_length": received,
                                        }),
                                    );

                                    if let Some(instruction) =
                                        app_state.get_instruction_for_client(client_id).await
                                    {
                                        // Copy the memory out before the call: the command
                                        // loop shares this mutex and a guard held across an
                                        // LLM round-trip would stall it.
                                        let memory = client_data.lock().await.memory.clone();
                                        if let Ok(ClientLlmResult {
                                            actions: new_actions,
                                            memory_updates,
                                        }) = call_llm_for_client(
                                            llm_client,
                                            app_state,
                                            client_id.to_string(),
                                            &instruction,
                                            &memory,
                                            Some(&event),
                                            protocol.as_ref(),
                                            status_tx,
                                        )
                                        .await
                                        {
                                            if let Some(mem) = memory_updates {
                                                client_data.lock().await.memory = mem;
                                            }
                                            Box::pin(Self::execute_actions(
                                                new_actions,
                                                protocol.clone(),
                                                interface,
                                                client_id,
                                                app_state,
                                                llm_client,
                                                status_tx,
                                                client_data,
                                            ))
                                            .await;
                                        }
                                    }
                                    UsbApplied::Executed(format!(
                                        "control_transfer IN: {received} bytes received"
                                    ))
                                }
                                Ok(_) => {
                                    trace!("USB client {} control transfer completed", client_id);
                                    if is_in {
                                        UsbApplied::Executed(
                                            "control_transfer IN: 0 bytes received".to_string(),
                                        )
                                    } else {
                                        // A control-OUT whose completion status is Ok really
                                        // put `out_len` payload bytes on the USB wire.
                                        UsbApplied::Sent(out_len)
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "USB client {} control transfer error: {}",
                                        client_id, e
                                    );
                                    UsbApplied::Executed(format!("control_transfer failed: {e}"))
                                }
                            }
                        }
                        "bulk_transfer_out" => {
                            let Some(endpoint) = byte_of(&data, "endpoint") else {
                                error!(
                                    "USB client {} bulk_transfer_out has no usable endpoint: {}",
                                    client_id, data
                                );
                                return UsbApplied::Executed(
                                    "bulk_transfer_out: 'endpoint' must be an integer in 0..=255"
                                        .to_string(),
                                );
                            };
                            let out_data = data["data"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                                        .collect::<Vec<u8>>()
                                })
                                .unwrap_or_default();

                            trace!(
                                "USB client {} bulk OUT transfer: endpoint={:02x} length={}",
                                client_id,
                                endpoint,
                                out_data.len()
                            );

                            let interface_clone = interface.clone();
                            let out_len = out_data.len();
                            let result = interface_clone.bulk_out(endpoint, out_data).await;

                            match result.status {
                                Ok(_) => {
                                    trace!("USB client {} bulk OUT completed", client_id);
                                    // The transfer completed, so these bytes really
                                    // reached the device.
                                    UsbApplied::Sent(out_len)
                                }
                                Err(e) => {
                                    error!("USB client {} bulk OUT error: {}", client_id, e);
                                    UsbApplied::Executed(format!(
                                        "bulk_transfer_out ep=0x{endpoint:02x} failed: {e}"
                                    ))
                                }
                            }
                        }
                        "bulk_transfer_in" => {
                            let (Some(endpoint), Some(length)) =
                                (byte_of(&data, "endpoint"), length_of(&data))
                            else {
                                error!(
                                    "USB client {} bulk_transfer_in has malformed fields: {}",
                                    client_id, data
                                );
                                return UsbApplied::Executed(
                                    "bulk_transfer_in: 'endpoint' must be 0..=255 and 'length' a \
                                     bounded byte count"
                                        .to_string(),
                                );
                            };

                            trace!(
                                "USB client {} bulk IN transfer: endpoint={:02x} length={}",
                                client_id,
                                endpoint,
                                length
                            );

                            let interface_clone = interface.clone();
                            let buffer = RequestBuffer::new(length);
                            let result = interface_clone.bulk_in(endpoint, buffer).await;

                            let response_data = result.data.to_vec();
                            let received = response_data.len();
                            if !response_data.is_empty() {
                                debug!(
                                    "USB client {} bulk IN received {} bytes",
                                    client_id, received
                                );

                                // Send event to LLM
                                let event = Event::new(
                                    &USB_BULK_DATA_RECEIVED_EVENT,
                                    serde_json::json!({
                                        "data_hex": hex::encode(&response_data),
                                        "data_length": response_data.len(),
                                        "endpoint": endpoint,
                                    }),
                                );

                                if let Some(instruction) =
                                    app_state.get_instruction_for_client(client_id).await
                                {
                                    // Copy the memory out before the call: the command
                                    // loop shares this mutex and a guard held across an
                                    // LLM round-trip would stall it.
                                    let memory = client_data.lock().await.memory.clone();
                                    if let Ok(ClientLlmResult {
                                        actions: new_actions,
                                        memory_updates,
                                    }) = call_llm_for_client(
                                        llm_client,
                                        app_state,
                                        client_id.to_string(),
                                        &instruction,
                                        &memory,
                                        Some(&event),
                                        protocol.as_ref(),
                                        status_tx,
                                    )
                                    .await
                                    {
                                        if let Some(mem) = memory_updates {
                                            client_data.lock().await.memory = mem;
                                        }
                                        Box::pin(Self::execute_actions(
                                            new_actions,
                                            protocol.clone(),
                                            interface,
                                            client_id,
                                            app_state,
                                            llm_client,
                                            status_tx,
                                            client_data,
                                        ))
                                        .await;
                                    }
                                }
                            }
                            UsbApplied::Executed(format!(
                                "bulk_transfer_in ep=0x{endpoint:02x}: {received} bytes received"
                            ))
                        }
                        "interrupt_transfer_in" => {
                            let (Some(endpoint), Some(length)) =
                                (byte_of(&data, "endpoint"), length_of(&data))
                            else {
                                error!(
                                    "USB client {} interrupt_transfer_in has malformed fields: {}",
                                    client_id, data
                                );
                                return UsbApplied::Executed(
                                    "interrupt_transfer_in: 'endpoint' must be 0..=255 and 'length' a \
                                     bounded byte count"
                                        .to_string(),
                                );
                            };

                            trace!(
                                "USB client {} interrupt IN transfer: endpoint={:02x} length={}",
                                client_id,
                                endpoint,
                                length
                            );

                            let interface_clone = interface.clone();
                            let buffer = RequestBuffer::new(length);
                            let result = interface_clone.interrupt_in(endpoint, buffer).await;

                            let response_data = result.data.to_vec();
                            let received = response_data.len();
                            if !response_data.is_empty() {
                                debug!(
                                    "USB client {} interrupt IN received {} bytes",
                                    client_id, received
                                );

                                // Send event to LLM
                                let event = Event::new(
                                    &USB_INTERRUPT_DATA_RECEIVED_EVENT,
                                    serde_json::json!({
                                        "data_hex": hex::encode(&response_data),
                                        "data_length": response_data.len(),
                                        "endpoint": endpoint,
                                    }),
                                );

                                if let Some(instruction) =
                                    app_state.get_instruction_for_client(client_id).await
                                {
                                    // Copy the memory out before the call: the command
                                    // loop shares this mutex and a guard held across an
                                    // LLM round-trip would stall it.
                                    let memory = client_data.lock().await.memory.clone();
                                    if let Ok(ClientLlmResult {
                                        actions: new_actions,
                                        memory_updates,
                                    }) = call_llm_for_client(
                                        llm_client,
                                        app_state,
                                        client_id.to_string(),
                                        &instruction,
                                        &memory,
                                        Some(&event),
                                        protocol.as_ref(),
                                        status_tx,
                                    )
                                    .await
                                    {
                                        if let Some(mem) = memory_updates {
                                            client_data.lock().await.memory = mem;
                                        }
                                        Box::pin(Self::execute_actions(
                                            new_actions,
                                            protocol.clone(),
                                            interface,
                                            client_id,
                                            app_state,
                                            llm_client,
                                            status_tx,
                                            client_data,
                                        ))
                                        .await;
                                    }
                                }
                            }
                            UsbApplied::Executed(format!(
                                "interrupt_transfer_in ep=0x{endpoint:02x}: {received} bytes received"
                            ))
                        }
                        "claim_interface" => {
                            let interface_num =
                                byte_of(&data, "interface_number").unwrap_or_default();
                            info!(
                                "USB client {} interface {} already claimed, skipping",
                                client_id, interface_num
                            );
                            UsbApplied::Executed(format!(
                                "claim_interface {interface_num}: already claimed at connect"
                            ))
                        }
                        _ => {
                            warn!("USB client {} unknown custom action: {}", client_id, name);
                            UsbApplied::Executed(format!(
                                "custom result '{name}' has no USB handler"
                            ))
                        }
                    }
                }
                crate::llm::actions::client_trait::ClientActionResult::Disconnect => {
                    info!("USB client {} disconnecting", client_id);
                    app_state
                        .update_client_status(client_id, ClientStatus::Disconnected)
                        .await;
                    // Drop the command handle here rather than only in the command loop:
                    // the LLM can detach too, and a handle left behind would offer
                    // [ send ] into a detached device.
                    app_state.remove_client_handle(client_id).await;
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                    UsbApplied::Disconnected
                }
                other => {
                    debug!("USB client {} unhandled action result", client_id);
                    UsbApplied::Executed(format!("unhandled action result {other:?}"))
                }
            }
        }
    }
}
