//! WebRTC Signaling Server - WebSocket-based SDP relay for WebRTC connections
pub mod actions;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{
    accept_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use tracing::{debug, error, info, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::WebRtcSignalingProtocol;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use actions::{
    WEBRTC_SIGNALING_MESSAGE_RECEIVED_EVENT, WEBRTC_SIGNALING_PEER_CONNECTED_EVENT,
    WEBRTC_SIGNALING_PEER_DISCONNECTED_EVENT,
};

/// Unique identifier for a signaling peer
pub type PeerId = String;

/// Longest a peer may take to complete the WebSocket upgrade.
const SIGNALING_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Largest WebSocket message accepted. Signaling carries SDP and ICE candidates;
/// a large real offer is a few kilobytes.
const SIGNALING_MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// Largest number of peers that may be registered at once.
///
/// The registry is keyed on a string the peer chooses, so without this one host
/// can register until memory runs out. The protocol's own CLAUDE.md recommended
/// "1,000-10,000 concurrent peers" while nothing enforced anything.
const SIGNALING_MAX_PEERS: usize = 1024;

/// Longest peer id accepted. The id is echoed in log lines, in the registration
/// event and on every relayed frame, so an unbounded one is stored and repeated
/// many times over.
const MAX_PEER_ID_BYTES: usize = 128;

/// Signaling message types
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    /// Register with a peer ID
    #[serde(rename = "register")]
    Register { peer_id: String },

    /// SDP offer
    #[serde(rename = "offer")]
    Offer {
        from: String,
        to: String,
        sdp: serde_json::Value,
    },

    /// SDP answer
    #[serde(rename = "answer")]
    Answer {
        from: String,
        to: String,
        sdp: serde_json::Value,
    },

    /// ICE candidate
    #[serde(rename = "ice_candidate")]
    IceCandidate {
        from: String,
        to: String,
        candidate: serde_json::Value,
    },

    /// Error message
    #[serde(rename = "error")]
    Error { message: String },

    /// Registration success
    #[serde(rename = "registered")]
    Registered { peer_id: String },

    /// Generic relay message
    #[serde(rename = "relay")]
    Relay {
        from: String,
        to: String,
        data: serde_json::Value,
    },
}

/// Peer connection data
///
/// Holds a channel into the connection's writer task rather than the `SplitSink` itself.
/// That is the whole fix for the relay: `register_peer` used to take ownership of the
/// sink, so `handle_connection` could not keep reading and had to `break` immediately
/// after a successful registration — which ran the disconnect cleanup, unregistered the
/// peer that had just registered, and dropped its socket. No peer was ever registered for
/// longer than one statement, so `forward_message` could never find a recipient and not a
/// single offer, answer or ICE candidate was ever relayed.
struct PeerConnection {
    #[allow(dead_code)]
    peer_id: PeerId,
    /// Messages queued here are written by this connection's writer task.
    out_tx: mpsc::UnboundedSender<Message>,
    #[allow(dead_code)]
    remote_addr: SocketAddr,
    #[allow(dead_code)]
    connection_id: ConnectionId,
}

/// WebRTC signaling server shared state
pub struct WebRtcSignalingServerData {
    /// Connected peers indexed by peer ID
    peers: Arc<Mutex<HashMap<PeerId, PeerConnection>>>,
}

impl Default for WebRtcSignalingServerData {
    fn default() -> Self {
        Self::new()
    }
}

impl WebRtcSignalingServerData {
    pub fn new() -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a new peer
    async fn register_peer(
        &self,
        peer_id: PeerId,
        out_tx: mpsc::UnboundedSender<Message>,
        remote_addr: SocketAddr,
        connection_id: ConnectionId,
    ) -> Result<()> {
        if peer_id.is_empty() || peer_id.len() > MAX_PEER_ID_BYTES {
            anyhow::bail!(
                "Peer ID must be 1-{} bytes, got {}",
                MAX_PEER_ID_BYTES,
                peer_id.len()
            );
        }

        let mut peers = self.peers.lock().await;
        if peers.len() >= SIGNALING_MAX_PEERS {
            anyhow::bail!(
                "Signaling server is full ({} peers registered)",
                SIGNALING_MAX_PEERS
            );
        }
        if peers.contains_key(&peer_id) {
            anyhow::bail!("Peer ID {} already registered", peer_id);
        }

        peers.insert(
            peer_id.clone(),
            PeerConnection {
                peer_id: peer_id.clone(),
                out_tx,
                remote_addr,
                connection_id,
            },
        );
        info!("Registered signaling peer: {}", peer_id);

        Ok(())
    }

    /// Unregister a peer
    pub async fn unregister_peer(&self, peer_id: &str) {
        let mut peers = self.peers.lock().await;
        peers.remove(peer_id);
        info!("Unregistered signaling peer: {}", peer_id);
    }

    /// Forward message to a specific peer
    pub async fn forward_message(&self, to: &str, message: &SignalingMessage) -> Result<()> {
        let msg_json = serde_json::to_string(message)?;
        let peers = self.peers.lock().await;
        let peer_conn = peers.get(to).context(format!("Peer {} not found", to))?;
        peer_conn
            .out_tx
            .send(Message::Text(msg_json))
            .context("Peer's writer task has stopped")?;
        drop(peers);

        trace!("Forwarded message to peer {}: {:?}", to, message);
        Ok(())
    }

    /// Send a message to one peer without requiring it to be the relay target
    pub async fn send_to_peer(&self, peer_id: &str, text: String) -> Result<()> {
        let peers = self.peers.lock().await;
        let peer_conn = peers
            .get(peer_id)
            .context(format!("Peer {} not found", peer_id))?;
        peer_conn
            .out_tx
            .send(Message::Text(text))
            .context("Peer's writer task has stopped")?;
        Ok(())
    }

    /// List all connected peer IDs
    pub async fn list_peers(&self) -> Vec<String> {
        self.peers.lock().await.keys().cloned().collect()
    }

    /// Get peer count
    pub async fn peer_count(&self) -> usize {
        self.peers.lock().await.len()
    }
}

/// WebRTC signaling server
pub struct WebRtcSignalingServer;

impl WebRtcSignalingServer {
    /// Spawn the WebRTC signaling server
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "WebRTC Signaling server listening on {}",
            local_addr
        ));

        // Create server data
        //
        // This used to be followed by a `set_protocol_field("server_data_ptr",
        // Arc::into_raw(...) as usize)`. Nothing in the tree ever read that field
        // and nothing ever called `Arc::from_raw`, so it leaked one `Arc` per
        // server start and wrote a live heap address into server state that the
        // TUI and MCP surfaces will happily print — an ASLR disclosure bought for
        // a capability nobody used.
        let server_data = Arc::new(WebRtcSignalingServerData::new());

        let protocol = Arc::new(WebRtcSignalingProtocol::new());

        // Spawn accept loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        info!("Signaling server accepted connection from {}", remote_addr);

                        let server_data_clone = Arc::clone(&server_data);
                        let app_state_clone = Arc::clone(&app_state);
                        let status_tx_clone = status_tx.clone();
                        let llm_client_clone = llm_client.clone();
                        let protocol_clone = Arc::clone(&protocol);

                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                stream,
                                remote_addr,
                                server_data_clone,
                                app_state_clone,
                                status_tx_clone,
                                llm_client_clone,
                                server_id,
                                protocol_clone,
                            )
                            .await
                            {
                                error!(
                                    "Error handling signaling connection from {}: {}",
                                    remote_addr, e
                                );
                            }
                        });
                    }
                    Err(e) => {
                        error!("Error accepting signaling connection: {}", e);
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        stream: TcpStream,
        remote_addr: SocketAddr,
        server_data: Arc<WebRtcSignalingServerData>,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        llm_client: OllamaClient,
        server_id: ServerId,
        protocol: Arc<WebRtcSignalingProtocol>,
    ) -> Result<()> {
        // Upgrade to WebSocket.
        //
        // Two bounds, both absent before and both reachable pre-registration by
        // anyone who can open a TCP connection:
        //
        // * The handshake gets a deadline. A socket that connects and never sends
        //   an HTTP upgrade parked inside `accept_async` forever, holding a task
        //   and an fd while remaining invisible to the dashboard — a connection is
        //   only added to `AppState` once it registers.
        // * Frames get a size limit. Bare `accept_async` takes tungstenite's
        //   defaults of 64 MiB per message and 16 MiB per frame, so a single peer
        //   could make the server buffer 64 MiB, and a peer id is just a string in
        //   a frame. Signaling carries SDP and ICE candidates; the largest real
        //   offer is a few kilobytes, so 256 KiB is generous by two orders of
        //   magnitude and still bounds the damage.
        let config = WebSocketConfig {
            max_message_size: Some(SIGNALING_MAX_MESSAGE_BYTES),
            max_frame_size: Some(SIGNALING_MAX_MESSAGE_BYTES),
            ..Default::default()
        };
        let ws_stream = tokio::time::timeout(
            SIGNALING_HANDSHAKE_TIMEOUT,
            accept_async_with_config(stream, Some(config)),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "WebSocket handshake from {} did not complete within {:?}",
                remote_addr,
                SIGNALING_HANDSHAKE_TIMEOUT
            )
        })??;
        info!("WebSocket connection established with {}", remote_addr);

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // One writer task owns the sink. Everything that wants to write — this read loop,
        // and any other peer relaying towards us — queues on `out_tx` instead. Handing the
        // sink itself to the peer registry is what previously forced the read loop to
        // terminate on registration.
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = ws_tx.send(msg).await {
                    debug!("Signaling writer stopped: {}", e);
                    break;
                }
            }
            let _ = ws_tx.close().await;
        });

        let mut peer_id: Option<PeerId> = None;
        let mut connection_id: Option<ConnectionId> = None;

        // Handle incoming messages
        while let Some(msg_result) = ws_rx.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    trace!("Received signaling message: {}", text);

                    // Parse message
                    let message: SignalingMessage = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            // The error goes to the log; the peer gets a category. serde's
                            // message names the enum's variants and the exact column it
                            // choked on ("unknown variant `foo`, expected one of `register`,
                            // `offer`, ... at line 1 column 15"), and that was going verbatim
                            // to an unauthenticated stranger. Same rule as
                            // `crate::utils::WireFailure`, which does not apply here only
                            // because this is a parse failure rather than a backend one.
                            warn!(
                                "Invalid signaling message from {}: {}",
                                remote_addr,
                                crate::utils::truncate_for_log(&e.to_string(), 300)
                            );
                            let _ = Self::reply(
                                &out_tx,
                                &SignalingMessage::Error {
                                    message: "invalid signaling message: expected a JSON \
                                              object with a 'type' of register, offer, \
                                              answer, ice_candidate or relay"
                                        .to_string(),
                                },
                            );
                            continue;
                        }
                    };

                    match message {
                        SignalingMessage::Register {
                            peer_id: new_peer_id,
                        } => {
                            if peer_id.is_some() {
                                let _ = Self::reply(
                                    &out_tx,
                                    &SignalingMessage::Error {
                                        message: "This connection is already registered"
                                            .to_string(),
                                    },
                                );
                                continue;
                            }

                            let conn_id = ConnectionId::new(app_state.get_next_unified_id().await);

                            match server_data
                                .register_peer(
                                    new_peer_id.clone(),
                                    out_tx.clone(),
                                    remote_addr,
                                    conn_id,
                                )
                                .await
                            {
                                Ok(_) => {
                                    peer_id = Some(new_peer_id.clone());
                                    connection_id = Some(conn_id);

                                    // Add connection to server
                                    use crate::state::server::{
                                        ConnectionState as ServerConnectionState, ConnectionStatus,
                                        ProtocolConnectionInfo,
                                    };
                                    let now = std::time::Instant::now();
                                    let conn_state = ServerConnectionState {
                                        id: conn_id,
                                        remote_addr,
                                        local_addr: "0.0.0.0:0".parse().unwrap(),
                                        bytes_sent: 0,
                                        bytes_received: 0,
                                        packets_sent: 0,
                                        packets_received: 0,
                                        last_activity: now,
                                        status: ConnectionStatus::Active,
                                        status_changed_at: now,
                                        protocol_info: ProtocolConnectionInfo::new(
                                            serde_json::json!({
                                                "peer_id": new_peer_id,
                                            }),
                                        ),
                                    };
                                    app_state
                                        .add_connection_to_server(server_id, conn_state)
                                        .await;
                                    let _ = status_tx.send("__UPDATE_UI__".to_string());

                                    // Confirm registration. `SignalingMessage::Registered`
                                    // was defined but never sent, so a client waiting for
                                    // it (as the documented protocol says it may) hung.
                                    let _ = Self::reply(
                                        &out_tx,
                                        &SignalingMessage::Registered {
                                            peer_id: new_peer_id.clone(),
                                        },
                                    );
                                    Log::new(Some(&status_tx)).info(format!(
                                        "WebRTC signaling peer '{}' registered from {}",
                                        new_peer_id, remote_addr
                                    ));

                                    // Fire connected event
                                    let event = Event::new(
                                        &WEBRTC_SIGNALING_PEER_CONNECTED_EVENT,
                                        serde_json::json!({
                                            "peer_id": new_peer_id,
                                            "remote_addr": remote_addr.to_string(),
                                            "peer_count": server_data.peer_count().await,
                                        }),
                                    );

                                    match call_llm(
                                        &llm_client,
                                        &app_state,
                                        server_id,
                                        Some(conn_id),
                                        &event,
                                        protocol.as_ref(),
                                    )
                                    .await
                                    {
                                        Ok(result) => {
                                            if Self::apply_results(result, &out_tx, &status_tx) {
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            // This is the one signaling event the model can
                                            // act on (`send_signaling_message`,
                                            // `disconnect_peer`), so a peer may legitimately
                                            // be waiting for what the model decides. Swallowed,
                                            // that wait ended at the peer's own timeout. Say so
                                            // in the protocol's own vocabulary instead - and
                                            // never invent the reply the model did not give.
                                            Log::new(Some(&status_tx)).error(format!(
                                                "WebRTC signaling peer '{}': LLM call failed \
                                                 ({}) - sent error frame",
                                                new_peer_id, e
                                            ));
                                            let _ = Self::reply(
                                                &out_tx,
                                                &SignalingMessage::Error {
                                                    message: format!(
                                                        "Server-side handler for peer '{}' \
                                                         failed; registration stands but no \
                                                         handler response follows",
                                                        new_peer_id
                                                    ),
                                                },
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to register peer {}: {}", new_peer_id, e);
                                    let _ = Self::reply(
                                        &out_tx,
                                        &SignalingMessage::Error {
                                            message: e.to_string(),
                                        },
                                    );
                                }
                            }
                        }
                        SignalingMessage::Offer { .. }
                        | SignalingMessage::Answer { .. }
                        | SignalingMessage::IceCandidate { .. }
                        | SignalingMessage::Relay { .. } => {
                            let (kind, claimed_from, to) = match &message {
                                SignalingMessage::Offer { from, to, .. } => {
                                    ("offer", from.clone(), to.clone())
                                }
                                SignalingMessage::Answer { from, to, .. } => {
                                    ("answer", from.clone(), to.clone())
                                }
                                SignalingMessage::IceCandidate { from, to, .. } => {
                                    ("ice_candidate", from.clone(), to.clone())
                                }
                                SignalingMessage::Relay { from, to, .. } => {
                                    ("relay", from.clone(), to.clone())
                                }
                                _ => unreachable!(),
                            };

                            // A sender must have registered. Nothing checked this: an
                            // anonymous socket that never sent `register` could inject
                            // offers, answers, ICE candidates and arbitrary `relay` JSON at
                            // any registered peer. A relay with no identity at all on the
                            // sending side is not a relay, it is an open injection point,
                            // and the peer on the far end has no way to tell the difference.
                            let Some(sender_id) = peer_id.clone() else {
                                let _ = Self::reply(
                                    &out_tx,
                                    &SignalingMessage::Error {
                                        message: "register before sending offer, answer, \
                                                  ice_candidate or relay"
                                            .to_string(),
                                    },
                                );
                                Log::new(Some(&status_tx)).warn(format!(
                                    "WebRTC signaling refused {} from unregistered {}",
                                    kind, remote_addr
                                ));
                                continue;
                            };

                            // And `from` is this connection's registered id, not whatever
                            // the frame claimed. It was taken verbatim, so any registered
                            // peer could send an offer that both the recipient and the
                            // `webrtc_signaling_message_received` event attributed to
                            // somebody else — and the recipient would answer *that* peer,
                            // splicing a stranger into a session it is not part of.
                            //
                            // A mismatch is rewritten rather than refused: the field is
                            // redundant (the connection already identifies the sender) and
                            // clients that fill it in correctly are unaffected.
                            if claimed_from != sender_id {
                                Log::new(Some(&status_tx)).warn(format!(
                                    "WebRTC signaling rewrote forged 'from' on a {}: peer '{}' \
                                     claimed to be '{}'",
                                    kind, sender_id, claimed_from
                                ));
                            }
                            let from = sender_id;
                            let message = Self::with_sender(message, &from);

                            // Relay: a signaling server is a relay, and putting a model
                            // round-trip in front of every ICE candidate would break any
                            // real browser peer.
                            let delivered = match server_data.forward_message(&to, &message).await {
                                Ok(()) => true,
                                Err(e) => {
                                    warn!(
                                        "Failed to forward {} from {} to {}: {}",
                                        kind, from, to, e
                                    );
                                    let _ = Self::reply(
                                        &out_tx,
                                        &SignalingMessage::Error {
                                            message: format!(
                                                "Cannot deliver {} to {}: {}",
                                                kind, to, e
                                            ),
                                        },
                                    );
                                    false
                                }
                            };

                            Log::new(Some(&status_tx)).debug(format!(
                                "WebRTC signaling {} {} -> {} ({})",
                                kind,
                                from,
                                to,
                                if delivered {
                                    "delivered"
                                } else {
                                    "undeliverable"
                                }
                            ));

                            // Notify the LLM out of band so observation never delays relay.
                            let event = Event::new(
                                &WEBRTC_SIGNALING_MESSAGE_RECEIVED_EVENT,
                                serde_json::json!({
                                    "peer_id": from,
                                    "message_type": kind,
                                    "target_peer": to,
                                    "delivered": delivered,
                                }),
                            );
                            let llm = llm_client.clone();
                            let state = app_state.clone();
                            let proto = protocol.clone();
                            let status = status_tx.clone();
                            let observed = format!("{} {} -> {}", kind, from, to);
                            tokio::spawn(async move {
                                if let Err(e) =
                                    call_llm(&llm, &state, server_id, None, &event, proto.as_ref())
                                        .await
                                {
                                    // Deliberately silent on the wire, and that is the only
                                    // correct answer here: `webrtc_signaling_message_received`
                                    // is declared `.with_no_actions()` and fires *after* the
                                    // relay has already been decided and already reported to
                                    // the sender. The model cannot speak to the peer on the
                                    // success path either, so an error frame on this path
                                    // would announce a failure the peer's signaling did not
                                    // suffer and could abort a negotiation that succeeded.
                                    // The operator is who needs to know, so say it loudly
                                    // there.
                                    Log::new(Some(&status)).error(format!(
                                        "WebRTC signaling {}: LLM call failed ({}) - message \
                                         was already relayed, no frame sent",
                                        observed, e
                                    ));
                                }
                            });
                        }
                        other => {
                            debug!("Ignoring signaling message: {:?}", other);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    info!("Signaling connection closed by peer");
                    break;
                }
                Ok(_) => {
                    // Ignore binary, ping, pong messages
                }
                Err(e) => {
                    warn!("WebSocket error: {}", e);
                    break;
                }
            }
        }

        // Cleanup on disconnect
        if let Some(pid) = peer_id {
            server_data.unregister_peer(&pid).await;

            // Fire disconnected event
            let event = Event::new(
                &WEBRTC_SIGNALING_PEER_DISCONNECTED_EVENT,
                serde_json::json!({
                    "peer_id": pid,
                    "peer_count": server_data.peer_count().await,
                }),
            );

            if let Err(e) = call_llm(
                &llm_client,
                &app_state,
                server_id,
                connection_id,
                &event,
                protocol.as_ref(),
            )
            .await
            {
                // Silence is forced here rather than chosen: this fires because the peer's
                // socket has already gone, so there is nobody left to answer in any
                // vocabulary. `webrtc_signaling_peer_disconnected` is declared
                // `.with_no_actions()` for the same reason. Log it loudly and carry on with
                // the cleanup below - the connection must still be torn down.
                Log::new(Some(&status_tx)).error(format!(
                    "WebRTC signaling peer '{}': LLM call failed on disconnect ({}) - peer \
                     already gone, nothing sent",
                    pid, e
                ));
            }

            // Remove connection from server
            if let Some(conn_id) = connection_id {
                app_state
                    .remove_connection_from_server(server_id, conn_id)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
            }
        }

        // Dropping our sender lets the writer task finish once the registry copy is gone.
        drop(out_tx);
        let _ = writer.await;

        Ok(())
    }

    fn reply(out_tx: &mpsc::UnboundedSender<Message>, message: &SignalingMessage) -> Result<()> {
        out_tx.send(Message::Text(serde_json::to_string(message)?))?;
        Ok(())
    }

    /// Replace a relayed message's `from` with the sender's registered peer id.
    ///
    /// The wire field is whatever the sender typed; this connection's identity is
    /// what it registered. Delivering the former lets any peer forge an offer from
    /// any other, so the latter is what goes out and what the event reports.
    fn with_sender(message: SignalingMessage, sender: &str) -> SignalingMessage {
        match message {
            SignalingMessage::Offer { to, sdp, .. } => SignalingMessage::Offer {
                from: sender.to_string(),
                to,
                sdp,
            },
            SignalingMessage::Answer { to, sdp, .. } => SignalingMessage::Answer {
                from: sender.to_string(),
                to,
                sdp,
            },
            SignalingMessage::IceCandidate { to, candidate, .. } => {
                SignalingMessage::IceCandidate {
                    from: sender.to_string(),
                    to,
                    candidate,
                }
            }
            SignalingMessage::Relay { to, data, .. } => SignalingMessage::Relay {
                from: sender.to_string(),
                to,
                data,
            },
            other => other,
        }
    }

    /// Execute whatever the LLM returned for a signaling event.
    ///
    /// Returns true if the connection should be closed.
    fn apply_results(
        result: crate::llm::actions::executor::ExecutionResult,
        out_tx: &mpsc::UnboundedSender<Message>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> bool {
        let log = Log::new(Some(status_tx));
        for message in &result.messages {
            log.info(format!("{}", message));
        }

        let mut close = false;
        for protocol_result in result.protocol_results {
            match protocol_result {
                crate::llm::ActionResult::Output(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => {
                        let _ = out_tx.send(Message::Text(text));
                    }
                    Err(e) => {
                        error!("Signaling action produced non-UTF-8 output: {}", e);
                    }
                },
                crate::llm::ActionResult::CloseConnection => close = true,
                _ => {}
            }
        }
        close
    }
}
