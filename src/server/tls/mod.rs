//! TLS server implementation
//!
//! Provides a generic TLS transport layer that allows the LLM to implement
//! custom application protocols on top of encrypted connections.

pub mod actions;

use anyhow::{Context, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};

use super::connection::ConnectionId;
use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::TlsProtocol;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use actions::{TLS_CONNECTION_OPENED_EVENT, TLS_DATA_RECEIVED_EVENT};

/// The `decision=` token for an LLM call that returned `Err`.
///
/// TLS has exactly one failure shape on the wire - a close_notify alert - so an overloaded
/// backend and a broken one cannot be told apart by the peer. They are told apart in the log
/// instead: the token is stable, so `grep 'decision=fail_closed_'` finds every connection that
/// was hung up on because nothing usable was produced, and the `_overloaded` suffix marks the
/// transient half. The error itself is logged, never written to the socket.
fn llm_error_decision(err: &anyhow::Error) -> &'static str {
    match WireFailure::classify(err) {
        WireFailure::Overloaded => "fail_closed_llm_error_overloaded",
        WireFailure::Unavailable => "fail_closed_llm_error_unavailable",
    }
}

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
    write_half: Arc<Mutex<tokio::io::WriteHalf<tokio_rustls::server::TlsStream<TcpStream>>>>,
}

/// TLS server that listens for incoming connections
pub struct TlsServer;

impl TlsServer {
    /// Spawn the TLS server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        send_first: bool,
        server_id: crate::state::ServerId,
        tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> Result<SocketAddr> {
        // Create TLS configuration (use provided or generate default)
        let tls_config = if let Some(config) = tls_config {
            config
        } else {
            crate::server::tls_cert_manager::generate_default_tls_config()
                .context("Failed to generate TLS configuration")?
        };

        // Create and bind TCP listener
        let listener = TcpListener::bind(listen_addr)
            .await
            .context("Failed to bind TLS TCP listener")?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "TLS server (action-based) listening on {}",
            local_addr
        ));

        let connections = Arc::new(Mutex::new(HashMap::new()));
        let protocol = Arc::new(TlsProtocol::new());
        let acceptor = TlsAcceptor::from(tls_config);

        // Spawn accept loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        debug!("TLS TCP connection from {}", remote_addr);

                        let acceptor = acceptor.clone();
                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let connections_clone = connections.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            // Perform TLS handshake
                            let tls_stream = match acceptor.accept(stream).await {
                                Ok(stream) => stream,
                                Err(e) => {
                                    // Handshake failure ends this connection but is a
                                    // client-side condition, not a server error: WARN.
                                    Log::new(Some(&status_tx_clone)).warn(format!(
                                        "TLS handshake failed with {}: {}",
                                        remote_addr, e
                                    ));
                                    return;
                                }
                            };

                            Log::new(Some(&status_tx_clone))
                                .debug(format!("TLS handshake complete with {}", remote_addr));

                            info!(
                                "Accepted TLS connection {} from {}",
                                connection_id, remote_addr
                            );

                            // Split stream
                            let (read_half, write_half) = tokio::io::split(tls_stream);
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
                                protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                                    "state": "Idle",
                                    "tls_handshake": "complete"
                                })),
                            };
                            app_state_clone
                                .add_connection_to_server(server_id, conn_state)
                                .await;
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());

                            // Register the connection HERE, before either task is spawned.
                            //
                            // This used to be the first thing the banner task did, racing the
                            // reader task spawned immediately after it, and
                            // handle_data_with_actions returns silently when the connection
                            // is not in the map. TLS loses that race almost every time: the
                            // handshake reads from the socket, so application data sent right
                            // behind the client's Finished is already buffered inside rustls
                            // and the reader's first read() returns it without ever waiting on
                            // the I/O driver. 15 of 16 clients that wrote at handshake
                            // completion had their request dropped with no response and no log
                            // line.
                            connections_clone.lock().await.insert(
                                connection_id,
                                ConnectionData {
                                    state: ConnectionState::Idle,
                                    queued_data: Vec::new(),
                                    write_half: write_half_arc.clone(),
                                },
                            );

                            // Send the greeting banner, if this server was asked for one.
                            if send_first {
                                let llm_client_for_conn = llm_client_clone.clone();
                                let app_state_for_conn = app_state_clone.clone();
                                let status_tx_for_conn = status_tx_clone.clone();
                                let connections_for_conn = connections_clone.clone();
                                let write_half_for_conn = write_half_arc.clone();
                                let protocol_for_conn = protocol_clone.clone();
                                tokio::spawn(async move {
                                    Self::send_banner(
                                        connection_id,
                                        server_id,
                                        llm_client_for_conn,
                                        app_state_for_conn,
                                        status_tx_for_conn,
                                        connections_for_conn,
                                        write_half_for_conn,
                                        protocol_for_conn,
                                    )
                                    .await;
                                });
                            }

                            // Spawn reader task
                            let llm_client_for_read = llm_client_clone.clone();
                            let app_state_for_read = app_state_clone.clone();
                            let status_tx_for_read = status_tx_clone.clone();
                            let connections_for_read = connections_clone.clone();
                            let protocol_for_read = protocol_clone.clone();
                            tokio::spawn(async move {
                                let mut buffer = vec![0u8; 8192];
                                let mut read_half = read_half;

                                loop {
                                    match read_half.read(&mut buffer).await {
                                        Ok(0) => {
                                            // Connection closed
                                            connections_for_read
                                                .lock()
                                                .await
                                                .remove(&connection_id);
                                            app_state_for_read
                                                .close_connection_on_server(
                                                    server_id,
                                                    connection_id,
                                                )
                                                .await;
                                            Log::new(Some(&status_tx_for_read)).info(format!(
                                                "TLS connection {connection_id} closed"
                                            ));
                                            let _ = status_tx_for_read
                                                .send("__UPDATE_UI__".to_string());
                                            break;
                                        }
                                        Ok(n) => {
                                            let data = Bytes::copy_from_slice(&buffer[..n]);

                                            // Data summary + full payload are FileOnly: the
                                            // tls_data_received event template renders the
                                            // equivalent lines to the TUI, so streaming the
                                            // payload here too would duplicate it and load the
                                            // unbounded status channel.
                                            let log = Log::new(Some(&status_tx_for_read));
                                            if data.iter().all(|&b| {
                                                b.is_ascii_graphic() || b.is_ascii_whitespace()
                                            }) {
                                                let data_str = String::from_utf8_lossy(&data);
                                                let preview = if data_str.len() > 100 {
                                                    format!("{}...", &data_str[..100])
                                                } else {
                                                    data_str.to_string()
                                                };
                                                log.debug(format!(
                                                    "TLS received {} bytes on {}: {}",
                                                    n, connection_id, preview
                                                ));
                                                log.trace(format!(
                                                    "TLS data (text): {:?}",
                                                    data_str
                                                ));
                                            } else {
                                                log.debug(format!(
                                                    "TLS received {} bytes on {} (binary data)",
                                                    n, connection_id
                                                ));
                                                log.trace(format!(
                                                    "TLS data (hex): {}",
                                                    hex::encode(&data)
                                                ));
                                            }

                                            // Handle data in separate task
                                            let llm_clone = llm_client_for_read.clone();
                                            let state_clone = app_state_for_read.clone();
                                            let status_clone = status_tx_for_read.clone();
                                            let conns_clone = connections_for_read.clone();
                                            let protocol_clone = protocol_for_read.clone();
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
                                            Log::new(Some(&status_tx_for_read)).error(format!(
                                                "Read error on {}: {}",
                                                connection_id, e
                                            ));
                                            connections_for_read
                                                .lock()
                                                .await
                                                .remove(&connection_id);
                                            break;
                                        }
                                    }
                                }
                            });
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Accept error: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Send the greeting banner for a new connection (`send_first` servers only).
    ///
    /// The connection is already registered by the accept path by the time this runs.
    #[allow(clippy::too_many_arguments)]
    async fn send_banner(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        write_half: Arc<Mutex<tokio::io::WriteHalf<tokio_rustls::server::TlsStream<TcpStream>>>>,
        protocol: Arc<TlsProtocol>,
    ) {
        {
            // Create connection opened event
            let event = Event::new(&TLS_CONNECTION_OPENED_EVENT, serde_json::json!({}));

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
                    debug!("LLM TLS banner response received");

                    // Display messages
                    for msg in execution_result.messages {
                        let _ = status_tx.send(msg);
                    }

                    // Handle protocol results (send banner)
                    let mut acted = false;
                    for protocol_result in execution_result.protocol_results {
                        match protocol_result {
                            ActionResult::Output(output_data) => {
                                acted = true;
                                let mut write = write_half.lock().await;
                                let log = Log::new(Some(&status_tx));
                                if let Err(e) = write.write_all(&output_data).await {
                                    log.error(format!("Failed to send banner: {}", e));
                                } else {
                                    // Sent-data summary + payload are FileOnly: the
                                    // send_tls_data action template already reports the send
                                    // to the TUI.
                                    if output_data
                                        .iter()
                                        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
                                    {
                                        let data_str = String::from_utf8_lossy(&output_data);
                                        let preview = if data_str.len() > 100 {
                                            format!("{}...", &data_str[..100])
                                        } else {
                                            data_str.to_string()
                                        };
                                        log.debug(format!(
                                            "TLS sent {} bytes to {}: {}",
                                            output_data.len(),
                                            connection_id,
                                            preview
                                        ));
                                        log.trace(format!("TLS sent (text): {:?}", data_str));
                                    } else {
                                        log.debug(format!(
                                            "TLS sent {} bytes to {} (binary data)",
                                            output_data.len(),
                                            connection_id
                                        ));
                                        log.trace(format!(
                                            "TLS sent (hex): {}",
                                            hex::encode(&output_data)
                                        ));
                                    }
                                    log.debug(format!("Sent banner to {connection_id}"));
                                }
                            }
                            ActionResult::CloseConnection => {
                                acted = true;
                                connections.lock().await.remove(&connection_id);
                                if let Err(e) = write_half.lock().await.shutdown().await {
                                    debug!("TLS shutdown on {} returned: {}", connection_id, e);
                                }
                                Log::new(Some(&status_tx)).info(format!(
                                    "Closed TLS connection {connection_id} after banner"
                                ));
                            }
                            _ => {}
                        }
                    }

                    // The model answering with no banner is a real answer - "say nothing" -
                    // and is left as silence, but it is not the same as the backend failing,
                    // and a peer waiting for a greeting that never comes is worth a line.
                    if !acted {
                        Log::new(Some(&status_tx)).warn(format!(
                            "TLS connection {connection_id} decision=model_no_action: model \
                             produced no banner, nothing sent"
                        ));
                    }
                }
                Err(e) => {
                    // A `send_first` server owes the peer a greeting; without one the peer
                    // waits for a banner that will never come. Close the TLS connection with
                    // a close_notify alert so it reads EOF instead. (See the data path below
                    // for why this is close_notify and not a fatal alert.)
                    // The full error goes to the log and the status stream, which is where
                    // an operator looks; the peer gets a close_notify alert and nothing else.
                    let decision = llm_error_decision(&e);
                    error!(
                        "TLS connection {} decision={}: LLM call failed generating banner: {:#}",
                        connection_id, decision, e
                    );
                    Log::new(Some(&status_tx)).warn(format!(
                        "TLS connection {connection_id} decision={decision}: no banner \
                         generated, closing with close_notify"
                    ));
                    {
                        let mut write = write_half.lock().await;
                        if let Err(shutdown_err) = write.shutdown().await {
                            debug!(
                                "TLS shutdown on {} returned: {}",
                                connection_id, shutdown_err
                            );
                        }
                    }
                    connections.lock().await.remove(&connection_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    Log::new(Some(&status_tx)).info(format!(
                        "Closed TLS connection {connection_id} after banner LLM error"
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
        protocol: Arc<TlsProtocol>,
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
                    "TLS connection {} is no longer registered; dropping {} received bytes",
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
            Log::new(Some(&status_tx)).debug(format!(
                "Queued {} bytes for {}",
                data.len(),
                connection_id
            ));
            return;
        }

        // Merge any queued data with new data.
        //
        // The lock was released after the state check above, so the reader task
        // may have removed this connection in the meantime (client disconnected).
        // Unwrapping here panicked the task on that race, and a panicked task
        // leaves the server reporting Running.
        let mut all_data = {
            let mut conns = connections.lock().await;
            let Some(conn_data) = conns.get_mut(&connection_id) else {
                return; // Connection closed while we were waiting for the lock
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
                    "TLS connection {} went away before its response could be written",
                    connection_id
                );
                return;
            };

            // Format data for event parameter. The encoding is reported
            // alongside so the model can echo binary back through
            // send_tls_data with encoding="hex" instead of sending the ASCII
            // hex digits.
            let printable = all_data
                .iter()
                .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace());
            let (data_str, encoding) = if printable {
                (String::from_utf8_lossy(&all_data).to_string(), "utf8")
            } else {
                (hex::encode(&all_data), "hex")
            };

            // Create data received event
            let event = Event::new(
                &TLS_DATA_RECEIVED_EVENT,
                serde_json::json!({
                    "data": data_str,
                    "encoding": encoding
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
                    debug!("LLM TLS response received");

                    // Display messages
                    for msg in execution_result.messages {
                        let _ = status_tx.send(msg);
                    }

                    // Handle protocol results
                    let mut should_close = false;
                    let mut should_wait = false;
                    let mut acted = false;

                    for protocol_result in execution_result.protocol_results {
                        match protocol_result {
                            ActionResult::Output(output_data) => {
                                acted = true;
                                let mut write = write_half.lock().await;
                                let log = Log::new(Some(&status_tx));
                                if let Err(e) = write.write_all(&output_data).await {
                                    log.error(format!("Failed to send response: {}", e));
                                } else {
                                    // Sent-data summary + payload are FileOnly: the
                                    // send_tls_data action template already reports the send
                                    // to the TUI.
                                    if output_data
                                        .iter()
                                        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
                                    {
                                        let data_str = String::from_utf8_lossy(&output_data);
                                        let preview = if data_str.len() > 100 {
                                            format!("{}...", &data_str[..100])
                                        } else {
                                            data_str.to_string()
                                        };
                                        log.debug(format!(
                                            "TLS sent {} bytes to {}: {}",
                                            output_data.len(),
                                            connection_id,
                                            preview
                                        ));
                                        log.trace(format!("TLS sent (text): {:?}", data_str));
                                    } else {
                                        log.debug(format!(
                                            "TLS sent {} bytes to {} (binary data)",
                                            output_data.len(),
                                            connection_id
                                        ));
                                        log.trace(format!(
                                            "TLS sent (hex): {}",
                                            hex::encode(&output_data)
                                        ));
                                    }
                                    log.debug(format!(
                                        "Sent {} bytes to {}",
                                        output_data.len(),
                                        connection_id
                                    ));
                                }
                            }
                            ActionResult::CloseConnection => {
                                acted = true;
                                should_close = true;
                            }
                            ActionResult::WaitForMore => {
                                acted = true;
                                should_wait = true;
                            }
                            _ => {}
                        }
                    }

                    // An answer that carries no output, no close and no wait leaves the
                    // peer holding an unanswered request. That is a legitimate "say nothing"
                    // for a transport with no reply obligation, so the connection stays open -
                    // but it is logged distinctly from a backend failure, which hangs up.
                    if !acted {
                        Log::new(Some(&status_tx)).warn(format!(
                            "TLS connection {connection_id} decision=model_no_action: model \
                             produced no usable action, nothing sent"
                        ));
                    }

                    // Handle wait_for_more
                    if should_wait {
                        connections
                            .lock()
                            .await
                            .entry(connection_id)
                            .and_modify(|conn| conn.state = ConnectionState::Accumulating);
                        Log::new(Some(&status_tx))
                            .debug(format!("Waiting for more data from {connection_id}"));
                        return;
                    }

                    // Handle close_connection.
                    //
                    // Dropping the map entry alone left the socket open forever
                    // (the reader task kept blocking on read and the client never
                    // saw a close), so close_this_connection did not close
                    // anything. Shut the TLS write half down to send close_notify
                    // and let the peer's read return EOF.
                    if should_close {
                        connections.lock().await.remove(&connection_id);
                        if let Err(e) = write_half.lock().await.shutdown().await {
                            debug!("TLS shutdown on {} returned: {}", connection_id, e);
                        }
                        Log::new(Some(&status_tx))
                            .info(format!("Closed TLS connection {connection_id}"));
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
                    // The full error goes to the log and the status stream; the peer gets a
                    // close_notify alert carrying nothing about why.
                    let decision = llm_error_decision(&e);
                    error!(
                        "TLS connection {} decision={}: LLM call failed for received data: {:#}",
                        connection_id, decision, e
                    );
                    Log::new(Some(&status_tx)).warn(format!(
                        "TLS connection {connection_id} decision={decision}: no response \
                         generated, closing with close_notify"
                    ));

                    // Say something on the wire instead of resetting to Idle in silence.
                    //
                    // TLS carries no application-level error - the application protocol here
                    // is whatever the handler invents, so there is no reply we could phrase in
                    // it. What TLS does have is the alert protocol, and `shutdown()` emits a
                    // real close_notify alert record (not just a FIN), which every TLS client
                    // surfaces as a clean end of stream immediately rather than blocking until
                    // its own timeout.
                    //
                    // A *fatal* `internal_error` alert would be more precise, but rustls 0.23
                    // keeps `CommonState::send_fatal_alert` `pub(crate)`; close_notify is the
                    // strongest in-spec signal reachable through its public API. Forging a
                    // plaintext alert record onto the TCP socket underneath would violate
                    // TLS 1.3 record protection, so it is not done.
                    {
                        let mut write = write_half.lock().await;
                        if let Err(shutdown_err) = write.shutdown().await {
                            debug!(
                                "TLS shutdown on {} returned: {}",
                                connection_id, shutdown_err
                            );
                        }
                    }
                    connections.lock().await.remove(&connection_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    Log::new(Some(&status_tx)).info(format!(
                        "Closed TLS connection {connection_id} after LLM error"
                    ));
                    return;
                }
            }
        }
    }
}
