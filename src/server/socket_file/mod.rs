//! Unix domain socket server implementation
//!
//! Platform: Unix/Linux only (uses Unix domain sockets)
#![cfg(unix)]

pub mod actions;

use anyhow::{Context, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info};

use super::connection::ConnectionId;
use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::SocketFileProtocol;
use crate::state::app_state::AppState;
use actions::{SOCKET_FILE_CONNECTION_OPENED_EVENT, SOCKET_FILE_DATA_RECEIVED_EVENT};

/// Connection state for LLM processing
#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Idle,
    Processing,
    Accumulating,
}

/// Per-connection data for LLM handling
struct ConnectionData {
    state: ConnectionState,
    queued_data: Vec<u8>,
    write_half: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
}

/// Describe a file type for the "refusing to unlink" error message.
fn describe_file_type(ft: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "symlink"
    } else if ft.is_fifo() {
        "FIFO"
    } else if ft.is_char_device() {
        "character device"
    } else if ft.is_block_device() {
        "block device"
    } else {
        "regular file"
    }
}

/// Unix domain socket server that listens for incoming connections
pub struct SocketFileServer;

impl SocketFileServer {
    /// Spawn the socket file server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        socket_path: PathBuf,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        send_first: bool,
        server_id: crate::state::ServerId,
    ) -> Result<PathBuf> {
        // Remove a stale socket file, but ONLY if the path really is a socket.
        //
        // `socket_path` comes from the LLM or an MCP caller, so an unconditional
        // `remove_file` here is an arbitrary-file delete: "./netget.sock" typo'd as
        // "./netget.rs", or a deliberately chosen "~/.ssh/id_ed25519", would be unlinked
        // before the bind. `symlink_metadata` deliberately does not follow symlinks, so a
        // symlink pointing at a regular file is refused rather than followed and deleted.
        match std::fs::symlink_metadata(&socket_path) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    anyhow::bail!(
                        "Refusing to remove {:?}: it exists but is not a Unix domain socket \
                         (it is a {}). Delete it yourself if that is really what you want, or \
                         pass a different socket_path.",
                        socket_path,
                        describe_file_type(&meta.file_type())
                    );
                }
                std::fs::remove_file(&socket_path).with_context(|| {
                    format!("Failed to remove existing socket file: {:?}", socket_path)
                })?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Nothing there - normal case.
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to stat {:?}", socket_path));
            }
        }

        // Create and bind Unix domain socket server
        let listener = tokio::net::UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind to socket path: {:?}", socket_path))?;

        // Owner-only (0600). `bind` creates the node with 0777 & ~umask, which on a default
        // umask of 022 is `srwxr-xr-x`: on both Linux and macOS the kernel checks write
        // permission on the socket node at connect(2), so every local user could speak to a
        // server the model was told to run for one process. The model or an MCP caller chooses
        // `socket_path`, and a predictable path under a world-writable directory is the classic
        // local-escalation shape, so the default has to be closed. There is a brief window
        // between bind and chmod; closing it entirely means a private directory, which is the
        // caller's choice of path, not ours.
        std::fs::set_permissions(
            &socket_path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .with_context(|| {
            format!(
                "Failed to restrict permissions on socket file {:?} to owner-only",
                socket_path
            )
        })?;

        Log::new(Some(&status_tx))
            .info(format!("Socket file server listening on {:?}", socket_path));

        let connections = Arc::new(Mutex::new(HashMap::new()));
        let protocol = Arc::new(SocketFileProtocol::new());

        let socket_path_clone = socket_path.clone();

        // Spawn accept loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        info!("Accepted socket file connection {}", connection_id);

                        // Split stream
                        let (read_half, write_half) = tokio::io::split(stream);
                        let write_half_arc = Arc::new(Mutex::new(write_half));

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        // Use a dummy SocketAddr since Unix sockets don't have IP addresses
                        let dummy_addr = "127.0.0.1:0".parse().unwrap();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr: dummy_addr,
                            local_addr: dummy_addr,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                                "state": "Idle",
                                "socket_path": socket_path_clone.to_string_lossy()
                            })),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        // Register the connection HERE, before either task is spawned.
                        //
                        // This used to be the first thing the banner task did, racing the
                        // reader task: handle_data_with_actions returns silently when the
                        // connection is not in the map, so a client that wrote immediately
                        // after connecting (`printf ping | nc -U ...`) had its first payload
                        // dropped with no response and no log line. Inserting synchronously
                        // in the accept loop closes the window.
                        connections.lock().await.insert(
                            connection_id,
                            ConnectionData {
                                state: ConnectionState::Idle,
                                queued_data: Vec::new(),
                                write_half: write_half_arc.clone(),
                            },
                        );

                        // Send the greeting banner, if this server was asked for one.
                        if send_first {
                            let llm_client_clone = llm_client.clone();
                            let app_state_clone = app_state.clone();
                            let status_tx_clone = status_tx.clone();
                            let connections_clone = connections.clone();
                            let write_half_for_conn = write_half_arc.clone();
                            let protocol_clone = protocol.clone();
                            tokio::spawn(async move {
                                Self::send_banner(
                                    connection_id,
                                    server_id,
                                    llm_client_clone,
                                    app_state_clone,
                                    status_tx_clone,
                                    connections_clone,
                                    write_half_for_conn,
                                    protocol_clone,
                                )
                                .await;
                            });
                        }

                        // Spawn reader task
                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let connections_clone = connections.clone();
                        let protocol_clone = protocol.clone();
                        tokio::spawn(async move {
                            let mut buffer = vec![0u8; 8192];
                            let mut read_half = read_half;

                            loop {
                                match read_half.read(&mut buffer).await {
                                    Ok(0) => {
                                        // Connection closed
                                        connections_clone.lock().await.remove(&connection_id);
                                        app_state_clone
                                            .close_connection_on_server(server_id, connection_id)
                                            .await;
                                        Log::new(Some(&status_tx_clone)).info(format!(
                                            "Socket file connection {connection_id} closed"
                                        ));
                                        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                                        break;
                                    }
                                    Ok(n) => {
                                        let data = Bytes::copy_from_slice(&buffer[..n]);

                                        // Feeds the dashboard rail's counters and refreshes
                                        // `last_activity`; this protocol never called it, so a
                                        // live connection read 0B/0B in the tree forever.
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

                                        // Data summary + full payload are FileOnly: the
                                        // socket_file_data_received event template renders the
                                        // equivalent lines to the TUI, so streaming the payload
                                        // here too would duplicate it and load the unbounded
                                        // status channel.
                                        let log = Log::new(Some(&status_tx_clone));
                                        if data.iter().all(|&b| {
                                            b.is_ascii_graphic() || b.is_ascii_whitespace()
                                        }) {
                                            let data_str = String::from_utf8_lossy(&data);
                                            let preview =
                                                crate::utils::truncate_for_log(&data_str, 100);
                                            log.debug(format!(
                                                "Socket file received {} bytes on {}: {}",
                                                n, connection_id, preview
                                            ));
                                            log.trace(format!(
                                                "Socket file data (text): {:?}",
                                                data_str
                                            ));
                                        } else {
                                            log.debug(format!(
                                                "Socket file received {} bytes on {} (binary data)",
                                                n, connection_id
                                            ));
                                            log.trace(format!(
                                                "Socket file data (hex): {}",
                                                hex::encode(&data)
                                            ));
                                        }

                                        // Handle data in separate task
                                        let llm_clone = llm_client_clone.clone();
                                        let state_clone = app_state_clone.clone();
                                        let status_clone = status_tx_clone.clone();
                                        let conns_clone = connections_clone.clone();
                                        let protocol_clone = protocol_clone.clone();
                                        tokio::spawn(async move {
                                            Self::handle_data_with_actions(
                                                connection_id,
                                                server_id,
                                                data,
                                                llm_clone,
                                                state_clone,
                                                status_clone,
                                                conns_clone,
                                                protocol_clone,
                                            )
                                            .await;
                                        });
                                    }
                                    Err(e) => {
                                        Log::new(Some(&status_tx_clone)).error(format!(
                                            "Read error on socket file connection {}: {}",
                                            connection_id, e
                                        ));
                                        connections_clone.lock().await.remove(&connection_id);
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Accept error on socket file: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(socket_path)
    }

    /// Send the greeting banner for a new connection (`send_first` servers only).
    ///
    /// The connection is already registered by the accept loop by the time this runs.
    #[allow(clippy::too_many_arguments)]
    async fn send_banner(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        write_half: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
        protocol: Arc<SocketFileProtocol>,
    ) {
        {
            let log = Log::new(Some(&status_tx));
            // Create connection opened event
            let event = Event::new(&SOCKET_FILE_CONNECTION_OPENED_EVENT, serde_json::json!({}));

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
                    debug!("LLM socket file banner response received");

                    // Display messages
                    for msg in execution_result.messages {
                        let _ = status_tx.send(msg);
                    }

                    // Handle protocol results (send banner)
                    let mut wrote_banner = false;
                    for protocol_result in execution_result.protocol_results {
                        match protocol_result {
                            ActionResult::Output(output_data) => {
                                let mut write = write_half.lock().await;
                                if let Err(e) = write.write_all(&output_data).await {
                                    log.error(format!("Failed to send socket file banner: {}", e));
                                } else {
                                    app_state
                                        .update_connection_stats(
                                            server_id,
                                            connection_id,
                                            None,
                                            Some(output_data.len() as u64),
                                            None,
                                            Some(1),
                                        )
                                        .await;
                                    // Sent-data summary + payload are FileOnly: the
                                    // send_socket_data action template already reports the
                                    // send to the TUI.
                                    if output_data
                                        .iter()
                                        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
                                    {
                                        let data_str = String::from_utf8_lossy(&output_data);
                                        let preview =
                                            crate::utils::truncate_for_log(&data_str, 100);
                                        log.debug(format!(
                                            "Socket file sent {} bytes to {}: {}",
                                            output_data.len(),
                                            connection_id,
                                            preview
                                        ));
                                        log.trace(format!(
                                            "Socket file sent (text): {:?}",
                                            data_str
                                        ));
                                    } else {
                                        log.debug(format!(
                                            "Socket file sent {} bytes to {} (binary data)",
                                            output_data.len(),
                                            connection_id
                                        ));
                                        log.trace(format!(
                                            "Socket file sent (hex): {}",
                                            hex::encode(&output_data)
                                        ));
                                    }
                                    log.debug(format!(
                                        "Sent banner to socket file connection {connection_id}"
                                    ));
                                    wrote_banner = true;
                                }
                            }
                            ActionResult::CloseConnection => {
                                connections.lock().await.remove(&connection_id);
                                log.info(format!(
                                    "Closed socket file connection {connection_id} after banner: decision=model_close"
                                ));
                            }
                            _ => {}
                        }
                    }

                    // Greeting with nothing is a legitimate answer, not a failure —
                    // keep it distinguishable in the log from the two below.
                    if !wrote_banner {
                        log.debug(format!(
                            "No banner bytes for socket file connection {connection_id}: decision=model_no_actions"
                        ));
                    }
                }
                Err(e) => {
                    let failure = crate::utils::WireFailure::classify(&e);
                    let class = if failure.is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    };
                    // The full error goes to the log and the status stream, where an
                    // operator looks. Nothing derived from it reaches the peer.
                    log.warn(format!(
                        "Socket file banner failed for {connection_id}: decision=fail_closed_llm_error class={class} error={e}"
                    ));

                    // A send_first server owes this peer a greeting and now has none.
                    // A raw byte stream has no error frame, so the only honest signal is
                    // FIN: half-close so the peer's next read returns EOF immediately
                    // instead of blocking until its own timeout. Same shape as `tcp`.
                    {
                        let mut write = write_half.lock().await;
                        let _ = write.shutdown().await;
                    }
                    connections.lock().await.remove(&connection_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    log.info(format!(
                        "Closed socket file connection {connection_id} after banner LLM error"
                    ));
                }
            }
        }
    }

    /// Handle data received on a connection with LLM actions
    async fn handle_data_with_actions(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        data: Bytes,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        protocol: Arc<SocketFileProtocol>,
    ) {
        let log = Log::new(Some(&status_tx));

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
                    "socket-file connection {} is no longer registered; dropping {} received bytes",
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
            log.debug(format!(
                "Queued {} bytes for socket file connection {}",
                data.len(),
                connection_id
            ));
            return;
        }

        // Merge any queued data with new data.
        //
        // Not `unwrap()`: the map lock was released after the state read above, so the
        // connection can legitimately be gone by now - the peer reset, or another task ran
        // `close_this_connection`. `unwrap()` panicked inside a `tokio::spawn`, which swallows
        // the panic: the bytes vanished, the log said nothing, and the server stayed Running.
        let mut all_data = {
            let mut conns = connections.lock().await;
            let Some(conn_data) = conns.get_mut(&connection_id) else {
                debug!(
                    "socket-file connection {} went away before its {} received bytes could be \
                     processed",
                    connection_id,
                    data.len()
                );
                return;
            };
            conn_data.state = ConnectionState::Processing;
            let mut merged = conn_data.queued_data.clone();
            merged.extend_from_slice(&data);
            conn_data.queued_data.clear();
            Bytes::from(merged)
        };

        loop {
            // Get write_half for context
            let write_half = {
                let conns = connections.lock().await;
                conns.get(&connection_id).map(|c| c.write_half.clone())
            };

            let Some(write_half) = write_half else {
                debug!(
                    "socket-file connection {} went away before its response could be written",
                    connection_id
                );
                return;
            };

            // Format data for event parameter. Printable ASCII is passed through as text,
            // anything else is hex-encoded. `encoding` tells the LLM which one it got, so it
            // can echo the payload back with a matching `encoding` on send_socket_data.
            let (data_str, data_encoding) = if all_data
                .iter()
                .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
            {
                (String::from_utf8_lossy(&all_data).to_string(), "utf8")
            } else {
                (hex::encode(&all_data), "hex")
            };

            // Create data received event
            let event = Event::new(
                &SOCKET_FILE_DATA_RECEIVED_EVENT,
                serde_json::json!({
                    "data": data_str,
                    "encoding": data_encoding
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
                    debug!("LLM socket file response received");

                    // Display messages
                    for msg in execution_result.messages {
                        let _ = status_tx.send(msg);
                    }

                    // Handle protocol results
                    let mut should_close = false;
                    let mut should_wait = false;
                    let mut wrote_output = false;

                    for protocol_result in execution_result.protocol_results {
                        match protocol_result {
                            ActionResult::Output(output_data) => {
                                let mut write = write_half.lock().await;
                                if let Err(e) = write.write_all(&output_data).await {
                                    log.error(format!(
                                        "Failed to send socket file response: {}",
                                        e
                                    ));
                                } else {
                                    app_state
                                        .update_connection_stats(
                                            server_id,
                                            connection_id,
                                            None,
                                            Some(output_data.len() as u64),
                                            None,
                                            Some(1),
                                        )
                                        .await;
                                    // Sent-data summary + payload are FileOnly: the
                                    // send_socket_data action template already reports the
                                    // send to the TUI.
                                    if output_data
                                        .iter()
                                        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
                                    {
                                        let data_str = String::from_utf8_lossy(&output_data);
                                        let preview =
                                            crate::utils::truncate_for_log(&data_str, 100);
                                        log.debug(format!(
                                            "Socket file sent {} bytes to {}: {}",
                                            output_data.len(),
                                            connection_id,
                                            preview
                                        ));
                                        log.trace(format!(
                                            "Socket file sent (text): {:?}",
                                            data_str
                                        ));
                                    } else {
                                        log.debug(format!(
                                            "Socket file sent {} bytes to {} (binary data)",
                                            output_data.len(),
                                            connection_id
                                        ));
                                        log.trace(format!(
                                            "Socket file sent (hex): {}",
                                            hex::encode(&output_data)
                                        ));
                                    }
                                    log.debug(format!(
                                        "Sent {} bytes to socket file connection {}",
                                        output_data.len(),
                                        connection_id
                                    ));
                                    wrote_output = true;
                                }
                            }
                            ActionResult::CloseConnection => {
                                should_close = true;
                            }
                            ActionResult::WaitForMore => {
                                should_wait = true;
                            }
                            _ => {}
                        }
                    }

                    // Handle wait_for_more
                    if should_wait {
                        connections
                            .lock()
                            .await
                            .entry(connection_id)
                            .and_modify(|conn| conn.state = ConnectionState::Accumulating);
                        log.debug(format!(
                            "Waiting for more data from socket file connection {connection_id}"
                        ));
                        return;
                    }

                    // Handle close_connection
                    if should_close {
                        connections.lock().await.remove(&connection_id);
                        // The model answered by hanging up — distinct in the log from
                        // "answered nothing" and from an LLM failure.
                        log.info(format!(
                            "Closed socket file connection {connection_id}: decision=model_close"
                        ));
                        return;
                    }

                    // The model answered, but with no bytes and no lifecycle action. On a
                    // raw byte stream that is a legitimate answer ("say nothing, keep
                    // listening"), so the connection stays open — but it must not be
                    // confused with the backend having failed.
                    if !wrote_output {
                        log.debug(format!(
                            "No response bytes for socket file connection {connection_id}: decision=model_no_actions"
                        ));
                    }

                    // Check for queued data
                    let has_queued = {
                        let conns = connections.lock().await;
                        conns
                            .get(&connection_id)
                            .map(|c| !c.queued_data.is_empty())
                            .unwrap_or(false)
                    };

                    if has_queued {
                        // Take the queue and make it the next iteration's payload.
                        // Leaving it in place re-sent the SAME bytes to the model on
                        // every pass and never emptied the queue: one response per
                        // iteration, forever, for a single line of input.
                        let queued = {
                            let mut conns = connections.lock().await;
                            match conns.get_mut(&connection_id) {
                                Some(conn) => std::mem::take(&mut conn.queued_data),
                                None => return,
                            }
                        };
                        if queued.is_empty() {
                            connections
                                .lock()
                                .await
                                .entry(connection_id)
                                .and_modify(|conn| conn.state = ConnectionState::Idle);
                            return;
                        }
                        all_data = Bytes::from(queued);
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
                    let failure = crate::utils::WireFailure::classify(&e);
                    let class = if failure.is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    };
                    // Full error to the log/status stream only — never to the peer.
                    log.warn(format!(
                        "LLM error for socket file data on {connection_id}: decision=fail_closed_llm_error class={class} error={e}"
                    ));
                    if failure.is_overloaded() {
                        log.warn(format!(
                            "Socket file connection {connection_id} closed: LLM capacity exhausted"
                        ));
                    }

                    // Say *something* on the wire. A raw byte stream has no error frame,
                    // so the only honest signal is FIN: half-close the connection so the
                    // peer's next read returns EOF immediately. This path used to reset
                    // to Idle and write nothing, leaving the peer blocked until its own
                    // timeout with no indication anything had gone wrong.
                    {
                        let mut write = write_half.lock().await;
                        let _ = write.shutdown().await;
                    }
                    connections.lock().await.remove(&connection_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    log.info(format!(
                        "Closed socket file connection {connection_id} after LLM error"
                    ));
                    return;
                }
            }
        }
    }
}
