//! Bitcoin P2P protocol server implementation
pub mod actions;

use anyhow::{Context, Result};
use bitcoin::consensus::Decodable;
use bitcoin::p2p::message::{NetworkMessage, RawNetworkMessage};
use bitcoin::p2p::Magic;
use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::debug;

use super::connection::ConnectionId;
use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::BitcoinProtocol;
use crate::state::app_state::AppState;
use actions::{BITCOIN_CONNECTION_OPENED_EVENT, BITCOIN_MESSAGE_RECEIVED_EVENT};

/// Connection state for LLM processing
#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Idle,
    Processing,
    Accumulating,
}

/// Per-connection data for Bitcoin protocol
struct ConnectionData {
    state: ConnectionState,
    queued_data: Vec<u8>,
    write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    #[allow(dead_code)]
    handshake_complete: bool,
}

/// Bitcoin P2P protocol server
pub struct BitcoinServer;

impl BitcoinServer {
    /// Spawn the Bitcoin P2P server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        network: String,
    ) -> Result<SocketAddr> {
        // Parse network magic
        let magic = match network.to_lowercase().as_str() {
            "mainnet" | "main" => Magic::BITCOIN,
            "testnet" | "test" => Magic::TESTNET3,
            "signet" => Magic::SIGNET,
            "regtest" => Magic::REGTEST,
            _ => {
                Log::new(Some(&status_tx)).info(format!(
                    "Unknown network '{}', defaulting to mainnet",
                    network
                ));
                Magic::BITCOIN
            }
        };

        // Create and bind TCP server
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "Bitcoin P2P server listening on {} (network: {:?})",
            local_addr, magic
        ));

        let connections = Arc::new(Mutex::new(HashMap::new()));
        let protocol = Arc::new(BitcoinProtocol::new());

        // Spawn accept loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        Log::new(Some(&status_tx)).info(format!(
                            "Accepted Bitcoin P2P connection {} from {}",
                            connection_id, remote_addr
                        ));

                        // Split stream
                        let (read_half, write_half) = tokio::io::split(stream);
                        let write_half_arc = Arc::new(Mutex::new(write_half));

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: local_addr_conn,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        // Peer messaging: the dashboard's "message this peer" / "disconnect
                        // this peer" inject actions into THIS connection through the same
                        // executor the LLM path uses. Registered before the opened event so a
                        // manual `*` rule parking that event leaves the operator able to reach
                        // the connection while it waits.
                        let peer_rx = crate::server::peer_support::register_peer_channel(
                            &app_state,
                            server_id,
                            connection_id.as_u32(),
                        )
                        .await;
                        crate::server::peer_support::spawn_peer_command_task(
                            peer_rx,
                            protocol.clone(),
                            app_state.clone(),
                            server_id,
                            connection_id.as_u32(),
                            write_half_arc.clone(),
                            status_tx.clone(),
                        );

                        // Register the connection *before* either task is spawned.
                        //
                        // This used to happen inside `handle_connection_opened`, which runs in
                        // its own task alongside the reader task below — so a peer that sent
                        // its first message promptly raced the insert. `handle_data_with_actions`
                        // finds no entry, returns at its "connection not found" guard, and the
                        // bytes are dropped with no event, no queue and no log line: the read
                        // loop reports "received N bytes" and nothing further ever happens.
                        //
                        // A Bitcoin peer sends `version` immediately on connect, so this was hit
                        // constantly — three E2E tests waited out their 120s read timeouts, and
                        // flakily, since it is a race.
                        connections.lock().await.insert(
                            connection_id,
                            ConnectionData {
                                state: ConnectionState::Idle,
                                queued_data: Vec::new(),
                                write_half: write_half_arc.clone(),
                                handshake_complete: false,
                            },
                        );

                        // Handle connection opened event
                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let connections_clone = connections.clone();
                        let write_half_for_conn = write_half_arc.clone();
                        let protocol_clone = protocol.clone();
                        let magic_clone = magic;
                        tokio::spawn(async move {
                            Self::handle_connection_opened(
                                connection_id,
                                server_id,
                                llm_client_clone,
                                app_state_clone,
                                status_tx_clone,
                                connections_clone,
                                write_half_for_conn,
                                protocol_clone,
                                magic_clone,
                            )
                            .await;
                        });

                        // Spawn reader task
                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let connections_clone = connections.clone();
                        let protocol_clone = protocol.clone();
                        let magic_clone = magic;
                        tokio::spawn(async move {
                            let mut buffer = vec![0u8; 8192];
                            let mut read_half = read_half;

                            loop {
                                match read_half.read(&mut buffer).await {
                                    Ok(0) => {
                                        // Connection closed
                                        Self::teardown_connection(
                                            &connections_clone,
                                            &app_state_clone,
                                            server_id,
                                            connection_id,
                                        )
                                        .await;
                                        Log::new(Some(&status_tx_clone)).info(format!(
                                            "Bitcoin connection {} closed",
                                            connection_id
                                        ));
                                        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                                        break;
                                    }
                                    Ok(n) => {
                                        let data = &buffer[..n];
                                        app_state_clone
                                            .update_connection_stats(
                                                server_id,
                                                connection_id,
                                                Some(n as u64),
                                                None,
                                                Some(1),
                                                None,
                                            )
                                            .await;

                                        // Byte-count summary and full hex payload are
                                        // FileOnly: the bitcoin_message_received event
                                        // template reports the message to the TUI, so
                                        // streaming raw bytes here would duplicate it and
                                        // load the unbounded status channel.
                                        let log = Log::new(Some(&status_tx_clone));
                                        log.debug(format!(
                                            "Bitcoin P2P received {} bytes on {}",
                                            n, connection_id
                                        ));
                                        log.trace(format!(
                                            "Bitcoin P2P data (hex): {}",
                                            hex::encode(data)
                                        ));

                                        // Handle data in separate task
                                        let llm_clone = llm_client_clone.clone();
                                        let state_clone = app_state_clone.clone();
                                        let status_clone = status_tx_clone.clone();
                                        let conns_clone = connections_clone.clone();
                                        let protocol_clone = protocol_clone.clone();
                                        let data_vec = data.to_vec();
                                        tokio::spawn(async move {
                                            Self::handle_data_with_actions(
                                                connection_id,
                                                server_id,
                                                data_vec,
                                                llm_clone,
                                                state_clone,
                                                status_clone,
                                                conns_clone,
                                                protocol_clone,
                                                magic_clone,
                                            )
                                            .await;
                                        });
                                    }
                                    Err(e) => {
                                        Log::new(Some(&status_tx_clone)).error(format!(
                                            "Read error on Bitcoin connection {}: {}",
                                            connection_id, e
                                        ));
                                        Self::teardown_connection(
                                            &connections_clone,
                                            &app_state_clone,
                                            server_id,
                                            connection_id,
                                        )
                                        .await;
                                        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Accept error on Bitcoin server: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Handle new connection opened event
    async fn handle_connection_opened(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        protocol: Arc<BitcoinProtocol>,
        magic: Magic,
    ) {
        // The connection is registered by the accept loop before this task and the reader task
        // are spawned — inserting it here raced the reader and silently dropped whatever the
        // peer sent first. Deliberately not re-inserted: doing so would reset the state machine
        // and discard any bytes the reader has already queued. (`connections` is still used
        // below, to remove the entry when the model closes the connection.)

        // Create connection opened event
        let event = Event::new(&BITCOIN_CONNECTION_OPENED_EVENT, serde_json::json!({}));

        // Call LLM to decide what to do (wait or send version message)
        match call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                debug!("LLM Bitcoin connection opened response received");

                // Display messages
                for msg in execution_result.messages {
                    let _ = status_tx.send(msg);
                }

                // The model answering with no action is a real answer here - most of the
                // Bitcoin handshake is peer-driven and "say nothing, wait for their version"
                // is correct - so it stays silent. It is still tagged, because in the log it
                // must not look like the LLM-error path below.
                if execution_result.protocol_results.is_empty() {
                    Log::new(Some(&status_tx)).debug(format!(
                        "Bitcoin connection {} opened decision=model_no_action (no message sent)",
                        connection_id
                    ));
                }

                // Handle protocol results
                for protocol_result in execution_result.protocol_results {
                    match protocol_result {
                        ActionResult::Output(output_data) => {
                            if let Err(e) = Self::send_bitcoin_message(
                                &write_half,
                                &output_data,
                                connection_id,
                                server_id,
                                &app_state,
                                &status_tx,
                                magic,
                            )
                            .await
                            {
                                Log::new(Some(&status_tx))
                                    .error(format!("Failed to send Bitcoin message: {}", e));
                            }
                        }
                        ActionResult::CloseConnection => {
                            Self::close_from_model(
                                &write_half,
                                &connections,
                                &app_state,
                                server_id,
                                connection_id,
                            )
                            .await;
                            Log::new(Some(&status_tx)).info(format!(
                                "Closed Bitcoin connection {} after connection opened \
                                 decision=model_close",
                                connection_id
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                Self::fail_closed(
                    &write_half,
                    &connections,
                    &app_state,
                    &status_tx,
                    server_id,
                    connection_id,
                    &e,
                    "connection opened",
                )
                .await;
            }
        }
    }

    /// The LLM call itself failed. Bitcoin P2P has no error frame a modern peer will act on -
    /// BIP61 `reject` was removed from Bitcoin Core in 0.20 and is ignored by everything on
    /// the network today - so the only signal that reaches the peer is a disconnect, which is
    /// exactly what a real node does when it cannot serve a connection. Half-close the write
    /// side so the peer reads EOF at once and moves on to another peer, instead of blocking
    /// until its own timeout.
    ///
    /// Nothing derived from the error is written to the socket: FIN carries no text, and the
    /// error goes to the log and the status stream, where an operator looks. The overload and
    /// non-overload categories cannot be distinguished on the wire because Bitcoin has only
    /// this one shape, so they are distinguished in the log instead - `decision=` is stable so
    /// an operator can grep for it.
    #[allow(clippy::too_many_arguments)]
    async fn fail_closed(
        write_half: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        connections: &Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        err: &anyhow::Error,
        context: &str,
    ) {
        let decision = match crate::utils::WireFailure::classify(err) {
            crate::utils::WireFailure::Overloaded => "fail_closed_llm_overloaded",
            crate::utils::WireFailure::Unavailable => "fail_closed_llm_error",
        };
        let log = Log::new(Some(status_tx));
        // The error text belongs here, never on the wire.
        log.warn(format!(
            "LLM error on Bitcoin {} for connection {} decision={}: {}",
            context, connection_id, decision, err
        ));
        Self::close_from_model(write_half, connections, app_state, server_id, connection_id).await;
        log.info(format!(
            "Closed Bitcoin connection {} decision={}",
            connection_id, decision
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Handle data received on a connection with LLM actions
    async fn handle_data_with_actions(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        data: Vec<u8>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        protocol: Arc<BitcoinProtocol>,
        magic: Magic,
    ) {
        // Check connection state
        let current_state = {
            let conns = connections.lock().await;
            if let Some(conn_data) = conns.get(&connection_id) {
                conn_data.state.clone()
            } else {
                // Never silent. A miss here means the connection was torn down between the
                // read and this lookup (peer reset, or close_this_connection on another
                // task) - legitimate, but indistinguishable from a registration race, which
                // is exactly what made the bitcoin accept-order bug so hard to find: the
                // read loop logged "received N bytes" and then nothing at all.
                debug!(
                    "Bitcoin connection {} is no longer registered; dropping {} received bytes",
                    connection_id,
                    data.len()
                );
                return;
            }
        };

        // If processing, queue the data
        if current_state == ConnectionState::Processing {
            connections
                .lock()
                .await
                .entry(connection_id)
                .and_modify(|conn| {
                    conn.queued_data.extend_from_slice(&data);
                });
            debug!(
                "Queued {} bytes for Bitcoin connection {}",
                data.len(),
                connection_id
            );
            let _ = status_tx.send(format!(
                "⏸ Queued {} bytes for {}",
                data.len(),
                connection_id
            ));
            return;
        }

        // Merge any queued data with new data.
        //
        // The lock was released after the state check above, so the reader task's EOF branch
        // or a close_this_connection may have removed this entry in between - `.unwrap()` here
        // panicked the task whenever a peer disconnected while a datagram was in flight.
        let Some(mut buffer) = ({
            let mut conns = connections.lock().await;
            conns.get_mut(&connection_id).map(|conn_data| {
                conn_data.state = ConnectionState::Processing;
                let mut merged = std::mem::take(&mut conn_data.queued_data);
                merged.extend_from_slice(&data);
                merged
            })
        }) else {
            debug!(
                "Bitcoin connection {} went away before its data could be processed",
                connection_id
            );
            return;
        };

        loop {
            // Try to parse Bitcoin message
            let parsed_message = Self::try_parse_bitcoin_message(&buffer, magic);

            match parsed_message {
                Ok(Some((message, remaining))) => {
                    // Consume what we just parsed. Leaving `buffer` untouched (the old
                    // behaviour, which dropped `remaining`) meant that whenever a peer
                    // pipelined a second message the loop re-parsed the *first* one and
                    // called the LLM on it again, without end.
                    buffer = remaining;

                    // Successfully parsed a message
                    let payload = message.payload();
                    let message_type = Self::get_message_type_name(payload);
                    Log::new(Some(&status_tx)).info(format!(
                        "Received Bitcoin message: {} from {}",
                        message_type, connection_id
                    ));

                    // Update connection info with message type
                    app_state
                        .update_bitcoin_connection_info(
                            server_id,
                            connection_id,
                            message_type.clone(),
                        )
                        .await;

                    // Get write_half for context
                    let write_half = {
                        let conns = connections.lock().await;
                        conns.get(&connection_id).map(|c| c.write_half.clone())
                    };

                    let Some(write_half) = write_half else {
                        return; // Connection not found
                    };

                    // Serialize message for LLM
                    let message_json = Self::serialize_message_to_json(payload);

                    // Create data received event
                    let event = Event::new(
                        &BITCOIN_MESSAGE_RECEIVED_EVENT,
                        serde_json::json!({
                            "message_type": message_type.clone(),
                            "message": message_json,
                        }),
                    );

                    // Call LLM
                    match call_llm(
                        &llm_client,
                        &app_state,
                        server_id,
                        Some(connection_id),
                        &event,
                        protocol.as_ref(),
                    )
                    .await
                    {
                        Ok(execution_result) => {
                            debug!("LLM Bitcoin response received");

                            // Display messages
                            for msg in execution_result.messages {
                                let _ = status_tx.send(msg);
                            }

                            // As on the opened event: no action is a legitimate answer for
                            // most message types (an `addr` or an `inv` needs no reply), so
                            // stay silent - but tag it, so the log never confuses it with the
                            // LLM-error path below.
                            if execution_result.protocol_results.is_empty() {
                                Log::new(Some(&status_tx)).debug(format!(
                                    "Bitcoin {} on {} decision=model_no_action (no reply sent)",
                                    message_type, connection_id
                                ));
                            }

                            // Handle protocol results
                            let mut should_close = false;

                            for protocol_result in execution_result.protocol_results {
                                match protocol_result {
                                    ActionResult::Output(output_data) => {
                                        if let Err(e) = Self::send_bitcoin_message(
                                            &write_half,
                                            &output_data,
                                            connection_id,
                                            server_id,
                                            &app_state,
                                            &status_tx,
                                            magic,
                                        )
                                        .await
                                        {
                                            Log::new(Some(&status_tx)).error(format!(
                                                "Failed to send Bitcoin response: {}",
                                                e
                                            ));
                                        }
                                    }
                                    ActionResult::CloseConnection => {
                                        should_close = true;
                                    }
                                    _ => {}
                                }
                            }

                            // Handle close_connection
                            if should_close {
                                Self::close_from_model(
                                    &write_half,
                                    &connections,
                                    &app_state,
                                    server_id,
                                    connection_id,
                                )
                                .await;
                                let _ = status_tx.send("__UPDATE_UI__".to_string());
                                Log::new(Some(&status_tx)).info(format!(
                                    "Closed Bitcoin connection {} decision=model_close",
                                    connection_id
                                ));
                                return;
                            }

                            // Check for queued data
                            let has_queued = {
                                let conns = connections.lock().await;
                                conns
                                    .get(&connection_id)
                                    .map(|c| !c.queued_data.is_empty())
                                    .unwrap_or(false)
                            };

                            if has_queued || !buffer.is_empty() {
                                Log::new(Some(&status_tx)).debug(format!(
                                    "Processing queued data for Bitcoin connection {}",
                                    connection_id
                                ));
                                // Fold anything that arrived during the LLM call onto the
                                // bytes we have not consumed yet, then go round again.
                                {
                                    let mut conns = connections.lock().await;
                                    if let Some(conn) = conns.get_mut(&connection_id) {
                                        let queued = std::mem::take(&mut conn.queued_data);
                                        buffer.extend_from_slice(&queued);
                                    }
                                }
                            } else {
                                // Go to Idle state
                                connections
                                    .lock()
                                    .await
                                    .entry(connection_id)
                                    .and_modify(|conn| conn.state = ConnectionState::Idle);
                                return;
                            }
                        }
                        Err(e) => {
                            // This used to reset the state to Idle and write nothing, leaving
                            // a peer that had just sent us a `version` or a `ping` blocked
                            // until its own timeout with no indication anything went wrong.
                            Self::fail_closed(
                                &write_half,
                                &connections,
                                &app_state,
                                &status_tx,
                                server_id,
                                connection_id,
                                &e,
                                &format!("{} message", message_type),
                            )
                            .await;
                            return;
                        }
                    }
                }
                Ok(None) => {
                    // Need more data to complete message.
                    //
                    // try_parse_bitcoin_message only reports "incomplete" for a header that
                    // validates, so the outstanding bytes are bounded by MAX_MESSAGE_BYTES and
                    // this cannot be used to grow the buffer without limit.
                    Log::new(Some(&status_tx)).debug(format!(
                        "Incomplete Bitcoin message ({} bytes buffered), waiting for more on {}",
                        buffer.len(),
                        connection_id
                    ));
                    connections
                        .lock()
                        .await
                        .entry(connection_id)
                        .and_modify(|conn| {
                            conn.state = ConnectionState::Accumulating;
                            let mut pending = buffer;
                            // Anything that landed during the LLM call goes after it.
                            let queued = std::mem::take(&mut conn.queued_data);
                            pending.extend_from_slice(&queued);
                            conn.queued_data = pending;
                        });
                    return;
                }
                Err(e) => {
                    // Parse error
                    Log::new(Some(&status_tx)).error(format!(
                        "Failed to parse Bitcoin message on {}: {}",
                        connection_id, e
                    ));
                    connections
                        .lock()
                        .await
                        .entry(connection_id)
                        .and_modify(|conn| conn.state = ConnectionState::Idle);
                    return;
                }
            }
        }
    }

    /// Try to parse one Bitcoin P2P message off the front of `data`.
    ///
    /// Returns `Ok(None)` **only** when the bytes so far are a valid prefix of a message whose
    /// declared length has not arrived yet. Everything else - bad magic, an absurd length, a
    /// body that will not decode - is `Err`, so the caller drops the connection instead of
    /// buffering forever. The previous version mapped every decode failure to `Ok(None)`,
    /// which turned a stream of garbage into unbounded memory growth: the caller kept the
    /// whole thing as "incomplete" and re-parsed it on every read.
    fn try_parse_bitcoin_message(
        data: &[u8],
        magic: Magic,
    ) -> Result<Option<(RawNetworkMessage, Vec<u8>)>> {
        // magic(4) | command(12) | length(4) | checksum(4)
        const HEADER_LEN: usize = 24;
        // Bitcoin Core's own cap on a P2P message body.
        const MAX_MESSAGE_BYTES: usize = 4_000_000;

        if data.len() < HEADER_LEN {
            return Ok(None);
        }

        // Validate the header before trusting the length field.
        let got_magic = Magic::from_bytes(
            data[0..4]
                .try_into()
                .expect("4-byte slice is always convertible"),
        );
        if got_magic != magic {
            return Err(anyhow::anyhow!(
                "Magic bytes mismatch: expected {:?}, got {:?}",
                magic,
                got_magic
            ));
        }

        let payload_len = u32::from_le_bytes(
            data[16..20]
                .try_into()
                .expect("4-byte slice is always convertible"),
        ) as usize;
        if payload_len > MAX_MESSAGE_BYTES {
            return Err(anyhow::anyhow!(
                "Bitcoin message declares {} byte payload, over the {} byte limit",
                payload_len,
                MAX_MESSAGE_BYTES
            ));
        }

        let total = HEADER_LEN + payload_len;
        if data.len() < total {
            // Genuinely incomplete, and bounded: at most MAX_MESSAGE_BYTES outstanding.
            return Ok(None);
        }

        let mut cursor = Cursor::new(&data[..total]);
        let message = RawNetworkMessage::consensus_decode(&mut cursor)
            .map_err(|e| anyhow::anyhow!("Malformed Bitcoin message: {}", e))?;

        Ok(Some((message, data[total..].to_vec())))
    }

    /// Get message type name from NetworkMessage
    fn get_message_type_name(payload: &NetworkMessage) -> String {
        match payload {
            NetworkMessage::Version(_) => "version".to_string(),
            NetworkMessage::Verack => "verack".to_string(),
            NetworkMessage::Addr(_) => "addr".to_string(),
            NetworkMessage::Inv(_) => "inv".to_string(),
            NetworkMessage::GetData(_) => "getdata".to_string(),
            NetworkMessage::NotFound(_) => "notfound".to_string(),
            NetworkMessage::GetBlocks(_) => "getblocks".to_string(),
            NetworkMessage::GetHeaders(_) => "getheaders".to_string(),
            NetworkMessage::MemPool => "mempool".to_string(),
            NetworkMessage::Tx(_) => "tx".to_string(),
            NetworkMessage::Block(_) => "block".to_string(),
            NetworkMessage::Headers(_) => "headers".to_string(),
            NetworkMessage::SendHeaders => "sendheaders".to_string(),
            NetworkMessage::GetAddr => "getaddr".to_string(),
            NetworkMessage::Ping(_) => "ping".to_string(),
            NetworkMessage::Pong(_) => "pong".to_string(),
            NetworkMessage::MerkleBlock(_) => "merkleblock".to_string(),
            NetworkMessage::FilterLoad(_) => "filterload".to_string(),
            NetworkMessage::FilterAdd(_) => "filteradd".to_string(),
            NetworkMessage::FilterClear => "filterclear".to_string(),
            NetworkMessage::GetCFilters(_) => "getcfilters".to_string(),
            NetworkMessage::CFilter(_) => "cfilter".to_string(),
            NetworkMessage::GetCFHeaders(_) => "getcfheaders".to_string(),
            NetworkMessage::CFHeaders(_) => "cfheaders".to_string(),
            NetworkMessage::GetCFCheckpt(_) => "getcfcheckpt".to_string(),
            NetworkMessage::CFCheckpt(_) => "cfcheckpt".to_string(),
            NetworkMessage::SendCmpct(_) => "sendcmpct".to_string(),
            NetworkMessage::CmpctBlock(_) => "cmpctblock".to_string(),
            NetworkMessage::GetBlockTxn(_) => "getblocktxn".to_string(),
            NetworkMessage::BlockTxn(_) => "blocktxn".to_string(),
            NetworkMessage::Alert(_) => "alert".to_string(),
            NetworkMessage::Reject(_) => "reject".to_string(),
            NetworkMessage::FeeFilter(_) => "feefilter".to_string(),
            NetworkMessage::WtxidRelay => "wtxidrelay".to_string(),
            NetworkMessage::AddrV2(_) => "addrv2".to_string(),
            NetworkMessage::SendAddrV2 => "sendaddrv2".to_string(),
            NetworkMessage::Unknown { command, .. } => format!("unknown({})", command),
        }
    }

    /// Serialize NetworkMessage to JSON for LLM
    fn serialize_message_to_json(payload: &NetworkMessage) -> serde_json::Value {
        match payload {
            NetworkMessage::Version(v) => serde_json::json!({
                "version": v.version,
                "services": v.services.to_u64(),
                "timestamp": v.timestamp,
                "receiver": v.receiver.socket_addr().ok().map(|a| a.to_string()),
                "sender": v.sender.socket_addr().ok().map(|a| a.to_string()),
                "nonce": v.nonce,
                "user_agent": v.user_agent,
                "start_height": v.start_height,
                "relay": v.relay,
            }),
            NetworkMessage::Ping(nonce) => serde_json::json!({ "nonce": nonce }),
            NetworkMessage::Pong(nonce) => serde_json::json!({ "nonce": nonce }),
            NetworkMessage::Addr(addrs) => {
                let addr_strings: Vec<Option<String>> = addrs
                    .iter()
                    .map(|(_, addr)| addr.socket_addr().ok().map(|a| a.to_string()))
                    .collect();
                serde_json::json!({ "count": addrs.len(), "addresses": addr_strings })
            }
            NetworkMessage::GetAddr => serde_json::json!({}),
            NetworkMessage::Verack => serde_json::json!({}),
            NetworkMessage::Inv(inv) => {
                serde_json::json!({ "count": inv.len(), "inventory": inv.iter().map(|i| format!("{:?}", i)).collect::<Vec<_>>() })
            }
            // For other message types, provide basic info
            _ => serde_json::json!({ "type": Self::get_message_type_name(payload) }),
        }
    }

    /// Forget a connection on every exit path of its reader task (EOF, read error): drop the
    /// state-machine entry, the dashboard's peer handle and the tracked connection.
    async fn teardown_connection(
        connections: &Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        app_state: &Arc<AppState>,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
    ) {
        connections.lock().await.remove(&connection_id);
        app_state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        app_state
            .close_connection_on_server(server_id, connection_id)
            .await;
    }

    /// `close_this_connection` from the model. Half-closes the write side so the peer reads
    /// EOF - previously this only dropped the map entry and the socket stayed open until the
    /// peer hung up - then runs the same teardown as the reader's exit paths. The reader task
    /// sees EOF once the peer closes and tears down again; every step is idempotent.
    async fn close_from_model(
        write_half: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        connections: &Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        app_state: &Arc<AppState>,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
    ) {
        {
            let mut write = write_half.lock().await;
            let _ = write.shutdown().await;
        }
        Self::teardown_connection(connections, app_state, server_id, connection_id).await;
    }

    /// Send a Bitcoin message (raw bytes that will be wrapped in Bitcoin message format)
    #[allow(clippy::too_many_arguments)]
    async fn send_bitcoin_message(
        write_half: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        data: &[u8],
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        _magic: Magic,
    ) -> Result<()> {
        {
            let mut write = write_half.lock().await;
            write
                .write_all(data)
                .await
                .context("Failed to write Bitcoin message")?;
            write
                .flush()
                .await
                .context("Failed to flush Bitcoin message")?;
        }
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(data.len() as u64),
                None,
                Some(1),
            )
            .await;

        // Byte-count summary and full hex payload are FileOnly; the one lifecycle
        // line to the TUI is the INFO below.
        let log = Log::new(Some(status_tx));
        log.debug(format!(
            "Bitcoin P2P sent {} bytes to {}",
            data.len(),
            connection_id
        ));
        log.trace(format!("Bitcoin P2P sent (hex): {}", hex::encode(data)));
        log.info(format!("Sent Bitcoin message to {}", connection_id));

        Ok(())
    }
}
