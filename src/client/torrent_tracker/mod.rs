//! BitTorrent Tracker client implementation
pub mod actions;

pub use actions::TorrentTrackerClientProtocol;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::torrent_tracker::actions::{
    TRACKER_ANNOUNCE_RESPONSE_EVENT, TRACKER_SCRAPE_RESPONSE_EVENT,
};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::actions::protocol_trait::Protocol;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// BitTorrent tracker response (announce)
#[derive(Debug, Deserialize, Serialize)]
struct TrackerResponse {
    #[serde(rename = "failure reason")]
    failure_reason: Option<String>,
    #[serde(rename = "warning message")]
    warning_message: Option<String>,
    interval: Option<i64>,
    #[serde(rename = "min interval")]
    min_interval: Option<i64>,
    #[serde(rename = "tracker id")]
    tracker_id: Option<String>,
    complete: Option<i64>,
    incomplete: Option<i64>,
    peers: Option<serde_bencode::value::Value>,
}

/// BitTorrent tracker scrape response
#[derive(Debug, Deserialize, Serialize)]
struct ScrapeResponse {
    files: Option<serde_bencode::value::Value>,
}

/// BitTorrent Tracker client
pub struct TorrentTrackerClient;

impl TorrentTrackerClient {
    /// Connect to a BitTorrent tracker with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // BitTorrent tracker is HTTP-based, so we don't maintain a persistent connection
        // We'll just track the tracker URL and make HTTP requests as needed

        info!(
            "BitTorrent Tracker client {} initialized for {}",
            client_id, remote_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] BitTorrent Tracker client {} connected to {}",
            client_id, remote_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Command channel for injected actions (the dashboard's [ send ] row).
        // Registered - and already being drained by its own task - BEFORE the
        // connected-event LLM call, which a manual `*` rule can park for minutes: the
        // operator must be able to reach the client while it waits. This is also what
        // keeps the client reachable at all, since a tracker client has no read loop.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            client_id,
            remote_addr.clone(),
            app_state.clone(),
            llm_client.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Call LLM with connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let protocol = Arc::new(
                crate::client::torrent_tracker::actions::TorrentTrackerClientProtocol::new(),
            );
            let event = Event::new(
                &TRACKER_ANNOUNCE_RESPONSE_EVENT,
                serde_json::json!({
                    "tracker_url": remote_addr,
                    "status": "connected",
                }),
            );

            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            // Execute LLM call in background
            let app_state_clone = app_state.clone();
            let status_tx_clone = status_tx.clone();
            // Registered with AppState so stop_client can abort this task —
            // dropping a JoinHandle only detaches it in Tokio.
            let task_registrar = app_state.clone();
            let task_handle = tokio::spawn(async move {
                match call_llm_for_client(
                    &llm_client,
                    &app_state_clone,
                    client_id.to_string(),
                    &instruction,
                    &memory,
                    Some(&event),
                    protocol.as_ref(),
                    &status_tx_clone,
                )
                .await
                {
                    Ok(ClientLlmResult {
                        actions,
                        memory_updates,
                    }) => {
                        // Update memory
                        if let Some(mem) = memory_updates {
                            app_state_clone.set_memory_for_client(client_id, mem).await;
                        }

                        // Execute actions
                        for action in actions {
                            match protocol.as_ref().execute_action(action.clone()) {
                                Ok(result) => {
                                    if let Err(e) = Self::apply_action(
                                        client_id,
                                        result,
                                        Notify::Inline,
                                        &remote_addr,
                                        &app_state_clone,
                                        &llm_client,
                                        &status_tx_clone,
                                    )
                                    .await
                                    {
                                        error!("Failed to execute tracker action: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("Tracker action execution error: {}", e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("LLM error for Tracker client {}: {}", client_id, e);
                    }
                }
            });
            task_registrar
                .register_client_task(client_id, task_handle)
                .await;
        }

        // Return a dummy local address (tracker is HTTP-based)
        Ok("0.0.0.0:0".parse()?)
    }

    /// Drain injected commands until the channel closes (client removed) or an injected
    /// `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client: it writes
    /// `SendData` to a socket, and both tracker verbs yield `ClientActionResult::Custom`
    /// that has to become an HTTP GET against the tracker. So the action goes through
    /// [`Self::apply_action`] - the same function the connected-event path uses - and the
    /// outcome is recorded and replied exactly the way the generic arm does it.
    async fn command_loop(
        mut command_rx: tokio::sync::mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        tracker_url: String,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let protocol = crate::client::torrent_tracker::actions::TorrentTrackerClientProtocol::new();

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                // The HTTP GET is awaited, so the reported outcome describes a request
                // that has actually completed. Notify::Deferred delivers the tracker
                // response event from its own registered task, so a manual handler parked
                // for a human's think time cannot wedge this loop or time out the
                // dashboard's [ send ].
                Ok(result) => Self::apply_action(
                    client_id,
                    result,
                    Notify::Deferred,
                    &tracker_url,
                    &app_state,
                    &llm_client,
                    &status_tx,
                )
                .await
                .map(|applied| match applied {
                    Applied::Disconnect => ClientSendOutcome::Disconnected,
                    Applied::Executed(detail) => ClientSendOutcome::Executed { detail },
                }),
            };

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
                error!("Tracker client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                break;
            }
        }

        info!("Tracker client {} command loop stopped", client_id);
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Deliver a tracker response event to the LLM, inline or from its own registered
    /// task depending on `notify`.
    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        notify: Notify,
        client_id: ClientId,
        event_type: &'static crate::protocol::EventType,
        event_data: serde_json::Value,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        match notify {
            Notify::Inline => {
                Self::notify_response(
                    client_id, event_type, event_data, app_state, llm_client, status_tx,
                )
                .await
            }
            Notify::Deferred => {
                let state_clone = app_state.clone();
                let llm_clone = llm_client.clone();
                let status_clone = status_tx.clone();
                let notify_handle = tokio::spawn(async move {
                    TorrentTrackerClient::notify_response(
                        client_id,
                        event_type,
                        event_data,
                        &state_clone,
                        &llm_clone,
                        &status_clone,
                    )
                    .await;
                });
                // Registered so the notification - and the LLM call it makes - is aborted
                // when the client is stopped.
                app_state
                    .register_client_task(client_id, notify_handle)
                    .await;
            }
        }
    }

    /// Fire one tracker response event at the LLM and apply any memory update.
    async fn notify_response(
        client_id: ClientId,
        event_type: &'static crate::protocol::EventType,
        event_data: serde_json::Value,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let event = Event::new(event_type, event_data);
        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();
        let protocol =
            Arc::new(crate::client::torrent_tracker::actions::TorrentTrackerClientProtocol::new());

        match call_llm_for_client(
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
            Ok(ClientLlmResult { memory_updates, .. }) => {
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }
            }
            Err(e) => {
                error!("LLM error: {}", e);
            }
        }
    }

    /// Apply one already-executed action result. The single place a tracker announce or
    /// scrape is issued from, so an injected action behaves exactly like an LLM-produced
    /// one.
    async fn apply_action(
        client_id: ClientId,
        result: ClientActionResult,
        notify: Notify,
        tracker_url: &str,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        match result {
            ClientActionResult::Custom { name, data } if name == "tracker_announce" => {
                let info_hash = data
                    .get("info_hash")
                    .and_then(|v| v.as_str())
                    .context("Missing info_hash")?;
                let peer_id = data
                    .get("peer_id")
                    .and_then(|v| v.as_str())
                    .context("Missing peer_id")?;
                let port = data
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .context("Missing port")? as u16;
                let uploaded = data.get("uploaded").and_then(|v| v.as_u64()).unwrap_or(0);
                let downloaded = data.get("downloaded").and_then(|v| v.as_u64()).unwrap_or(0);
                let left = data.get("left").and_then(|v| v.as_u64()).unwrap_or(0);
                let event_type = data
                    .get("event")
                    .and_then(|v| v.as_str())
                    .unwrap_or("started");

                // Build announce URL
                let announce_url = format!(
                    "{}?info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&event={}",
                    tracker_url, info_hash, peer_id, port, uploaded, downloaded, left, event_type
                );

                trace!(
                    "Tracker client {} announcing to: {}",
                    client_id,
                    announce_url
                );

                // Make HTTP GET request
                let response = reqwest::get(&announce_url).await?;
                let body = response.bytes().await?;

                // Parse bencode response
                match serde_bencode::from_bytes::<TrackerResponse>(&body) {
                    Ok(tracker_resp) => {
                        trace!("Tracker response: {:?}", tracker_resp);

                        Self::deliver(
                            notify,
                            client_id,
                            &TRACKER_ANNOUNCE_RESPONSE_EVENT,
                            serde_json::json!({
                                "interval": tracker_resp.interval,
                                "complete": tracker_resp.complete,
                                "incomplete": tracker_resp.incomplete,
                                "peers": format!("{:?}", tracker_resp.peers),
                            }),
                            app_state,
                            llm_client,
                            status_tx,
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Failed to parse tracker response: {}", e);
                        return Ok(Applied::Executed(format!(
                            "tracker_announce sent; the tracker's reply could not be \
                             bdecoded: {e}"
                        )));
                    }
                }
                Ok(Applied::Executed("tracker_announce completed".to_string()))
            }
            ClientActionResult::Custom { name, data } if name == "tracker_scrape" => {
                let info_hash = data
                    .get("info_hash")
                    .and_then(|v| v.as_str())
                    .context("Missing info_hash")?;

                // Build scrape URL
                let scrape_url = format!("{}?info_hash={}", tracker_url, info_hash);

                trace!("Tracker client {} scraping: {}", client_id, scrape_url);

                // Make HTTP GET request
                let response = reqwest::get(&scrape_url).await?;
                let body = response.bytes().await?;

                // Parse bencode response
                match serde_bencode::from_bytes::<ScrapeResponse>(&body) {
                    Ok(scrape_resp) => {
                        trace!("Scrape response: {:?}", scrape_resp);

                        Self::deliver(
                            notify,
                            client_id,
                            &TRACKER_SCRAPE_RESPONSE_EVENT,
                            serde_json::json!({
                                "files": format!("{:?}", scrape_resp.files),
                            }),
                            app_state,
                            llm_client,
                            status_tx,
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Failed to parse scrape response: {}", e);
                        return Ok(Applied::Executed(format!(
                            "tracker_scrape sent; the tracker's reply could not be \
                             bdecoded: {e}"
                        )));
                    }
                }
                Ok(Applied::Executed("tracker_scrape completed".to_string()))
            }
            ClientActionResult::Disconnect => {
                info!("Tracker client {} disconnecting", client_id);
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                // Every exit path drops the handle so the dashboard stops offering
                // [ send ] into a dead client.
                app_state.remove_client_handle(client_id).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                Ok(Applied::Disconnect)
            }
            other => Ok(Applied::Executed(format!(
                "{other:?} produced no tracker request"
            ))),
        }
    }
}

/// When an action's tracker response event is delivered to the LLM.
///
/// The announce/scrape GET itself is always awaited; only the notification moves. The
/// request is already inside a spawned task on the LLM-driven path, so unlike `http` there
/// is no second "spawn the request" mode here.
#[derive(Clone, Copy)]
enum Notify {
    /// Fire the event before returning. The LLM-driven path.
    Inline,
    /// Fire the event from its own registered task and return at once. The
    /// injected-command path, which must reply to the operator first.
    Deferred,
}

/// What [`TorrentTrackerClient::apply_action`] did with one action. A tracker client owns
/// no socket - each announce/scrape is a one-shot HTTP GET - so there is no honest byte
/// count to report, only "the request ran" or "the session should end".
enum Applied {
    /// The action ran; the string says what, for the injected action's outcome detail.
    Executed(String),
    /// The session should end.
    Disconnect,
}
