//! BLE beacon server (iBeacon / Eddystone).
//!
//! A beacon is not a GATT server. It accepts no connections, exposes no characteristics, and
//! answers no reads: it *is* its advertising payload. That is why this protocol no longer wraps
//! the `bluetooth-ble` base stack — the base can only advertise a device name and a service-UUID
//! list, which is precisely the one thing a beacon cannot be built out of.
//!
//! Instead it owns a [`advertise::BeaconAdvertiser`], which on Linux registers an
//! `org.bluez.LEAdvertisement1` object with `ManufacturerData` / `ServiceData` on
//! `org.bluez.LEAdvertisingManager1`, and on every other platform refuses to start.
//!
//! # Shape of the server
//!
//! There is no accept loop and no event loop, because nothing ever arrives: a legacy beacon
//! advertisement is one-way. So there is no `JoinHandle` to hand to
//! `AppState::register_server_task()`. What there *is* is a live-instance handle
//! ([`BeaconServer`]) registered with `AppState::register_server_handle()`, which is how the
//! protocol's actions reach the running adapter — and which `AppState::teardown_server` drops
//! on stop, taking the `bluer` advertisement handle with it and unregistering the advertisement
//! from `bluetoothd`.
//!
//! One event is emitted, `beacon_started`, exactly once, from `spawn`. It is the only event this
//! protocol declares, because declaring one it never emits would advertise actions to the model
//! that can never fire.

pub mod actions;
pub mod advertise;
pub mod payload;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::{console_error, console_info, console_warn};
use actions::{BluetoothBleBeaconProtocol, BEACON_STARTED_EVENT};
use advertise::BeaconAdvertiser;
use payload::BeaconFrame;

/// Live-instance handle for a running beacon server.
///
/// Registered with `AppState::register_server_handle()` in `spawn` and looked up by
/// `BluetoothBleBeaconProtocol::execute_action_with_state`, which is the only way an action can
/// reach the adapter — the protocol object the registry holds is zero-sized and has no adapter
/// of its own.
pub struct BeaconServer {
    advertiser: Mutex<BeaconAdvertiser>,
    status_tx: mpsc::UnboundedSender<String>,
}

impl BeaconServer {
    /// Put `frame` on air, replacing whatever was there.
    pub async fn start_beacon(&self, frame: BeaconFrame) -> Result<String> {
        let description = frame.describe();
        let mut advertiser = self.advertiser.lock().await;
        advertiser.start(frame).await?;
        let adapter = advertiser.adapter_name().to_string();
        drop(advertiser);

        console_info!(
            self.status_tx,
            "Beacon advertising on {}: {}",
            adapter,
            description
        );
        Ok(description)
    }

    /// Stop advertising. Idempotent — stopping an idle beacon is not an error.
    pub async fn stop_beacon(&self) -> Option<String> {
        let mut advertiser = self.advertiser.lock().await;
        let previous = advertiser.current().map(BeaconFrame::describe);
        advertiser.stop().await;
        drop(advertiser);

        match &previous {
            Some(what) => console_info!(self.status_tx, "Beacon stopped advertising: {}", what),
            None => console_info!(self.status_tx, "Beacon was not advertising"),
        }
        previous
    }

    /// What is currently on air, if anything.
    pub async fn current(&self) -> Option<BeaconFrame> {
        self.advertiser.lock().await.current().cloned()
    }

    /// The adapter this server is bound to.
    pub async fn adapter_name(&self) -> String {
        self.advertiser.lock().await.adapter_name().to_string()
    }
}

/// The single source of the text used when the handler could not configure the beacon.
///
/// Kept in one place, and `pub`, for the same reason as
/// [`advertise::UNSUPPORTED_PLATFORM_MESSAGE`]: it is the only thing a user or a test can
/// observe about this failure, so it must say what happened *and* what the consequence is —
/// "nothing is being advertised" — rather than leaving a bare `anyhow` chain to be read as a
/// transient hiccup. The project forbids `#[cfg(test)]` modules in `src/`, and on macOS and
/// Windows `spawn` refuses before it ever reaches the model call, so this is the only part of
/// the fail-closed path a test can reach off Linux.
///
/// It carries the `decision=` tag as well as the prose, because a beacon puts nothing on the
/// wire on any failure path: the log line *is* the whole distinction between a saturated
/// backend, a broken one, and a handler that deliberately configured no frame.
pub fn beacon_configuration_failure(
    device_name: &str,
    adapter: &str,
    err: &anyhow::Error,
) -> String {
    let (decision, overloaded) = if crate::llm::is_overload_error(err) {
        (
            "decision=fail_closed_llm_overloaded",
            " The LLM backend was saturated rather than broken, so starting the server again may \
             succeed.",
        )
    } else {
        ("decision=fail_closed_llm_error", "")
    };
    format!(
        "BLE beacon '{device_name}' on adapter {adapter} could not be configured \
         ({decision}): the handler failed ({err}). NOTHING is being advertised and the server \
         is not running - a beacon is its advertising payload, so there is no useful default \
         to fall back to and inventing one would put an unattributable frame on the \
         air.{overloaded}"
    )
}

/// BLE Beacon server
pub struct BluetoothBleBeacon;

impl BluetoothBleBeacon {
    /// Start the beacon server.
    ///
    /// Fails — rather than reporting `Running` — when the platform cannot set an advertising
    /// payload, when no adapter is present, or when `bluetoothd` is unreachable. All three are
    /// detected before `spawn` returns, so `server_startup.rs` records `ServerStatus::Error`
    /// with the reason instead of a server that is up and broadcasting nothing.
    pub async fn spawn_with_llm_actions(
        device_name: String,
        adapter: Option<String>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        instruction: String,
    ) -> Result<std::net::SocketAddr> {
        info!("Starting BLE beacon server");

        // Open the adapter first: everything below assumes an advertisement can be registered,
        // and a failure here is the honest "this platform/host cannot do it" answer.
        //
        // Tagged with its own token rather than a `fail_closed_llm_*` one: no model was asked,
        // and conflating "the radio cannot do this at all" with "the backend was down" would
        // send someone to restart Ollama over a macOS CoreBluetooth limit.
        let advertiser = match BeaconAdvertiser::open(device_name.clone(), adapter).await {
            Ok(advertiser) => advertiser,
            Err(e) => {
                console_error!(
                    status_tx,
                    "BLE beacon '{}' cannot start (decision=refused_adapter_unavailable): {}. \
                     NOTHING is being advertised and the server is not running.",
                    device_name,
                    e
                );
                return Err(e);
            }
        };
        let adapter_name = advertiser.adapter_name().to_string();

        console_info!(
            status_tx,
            "BLE beacon ready on adapter {} (device name '{}')",
            adapter_name,
            device_name
        );

        let server = Arc::new(BeaconServer {
            advertiser: Mutex::new(advertiser),
            status_tx: status_tx.clone(),
        });

        // Must be registered *before* the LLM call: the actions that call answers with are
        // dispatched through `execute_action_with_state`, which looks the handle up by
        // server_id and would otherwise find nothing.
        app_state
            .register_server_handle(server_id, server.clone())
            .await;

        let protocol = BluetoothBleBeaconProtocol::new();
        let started_event = Event::new(
            &BEACON_STARTED_EVENT,
            serde_json::json!({
                "device_name": device_name,
                "adapter": adapter_name,
                "instruction": instruction,
            }),
        );

        // A failure here is fatal to the server, deliberately.
        //
        // For a GATT server the model answers *traffic*, so the base `bluetooth-ble` stack
        // keeps a powered adapter up when this call fails. A beacon has no traffic: the
        // configuration produced by this one call *is* the entire server. Reporting `Running`
        // with nothing on air would be the fail-open shape the root CLAUDE.md warns about —
        // indistinguishable, to anyone reading `list_servers`, from a beacon that is
        // broadcasting. So the adapter is released and `server_startup` records
        // `ServerStatus::Error` with the reason.
        //
        // `stop_beacon()` first because it is cheap and unconditional: `call_llm` reports a
        // failure before executing any action, but a partially-configured radio left
        // transmitting an unattributable frame is exactly the outcome that must not be
        // possible, and asserting that from here costs one lock.
        let result = match call_llm(
            &llm_client,
            &app_state,
            server_id,
            None, // beacons have no connections
            &started_event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                server.stop_beacon().await;
                let reason = beacon_configuration_failure(&device_name, &adapter_name, &e);
                console_error!(status_tx, "{}", reason);
                return Err(e.context(reason));
            }
        };

        // No fallback beacon. If the model (or the configured handler) named no frame, nothing
        // is broadcast and that is said out loud — inventing a default UUID would put a beacon
        // on the air that nobody asked for and that no scanner could attribute.
        //
        // Which *kind* of nothing it is has to come from the log, because the air is identical
        // in all three cases. What is on air is the authority — an action that claimed to start
        // a frame and failed leaves `current()` empty — so the classification is taken from
        // `failures` and `raw_actions` only once that has been checked.
        let refused: Vec<String> = result
            .failures
            .iter()
            .map(|f| format!("{}: {}", f.action, f.error))
            .collect();
        let asked_to_stop = result
            .raw_actions
            .iter()
            .any(|a| a.get("type").and_then(|v| v.as_str()) == Some("stop_beacon"));
        let action_count = result.raw_actions.len();

        match server.current().await {
            Some(frame) => {
                console_info!(
                    status_tx,
                    "BLE beacon '{}' on adapter {} is advertising (decision=model_answer): {}",
                    device_name,
                    adapter_name,
                    frame.describe()
                );
            }
            None if !refused.is_empty() => {
                console_error!(
                    status_tx,
                    "BLE beacon '{}' on adapter {} is idle \
                     (decision=fail_closed_bad_action): the handler answered, but no action it \
                     produced could be executed ({}). NOTHING is being advertised.",
                    device_name,
                    adapter_name,
                    refused.join("; ")
                );
            }
            None if asked_to_stop => {
                console_info!(
                    status_tx,
                    "BLE beacon '{}' on adapter {} is idle (decision=model_reject): the handler \
                     answered with stop_beacon, so nothing is advertised on purpose.",
                    device_name,
                    adapter_name
                );
            }
            None => {
                console_warn!(
                    status_tx,
                    "BLE beacon '{}' on adapter {} is idle (decision=model_silent): no \
                     start_ibeacon / start_eddystone_uid / start_eddystone_url action was \
                     produced ({} action(s) returned). NOTHING is being advertised; use one of \
                     those actions to begin broadcasting.",
                    device_name,
                    adapter_name,
                    action_count
                );
            }
        }

        // BLE has no IP address or port; the registry's display layer wants a SocketAddr, so
        // report the "binds no listening socket" placeholder the same way the bluetooth-ble
        // base does. Both used to return `127.0.0.1:{5900 + server_id % 100}`, which
        // `server_startup::is_bound_addr` accepts (it only rejects port 0), so the TUI showed
        // a loopback endpoint nothing had bound — on VNC's port, no less.
        Ok(std::net::SocketAddr::from((
            std::net::Ipv4Addr::UNSPECIFIED,
            0,
        )))
    }
}
