//! Bluetooth Low Energy (BLE) Battery Service implementation
//!
//! Builds on bluetooth-ble to provide standard Battery Service (0x180F).
//! Reports battery level as a percentage (0-100%).

pub mod actions;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

use crate::llm::ollama_client::OllamaClient;
use crate::server::bluetooth_ble::BluetoothBle;
use crate::state::app_state::AppState;

/// BLE Battery Service server
pub struct BluetoothBleBattery;

impl BluetoothBleBattery {
    /// Spawn BLE battery service server
    #[cfg(feature = "bluetooth-ble-battery")]
    pub async fn spawn_with_llm_actions(
        device_name: String,
        initial_level: u8,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        instruction: String,
    ) -> Result<std::net::SocketAddr> {
        info!(
            "Starting BLE Battery Service: {} (initial level: {}%)",
            device_name, initial_level
        );

        // The user's instruction leads; the profile preamble is appended to it. Replacing it
        // outright would silently discard whatever the user actually asked the server to do.
        BluetoothBle::spawn_with_llm_actions(
            device_name,
            llm_client,
            app_state,
            status_tx,
            server_id,
            format!(
                "{}. Configure as a BLE Battery Service (0x180F) with a Battery Level \
                 characteristic (0x2A19) starting at {}%.",
                instruction.trim_end_matches('.'),
                initial_level
            ),
        )
        .await
    }
}

#[cfg(not(feature = "bluetooth-ble-battery"))]
impl BluetoothBleBattery {
    pub async fn spawn_with_llm_actions(
        _device_name: String,
        _initial_level: u8,
        _llm_client: OllamaClient,
        _app_state: Arc<AppState>,
        _status_tx: mpsc::UnboundedSender<String>,
        _server_id: crate::state::ServerId,
        _instruction: String,
    ) -> Result<std::net::SocketAddr> {
        anyhow::bail!(
            "BLE battery support not enabled - compile with --features bluetooth-ble-battery"
        )
    }
}

/// Battery Service UUIDs (Bluetooth SIG assigned numbers)
pub mod battery_uuids {
    /// Battery Service UUID
    pub const BATTERY_SERVICE: u16 = 0x180F;

    /// Battery Level characteristic UUID
    pub const BATTERY_LEVEL: u16 = 0x2A19;
}

/// Encode a Battery Level (0x2A19) characteristic value.
///
/// The characteristic is a single `uint8` carrying a **percentage**: the Bluetooth SIG Battery
/// Service 1.0 specification §3.1 defines 0 to 100 and reserves every other value. 75% is the
/// single octet `0x4B`.
///
/// **Returns `Err` rather than clamping.** This used to be `level.min(100)`, which turned 200
/// into a confident "100%". A percentage is the one type where truncation is least survivable:
/// every out-of-range value lands on a perfectly ordinary reading, so a caller's obvious
/// mistake becomes an unfalsifiable claim about a device's charge. There is no octet that
/// honestly represents 200%, so there is nothing to return.
pub fn encode_battery_level(level: u8) -> Result<[u8; 1]> {
    if level > 100 {
        anyhow::bail!(
            "Battery Level (0x2A19) is a percentage and the Bluetooth SIG reserves every \
             value above 100: {level} cannot be encoded. Refusing rather than clamping, \
             because a clamped 100 is indistinguishable from a full battery."
        );
    }
    Ok([level])
}
