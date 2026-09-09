//! DataLink client implementation for raw Ethernet frame injection
//!
//! # Lifecycle
//!
//! `connect_with_llm_actions` **awaits readiness**: the blocking pcap task reports over a
//! oneshot whether it found the device and opened a capture handle, and `connect` returns
//! `Err` when it did not. Without that the client reported `Connected` on a host with no BPF
//! access, having opened nothing — the family bug (`arp`, `datalink`, `icmp`, `isis` all had
//! it on the server side, where it is already fixed).
//!
//! # Shutdown
//!
//! `JoinHandle::abort()` cannot interrupt a thread parked in `pcap::next_packet()`, so the
//! blocking loop is stopped cooperatively through [`crate::utils::StopSignal`], registered
//! with `register_client_task` exactly as the server side does. `remove_client` aborts that
//! parked task, the guard trips the flag, and the loop exits at its next poll (≤10ms in
//! injection-only mode, ≤100ms while parked in `next_packet`). Before this the loop ran
//! forever: it held the capture handle after the client was gone, and — measurably — hung any
//! process that dropped its runtime while a capture was open. `injected_frame_is_transmitted`
//! did not fail, it hung the whole test binary in `BlockingPool::shutdown`.
//!
//! # Bounds
//!
//! Two things here would otherwise be unbounded, and both are cheap to bound:
//!
//! * **The inject → report → inject chain.** A frame that goes out raises
//!   `datalink_frame_injected`, which asks the model, which may inject again. That is a real
//!   cycle with an LLM call per hop, so [`InjectionCommand::llm_depth`] caps it at
//!   [`MAX_INJECTION_FOLLOWUPS`] — the `MAX_FOLLOWUP_DEPTH` pattern the repo prescribes,
//!   carried through the queue rather than the stack. A frame the operator injects from the
//!   dashboard always starts a fresh chain at depth 0.
//! * **Captured frames arriving while the model is busy.** They used to be pushed onto a
//!   `Vec` that was only ever `clear()`ed, so a busy interface grew the heap by up to 64 KiB
//!   per frame to no purpose whatever. They are counted and dropped.

pub mod actions;

pub use actions::DataLinkClientProtocol;

use anyhow::{Context, Result};
use pcap::{Capture, Device};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace, warn};

use crate::client::datalink::actions::{
    DATALINK_CLIENT_CONNECTED_EVENT, DATALINK_CLIENT_FRAME_CAPTURED_EVENT,
    DATALINK_CLIENT_FRAME_INJECTED_EVENT,
};
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::{Event, StartupParams};
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};
use crate::utils::StopSignal;

/// Connection state for LLM processing
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConnectionState {
    Idle,
    Processing,
}

/// Per-client capture bookkeeping.
///
/// This exists to stop two LLM turns running for one client at once. It used to also hold a
/// `Vec<Vec<u8>>` of frames captured while busy and a second copy of the client's memory; the
/// queue was never drained (only `clear()`ed) and the memory copy diverged from the one in
/// `AppState`, which is the one every other path reads.
struct ClientData {
    state: ConnectionState,
    /// Frames dropped because a turn was already running. Reported, not accumulated.
    dropped_while_busy: u64,
}

/// Channel for sending frame injection commands to the pcap thread.
///
/// libpcap is a blocking API and its handle lives on the blocking task, so nothing else can
/// call `sendpacket`. `ack` is how an injected command learns whether the frame really went
/// out: the pcap loop reports the result back, which is what lets the command loop answer
/// `Sent { bytes_sent }` truthfully instead of guessing.
struct InjectionCommand {
    frame: Vec<u8>,
    ack: Option<tokio::sync::oneshot::Sender<std::result::Result<usize, String>>>,
    /// How many LLM turns deep into an inject → report → inject chain this frame is.
    ///
    /// 0 for anything the operator or the connected event started. The pcap loop only raises
    /// `datalink_frame_injected` while this is below [`MAX_INJECTION_FOLLOWUPS`], so a model
    /// that answers every injection with another injection stops instead of spending the
    /// LLM budget until something else kills it.
    llm_depth: u8,
}

/// How long an injected command waits for the pcap loop's acknowledgement.
const INJECTION_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum number of `inject → datalink_frame_injected → inject` hops in one chain.
const MAX_INJECTION_FOLLOWUPS: u8 = 4;

/// How many bytes of a captured or injected frame are hex-encoded into an event.
///
/// A frame can be 65535 bytes; all of it as hex is 131070 characters of prompt per event. The
/// true length is always in `frame_length`.
pub const MAX_HEX_BYTES_TO_MODEL: usize = 2048;

/// The event payload the model is given for one frame: hex of at most
/// [`MAX_HEX_BYTES_TO_MODEL`] bytes, with the fields that say so.
///
/// Pure, and public, so it can be asserted against literal frame bytes with no capture handle.
pub fn frame_event_fields(frame: &[u8]) -> serde_json::Value {
    let shown = frame.len().min(MAX_HEX_BYTES_TO_MODEL);
    serde_json::json!({
        "frame_hex": hex::encode(&frame[..shown]),
        "frame_length": frame.len(),
        "captured_length": shown,
        "truncated": shown < frame.len(),
    })
}

/// DataLink client that injects raw Ethernet frames
pub struct DataLinkClient;

impl DataLinkClient {
    /// Connect to a network interface for frame injection with integrated LLM actions
    pub async fn connect_with_llm_actions(
        _remote_addr: String, // Not used for DataLink (interface instead)
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract interface from startup_params
        let params = startup_params.as_ref().ok_or_else(|| {
            anyhow::anyhow!("DataLink client requires startup parameters (interface)")
        })?;

        let interface = params.get_string("interface")?;
        let promiscuous = params.get_optional_bool("promiscuous")?.unwrap_or(false);

        info!(
            "DataLink client {} opening interface: {} (promiscuous: {})",
            client_id, interface, promiscuous
        );

        // Create channel for frame injection commands
        let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<InjectionCommand>();
        let inject_tx_cmd = inject_tx.clone();
        let inject_tx_arc = Arc::new(Mutex::new(inject_tx));

        // Capture-side bookkeeping (one LLM turn at a time per client).
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            dropped_while_busy: 0,
        }));

        // Stops the blocking loop. Registered below, once the capture is genuinely open.
        let stop = StopSignal::new();
        let stop_in_loop = stop.clone();
        let stop_for_cmds = stop.clone();
        let stop_for_conn = stop.clone();

        // Whether the pcap handle really opened. `connect` returns Err if it did not, so a
        // client on a host without BPF access lands in ClientStatus::Error instead of sitting
        // in Connected having opened nothing.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

        // Clone for the blocking task
        let inject_tx_arc_thread = inject_tx_arc.clone();
        // ...and for the connected-event task below, which is spawned after the blocking
        // closure has taken ownership of `inject_tx_arc`.
        let inject_tx_conn = inject_tx_arc.clone();
        let interface_clone = interface.clone();
        let status_tx_clone = status_tx.clone();
        let app_state_clone = app_state.clone();
        let llm_client_clone = llm_client.clone();
        let client_data_clone = client_data.clone();

        // Spawn blocking task for pcap operations
        tokio::task::spawn_blocking(move || {
            let open_capture = || -> Result<pcap::Capture<pcap::Active>> {
                let device = Self::find_device(&interface_clone)
                    .with_context(|| format!("no such capture device '{}'", interface_clone))?;

                Capture::from_device(device)
                    .map(|c| c.promisc(promiscuous).snaplen(65535).timeout(100))
                    .and_then(|c| c.open())
                    .with_context(|| {
                        format!(
                            "failed to open pcap capture on '{}' (needs root, or read access \
                             to /dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux)",
                            interface_clone
                        )
                    })
            };

            let mut cap = match open_capture() {
                Ok(cap) => {
                    let _ = ready_tx.send(Ok(()));
                    cap
                }
                Err(e) => {
                    error!(
                        "DataLink client {} capture startup failed: {:#}",
                        client_id, e
                    );
                    let _ = status_tx_clone.send(format!(
                        "[ERROR] DataLink client {} capture startup failed: {:#}",
                        client_id, e
                    ));
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            info!("DataLink client {} capture opened successfully", client_id);
            let _ = status_tx_clone.send(format!(
                "[INFO] DataLink client {} ready for frame injection",
                client_id
            ));

            let runtime = tokio::runtime::Handle::current();

            // Main loop: handle injection commands and optionally capture frames.
            // Exits when the client is removed (which aborts the task holding `stop`) or the
            // capture errors.
            loop {
                if stop_in_loop.is_stopped() {
                    break;
                }

                // Check for injection commands (non-blocking)
                match inject_rx.try_recv() {
                    Ok(cmd) => {
                        let result = match cap.sendpacket(&cmd.frame[..]) {
                            Ok(_) => {
                                trace!(
                                    "DataLink client {} injected frame ({} bytes)",
                                    client_id,
                                    cmd.frame.len()
                                );
                                let _ = status_tx_clone.send(format!(
                                    "[TRACE] DataLink client {} injected frame ({} bytes)",
                                    client_id,
                                    cmd.frame.len()
                                ));

                                // Tell the model the frame went out. `datalink_frame_injected`
                                // was declared in get_event_types() and raised nowhere, so a
                                // model that injected a frame was never told it had worked and
                                // could not follow it with anything. Spawned onto the runtime
                                // rather than awaited: this is the blocking pcap thread, and
                                // the LLM call can park for minutes on a manual routing rule.
                                //
                                // Bounded: past MAX_INJECTION_FOLLOWUPS hops the frame still
                                // goes out, the model is simply not asked again.
                                if cmd.llm_depth < MAX_INJECTION_FOLLOWUPS {
                                    let ev_state = app_state_clone.clone();
                                    let ev_llm = llm_client_clone.clone();
                                    let ev_status = status_tx_clone.clone();
                                    let ev_inject = inject_tx_arc_thread.clone();
                                    let ev_stop = stop_in_loop.clone();
                                    let ev_frame = cmd.frame.clone();
                                    let ev_depth = cmd.llm_depth;
                                    runtime.spawn(async move {
                                        Self::report_injection(
                                            client_id, ev_frame, ev_depth, ev_state, ev_llm,
                                            ev_status, ev_inject, ev_stop,
                                        )
                                        .await;
                                    });
                                } else {
                                    warn!(
                                        "DataLink client {} decision=followup_depth_capped: \
                                         {}-byte frame sent, model not asked again after {} \
                                         inject→report→inject hops",
                                        client_id,
                                        cmd.frame.len(),
                                        MAX_INJECTION_FOLLOWUPS
                                    );
                                    let _ = status_tx_clone.send(format!(
                                        "[WARN] DataLink client {} stopped an injection chain \
                                         at depth {}",
                                        client_id, MAX_INJECTION_FOLLOWUPS
                                    ));
                                }

                                Ok(cmd.frame.len())
                            }
                            Err(e) => {
                                error!(
                                    "DataLink client {} frame injection failed: {}",
                                    client_id, e
                                );
                                let _ = status_tx_clone.send(format!(
                                    "[ERROR] DataLink client {} frame injection failed: {}",
                                    client_id, e
                                ));
                                Err(e.to_string())
                            }
                        };
                        if let Some(ack) = cmd.ack {
                            let _ = ack.send(result);
                        }
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }

                // If promiscuous mode, capture frames
                if promiscuous {
                    match cap.next_packet() {
                        Ok(packet) => {
                            let frame = packet.data.to_vec();
                            trace!(
                                "DataLink client {} captured frame ({} bytes)",
                                client_id,
                                frame.len()
                            );

                            let state_clone = app_state_clone.clone();
                            let llm_clone = llm_client_clone.clone();
                            let status_clone = status_tx_clone.clone();
                            let client_data_task = client_data_clone.clone();
                            let inject_tx_task = inject_tx_arc_thread.clone();
                            let stop_task = stop_in_loop.clone();

                            runtime.spawn(async move {
                                Self::handle_captured_frame(
                                    client_id,
                                    frame,
                                    client_data_task,
                                    state_clone,
                                    llm_clone,
                                    status_clone,
                                    inject_tx_task,
                                    stop_task,
                                )
                                .await;
                            });
                        }
                        Err(pcap::Error::TimeoutExpired) => {
                            // Normal timeout, continue
                        }
                        Err(e) => {
                            error!("DataLink client {} capture error: {}", client_id, e);
                            let _ = status_tx_clone.send(format!(
                                "[ERROR] DataLink client {} capture error: {}",
                                client_id, e
                            ));
                            break;
                        }
                    }
                } else {
                    // Nothing blocks in injection-only mode, so bound the poll rate and the
                    // latency with which a stop is noticed.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }

            info!("DataLink client {} disconnected", client_id);
            runtime.block_on(async {
                let dropped = client_data_clone.lock().await.dropped_while_busy;
                if dropped > 0 {
                    warn!(
                        "DataLink client {} dropped {} captured frame(s) that arrived while a \
                         model turn was already running",
                        client_id, dropped
                    );
                }
                app_state_clone
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                // The pcap handle is gone: drop the command handle so the dashboard stops
                // offering [ send ] on a client that can no longer inject anything. This
                // also closes the command channel, which ends `command_loop`.
                app_state_clone.remove_client_handle(client_id).await;
                let _ = status_tx_clone.send(format!(
                    "[CLIENT] DataLink client {} disconnected",
                    client_id
                ));
                let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
            });
        });

        // Wait for the blocking task to report whether the capture actually came up. Nothing
        // below this line runs for a client that has no capture handle.
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "DataLink capture task on '{}' exited before signalling readiness",
                    interface
                ))
            }
        }

        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] DataLink client {} connected to interface {}",
            client_id, interface
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Ends the blocking loop when the client is removed: `remove_client` aborts every
        // registered task, and dropping this one's future trips `stop`.
        app_state
            .register_client_task(client_id, stop.park_task())
            .await;

        // Command channel for injected actions (the dashboard's [ send ]). Registered before
        // the connected-event call below, which a `*` -> manual routing rule can park for
        // minutes — `[ send ]` must be live throughout that.
        let protocol = Arc::new(DataLinkClientProtocol::new());
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            protocol,
            inject_tx_cmd,
            client_id,
            app_state.clone(),
            status_tx.clone(),
            stop_for_cmds,
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Ask the model what to do now that the interface is open.
        //
        // This client used to make no connected-event LLM call at all, so a client created
        // with "inject an ARP request for 10.0.0.2" opened the capture and then sat there:
        // nothing consulted the model, so nothing was ever injected. Run from a registered
        // task rather than inline, because a dashboard-created client defaults to a
        // `*` -> manual rule and that call can park for minutes -- `connect` must return.
        let conn_state = app_state.clone();
        let conn_llm = llm_client.clone();
        let conn_status = status_tx.clone();
        let conn_inject = inject_tx_conn;
        let conn_interface = interface.clone();
        let conn_task = tokio::spawn(async move {
            let Some(instruction) = conn_state.get_instruction_for_client(client_id).await else {
                return;
            };
            let event = Event::new(
                &DATALINK_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "interface": conn_interface,
                    "promiscuous": promiscuous,
                }),
            );
            Self::run_llm_turn(
                client_id,
                &instruction,
                event,
                0,
                &conn_state,
                &conn_llm,
                &conn_status,
                &conn_inject,
                &stop_for_conn,
            )
            .await;
        });
        app_state.register_client_task(client_id, conn_task).await;

        // The interface name is stored in the client metadata
        Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// One captured frame: ask the model, unless a turn is already running for this client.
    ///
    /// The frames dropped here used to be pushed onto a `Vec` that nothing ever read, which
    /// on a busy interface grew without bound while the model was parked.
    #[allow(clippy::too_many_arguments)]
    async fn handle_captured_frame(
        client_id: ClientId,
        frame: Vec<u8>,
        client_data: Arc<Mutex<ClientData>>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
        inject_tx: Arc<Mutex<mpsc::UnboundedSender<InjectionCommand>>>,
        stop: StopSignal,
    ) {
        {
            let mut data = client_data.lock().await;
            if data.state != ConnectionState::Idle {
                data.dropped_while_busy += 1;
                let dropped = data.dropped_while_busy;
                drop(data);
                if dropped == 1 || dropped % 100 == 0 {
                    warn!(
                        "DataLink client {} dropped {} captured frame(s): a model turn was \
                         already running. Use promiscuous mode with a quiet interface, or a \
                         script/static handler, for high-rate capture.",
                        client_id, dropped
                    );
                }
                return;
            }
            data.state = ConnectionState::Processing;
        }

        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &DATALINK_CLIENT_FRAME_CAPTURED_EVENT,
                frame_event_fields(&frame),
            );
            Self::run_llm_turn(
                client_id,
                &instruction,
                event,
                0,
                &app_state,
                &llm_client,
                &status_tx,
                &inject_tx,
                &stop,
            )
            .await;
        }

        client_data.lock().await.state = ConnectionState::Idle;
    }

    /// Report a frame that really went out, and queue whatever the model answers with.
    #[allow(clippy::too_many_arguments)]
    async fn report_injection(
        client_id: ClientId,
        frame: Vec<u8>,
        llm_depth: u8,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
        inject_tx: Arc<Mutex<mpsc::UnboundedSender<InjectionCommand>>>,
        stop: StopSignal,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let event = Event::new(
            &DATALINK_CLIENT_FRAME_INJECTED_EVENT,
            frame_event_fields(&frame),
        );
        Self::run_llm_turn(
            client_id,
            &instruction,
            event,
            llm_depth.saturating_add(1),
            &app_state,
            &llm_client,
            &status_tx,
            &inject_tx,
            &stop,
        )
        .await;
    }

    /// One LLM turn: raise `event`, then queue any frames the model asks to inject.
    ///
    /// Deliberately does NOT recurse. Injecting a frame raises `datalink_frame_injected`
    /// from the pcap loop, which comes back here, so the chain continues through the queue
    /// rather than through the stack. `depth` is what bounds it: the frames queued here carry
    /// it, and the pcap loop stops raising the follow-up event past
    /// [`MAX_INJECTION_FOLLOWUPS`].
    #[allow(clippy::too_many_arguments)]
    async fn run_llm_turn(
        client_id: ClientId,
        instruction: &str,
        event: Event,
        depth: u8,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
        inject_tx: &Arc<Mutex<mpsc::UnboundedSender<InjectionCommand>>>,
        stop: &StopSignal,
    ) {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};

        let protocol = DataLinkClientProtocol::new();
        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();
        let event_id = event.event_type.id.clone();

        match call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            instruction,
            &memory,
            Some(&event),
            &protocol,
            status_tx,
        )
        .await
        {
            Ok(ClientLlmResult {
                actions,
                memory_updates,
            }) => {
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }
                // Nothing goes on the wire in three of the four outcomes, and they are
                // indistinguishable there, so the log has to tell them apart. Grep `decision=`.
                let mut injected = 0usize;
                let mut rejected = 0usize;
                let action_count = actions.len();
                for action in actions {
                    match protocol.execute_action(action) {
                        Ok(ClientActionResult::SendData(frame)) => {
                            injected += 1;
                            let _ = inject_tx.lock().await.send(InjectionCommand {
                                frame,
                                ack: None,
                                llm_depth: depth,
                            });
                        }
                        Ok(ClientActionResult::Disconnect) => {
                            info!("DataLink client {} decision=model_disconnect", client_id);
                            Self::shut_down(client_id, app_state, stop, status_tx).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            rejected += 1;
                            error!(
                                "DataLink client {} rejected action on {}: {}",
                                client_id, event_id, e
                            );
                        }
                    }
                }
                let decision = if injected > 0 {
                    "model_inject"
                } else if action_count == 0 {
                    "model_silent"
                } else if rejected == action_count {
                    "model_reject"
                } else {
                    "model_no_frame"
                };
                info!(
                    "DataLink client {} {} decision={} depth={} ({} action(s), {} injected)",
                    client_id, event_id, decision, depth, action_count, injected
                );
            }
            Err(e) => {
                // Nothing is written to the interface on failure. DataLink is one of the
                // deliberately silent protocols: a fabricated frame on a real network is
                // worse than no frame at all. The category is kept separate from the error
                // text so a saturated backend is distinguishable from a broken one.
                let category = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                    "overloaded"
                } else {
                    "unavailable"
                };
                error!(
                    "DataLink client {} {} decision=fail_closed_llm_error category={}, nothing \
                     injected: {}",
                    client_id, event_id, category, e
                );
                let _ = status_tx.send(format!(
                    "[ERROR] DataLink client {} decision=fail_closed_llm_error category={}: {}",
                    client_id, category, e
                ));
            }
        }
    }

    /// Tear the client down: stop the pcap loop, drop the command handle, mark it
    /// disconnected. The blocking loop notices `stop` within one poll and does the rest.
    async fn shut_down(
        client_id: ClientId,
        app_state: &Arc<AppState>,
        stop: &StopSignal,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        stop.stop();
        app_state.remove_client_handle(client_id).await;
        app_state
            .update_client_status(client_id, ClientStatus::Disconnected)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Drain injected commands until the channel closes (client removed, or the pcap loop
    /// exited and dropped the handle) or an injected `disconnect` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot serve this client:
    /// there is no `AsyncWrite` half at all. `inject_frame` yields
    /// `ClientActionResult::SendData`, which is handed to the same pcap injection queue the
    /// LLM path uses; the pcap loop acknowledges the `sendpacket` call so the reply can be
    /// truthful.
    #[allow(clippy::too_many_arguments)]
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        protocol: Arc<DataLinkClientProtocol>,
        inject_tx: mpsc::UnboundedSender<InjectionCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        stop: StopSignal,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = Self::execute_injected_action(&protocol, &inject_tx, &action).await;

            let outcome_json = match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            };
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

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            if let Err(e) = &outcome {
                error!(
                    "DataLink client {} injected action failed: {}",
                    client_id, e
                );
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                Self::shut_down(client_id, &app_state, &stop, &status_tx).await;
                break;
            }
        }
    }

    /// Execute one injected action and report exactly what happened to it.
    ///
    /// - `Sent { bytes_sent }` only after the pcap loop has acknowledged a successful
    ///   `sendpacket` for that many bytes.
    /// - `Executed { detail }` when the frame could not be handed over — the pcap loop has
    ///   exited, which for a live client means it is on its way down.
    /// - `Rejected { error }` for an action the protocol refuses (unknown type, bad hex,
    ///   a frame too short or too long to be an Ethernet frame).
    /// - `Err` when pcap accepted the frame and failed to transmit it.
    async fn execute_injected_action(
        protocol: &Arc<DataLinkClientProtocol>,
        inject_tx: &mpsc::UnboundedSender<InjectionCommand>,
        action: &serde_json::Value,
    ) -> Result<ClientSendOutcome> {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};

        let result = match protocol.as_ref().execute_action(action.clone()) {
            Ok(result) => result,
            Err(e) => {
                return Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                })
            }
        };

        let frame = match result {
            ClientActionResult::SendData(frame) => frame,
            ClientActionResult::Disconnect => return Ok(ClientSendOutcome::Disconnected),
            ClientActionResult::WaitForMore => {
                return Ok(ClientSendOutcome::Executed {
                    detail: "wait_for_more".to_string(),
                })
            }
            other => {
                return Ok(ClientSendOutcome::Executed {
                    detail: format!("{other:?} injects no frame"),
                })
            }
        };

        let frame_len = frame.len();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if inject_tx
            .send(InjectionCommand {
                frame,
                ack: Some(ack_tx),
                // The operator asking for a frame starts a fresh chain, however deep the
                // model's own chain went.
                llm_depth: 0,
            })
            .is_err()
        {
            // The pcap loop has exited and dropped the receiver. Since `connect` now awaits
            // the capture handle, this is a client on its way down rather than one that never
            // opened anything.
            return Ok(ClientSendOutcome::Executed {
                detail: format!(
                    "{frame_len}-byte frame built but not injected: the pcap capture is no \
                     longer open"
                ),
            });
        }

        match tokio::time::timeout(INJECTION_ACK_TIMEOUT, ack_rx).await {
            Ok(Ok(Ok(bytes_sent))) => Ok(ClientSendOutcome::Sent { bytes_sent }),
            Ok(Ok(Err(e))) => Err(anyhow::anyhow!("pcap sendpacket failed: {e}")),
            Ok(Err(_)) => Ok(ClientSendOutcome::Executed {
                detail: format!(
                    "{frame_len}-byte frame not injected: it was handed to the pcap loop, \
                     which exited without reporting the result"
                ),
            }),
            Err(_) => Ok(ClientSendOutcome::Executed {
                detail: format!(
                    "{frame_len}-byte frame not confirmed injected: the pcap loop did not \
                     acknowledge it within {}s",
                    INJECTION_ACK_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    /// Find a network device by name
    fn find_device(name: &str) -> Result<Device> {
        let devices = Device::list().context("Failed to list network devices")?;
        devices
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| anyhow::anyhow!("Network device '{}' not found", name))
    }
}
