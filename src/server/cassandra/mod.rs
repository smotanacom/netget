//! Cassandra/CQL server implementation using cassandra-protocol
//!
//! Phase 1 (Minimal Viable):
//! - STARTUP → READY (no auth)
//! - OPTIONS → SUPPORTED
//! - QUERY → RESULT (LLM-generated rows)
//! - Single-stream operation (sequential processing)
//! - Protocol v4 only
//! - Basic types: int, varchar, boolean

pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use actions::*;
use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use cassandra_protocol::compression::Compression;
use cassandra_protocol::frame::{Direction, Envelope, Flags, Opcode, Version};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// Largest frame body accepted, matching the native protocol's own 256 MiB maximum.
const MAX_FRAME_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Cap on prepared statements retained per connection.
///
/// This is protocol session state, not storage: it holds the query text a client asked the
/// server to remember so a later EXECUTE can be resolved back to it. It still needs a bound,
/// because a client can PREPARE unlimited distinct queries on one connection and every one of
/// them was retained for the life of that connection.
const MAX_PREPARED_STATEMENTS: usize = 1024;

/// Native-protocol ERROR code 0x0000, "Server error: something unexpected happened".
const CASSANDRA_ERROR_SERVER_ERROR: u32 = 0x0000;

/// Native-protocol ERROR code 0x1001, "Overloaded: the request cannot be processed because the
/// coordinator node is overloaded". Drivers treat it as retryable.
const CASSANDRA_ERROR_OVERLOADED: u32 = 0x1001;

/// Cassandra server implementation
pub struct CassandraServer {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    _status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
}

/// Connection state for a Cassandra client
struct CassandraConnectionState {
    ready: bool,
    protocol_version: u8,
    /// Prepared statements: statement_id -> (query_string, param_count)
    prepared_statements: HashMap<Vec<u8>, (String, usize)>,
    /// Authentication state
    authenticated: bool,
    /// Authenticated username (if any)
    username: Option<String>,
}

impl CassandraServer {
    /// Create a new Cassandra server
    pub fn new(
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
    ) -> Self {
        Self {
            llm_client,
            app_state,
            _status_tx: status_tx,
            server_id,
        }
    }

    /// Spawn Cassandra server with LLM integration
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        _send_first: bool,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx))
            .info(format!("Cassandra/CQL server listening on {}", actual_addr));

        let server = Arc::new(CassandraServer::new(
            llm_client,
            app_state.clone(),
            status_tx.clone(),
            Some(server_id),
        ));

        // Spawn the accept loop
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        Log::new(Some(&status_tx))
                            .debug(format!("Cassandra connection from {}", addr));

                        let server_clone = server.clone();
                        let status_tx_clone = status_tx.clone();

                        tokio::spawn(async move {
                            if let Err(e) = server_clone
                                .handle_connection(stream, addr, status_tx_clone)
                                .await
                            {
                                error!("Cassandra connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        // A persistent accept error (EMFILE, listener torn down) recurs
                        // immediately, so continuing spins a hot loop that floods the
                        // unbounded status channel. Stop the listener instead.
                        Log::new(Some(&status_tx))
                            .error(format!("Cassandra accept failed, listener stopped: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        app_state
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(actual_addr)
    }

    /// Handle a single Cassandra connection
    async fn handle_connection(
        &self,
        mut stream: TcpStream,
        addr: SocketAddr,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let connection_id = ConnectionId::new(self.app_state.get_next_unified_id().await);

        // Track the connection
        if let Some(server_id) = self.server_id {
            let now = Instant::now();
            let conn_state = ConnectionState {
                id: connection_id,
                remote_addr: addr,
                local_addr: stream.local_addr().unwrap_or(addr),
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: now,
                status: ConnectionStatus::Active,
                status_changed_at: now,
                protocol_info: ProtocolConnectionInfo::empty(),
            };

            self.app_state
                .add_connection_to_server(server_id, conn_state)
                .await;
        }

        let mut conn_state = CassandraConnectionState {
            ready: false,
            protocol_version: 4,
            prepared_statements: HashMap::new(),
            authenticated: false,
            username: None,
        };

        let mut buffer = BytesMut::with_capacity(4096);
        // Set once a handler asks to close, so the outer read loop stops too. Breaking only
        // out of the inner frame loop left the connection open and re-entered read_buf, which
        // made `close_this_connection` a no-op on every path.
        let mut closing = false;

        loop {
            // Read data from stream
            let n = match stream.read_buf(&mut buffer).await {
                Ok(0) => {
                    Log::new(Some(&status_tx))
                        .debug(format!("Cassandra client {} disconnected", addr));
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    error!("Read error from {}: {}", addr, e);
                    break;
                }
            };

            trace!("Read {} bytes from Cassandra client {}", n, addr);

            // Try to parse and handle frames
            while buffer.remaining() >= 9 {
                // Check if we have a complete frame header (9 bytes)
                let frame_start = buffer.as_ref();
                if frame_start.len() < 9 {
                    break;
                }

                // Read frame length from header (bytes 5-8)
                let length = u32::from_be_bytes([
                    frame_start[5],
                    frame_start[6],
                    frame_start[7],
                    frame_start[8],
                ]) as usize;

                // The length is attacker-chosen and up to 4 GiB. Without this check the loop
                // simply waits for the declared bytes while read_buf keeps growing BytesMut,
                // so a client that declares a huge frame and then dribbles data grows the
                // process without limit. 256 MiB is the protocol's own maximum frame size.
                if length > MAX_FRAME_BODY_BYTES {
                    Log::new(Some(&status_tx)).error(format!(
                        "Cassandra frame too large ({} bytes, limit {}), closing {}",
                        length, MAX_FRAME_BODY_BYTES, addr
                    ));
                    closing = true;
                    break;
                }

                // Check if we have the complete frame
                if buffer.remaining() < 9 + length {
                    trace!(
                        "Waiting for complete frame: have {}, need {}",
                        buffer.remaining(),
                        9 + length
                    );
                    break;
                }

                // We have a complete frame, parse it
                let frame_bytes = buffer.split_to(9 + length);

                match self
                    .handle_frame(
                        &frame_bytes,
                        &mut conn_state,
                        &mut stream,
                        connection_id,
                        &status_tx,
                    )
                    .await
                {
                    Ok(should_continue) => {
                        if !should_continue {
                            debug!("Closing Cassandra connection to {}", addr);
                            closing = true;
                            break;
                        }
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Cassandra frame handling error: {}", e));
                        // Send error frame and close connection
                        closing = true;
                        break;
                    }
                }
            }

            if closing {
                break;
            }
        }

        // Close connection
        if let Some(server_id) = self.server_id {
            self.app_state
                .close_connection_on_server(server_id, connection_id)
                .await;
        }

        Ok(())
    }

    /// Handle a single Cassandra frame
    async fn handle_frame(
        &self,
        frame_bytes: &[u8],
        conn_state: &mut CassandraConnectionState,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        // Parse frame using cassandra-protocol
        let parsed = Envelope::from_buffer(frame_bytes, Compression::None)
            .context("Failed to parse Cassandra frame")?;
        let frame = parsed.envelope;

        Log::new(Some(status_tx)).trace(format!(
            "Cassandra ← {:?} stream={}",
            frame.opcode, frame.stream_id
        ));

        match frame.opcode {
            Opcode::Startup => {
                self.handle_startup(frame, conn_state, stream, connection_id, status_tx)
                    .await
            }
            Opcode::Options => {
                self.handle_options(frame, stream, connection_id, status_tx)
                    .await
            }
            Opcode::Query => {
                self.handle_query(frame, stream, connection_id, status_tx)
                    .await
            }
            Opcode::Prepare => {
                self.handle_prepare(frame, conn_state, stream, connection_id, status_tx)
                    .await
            }
            Opcode::Execute => {
                self.handle_execute(frame, conn_state, stream, connection_id, status_tx)
                    .await
            }
            Opcode::AuthResponse => {
                self.handle_auth_response(frame, conn_state, stream, connection_id, status_tx)
                    .await
            }
            Opcode::Register => {
                // Client wants to register for server events
                // We don't support server events, but respond with READY to acknowledge
                Log::new(Some(status_tx))
                    .debug("Cassandra: Client registered for events (not supported, no-op)");
                self.send_ready(frame.stream_id, stream, status_tx).await?;
                Ok(true)
            }
            _ => {
                Log::new(Some(status_tx))
                    .warn(format!("Unsupported Cassandra opcode: {:?}", frame.opcode));
                // Send error response
                self.send_error(
                    frame.stream_id,
                    0x000A,
                    "Unsupported operation",
                    stream,
                    status_tx,
                )
                .await?;
                Ok(true)
            }
        }
    }

    /// Handle STARTUP frame
    async fn handle_startup(
        &self,
        frame: Envelope,
        conn_state: &mut CassandraConnectionState,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        debug!("Handling STARTUP from connection {}", connection_id);

        // Parse startup options (CQL_VERSION, etc.)
        // For Phase 1, we just accept any version and send READY

        // Call LLM to decide response
        let protocol =
            CassandraProtocol::new(connection_id, self.app_state.clone(), status_tx.clone());

        let event = Event {
            event_type: &CASSANDRA_STARTUP_EVENT,
            data: json!({
                "protocol_version": conn_state.protocol_version,
                "options": {"CQL_VERSION": "3.0.0"}
            }),
        };

        let server_id = self.server_id.context("Server ID not set")?;

        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.send_llm_failure_error(
                    frame.stream_id,
                    "STARTUP",
                    &e,
                    stream,
                    connection_id,
                    status_tx,
                )
                .await?;
                return Ok(true);
            }
        };

        // Show messages
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        // Execute the protocol actions
        for action_result in execution_result.protocol_results {
            match action_result {
                ActionResult::Custom { name, data } => match name.as_str() {
                    "cassandra_ready" => {
                        conn_state.ready = true;
                        self.send_ready(frame.stream_id, stream, status_tx).await?;
                        return Ok(true);
                    }
                    // Answering STARTUP with AUTHENTICATE is the only way a driver is ever
                    // prompted for credentials. Without it the client goes straight to
                    // queries, so cassandra_auth never fired and cassandra_auth_success was
                    // unreachable - the whole "Phase 3" auth path was dead.
                    "cassandra_authenticate" => {
                        let authenticator = data
                            .get("authenticator")
                            .and_then(|v| v.as_str())
                            .unwrap_or("org.apache.cassandra.auth.PasswordAuthenticator");
                        self.send_authenticate(frame.stream_id, authenticator, stream, status_tx)
                            .await?;
                        return Ok(true);
                    }
                    "cassandra_error" => {
                        let error_code = data
                            .get("error_code")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0x0000) as u32;
                        let message = data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown error");
                        self.send_model_error(
                            frame.stream_id,
                            "STARTUP",
                            error_code,
                            message,
                            stream,
                            connection_id,
                            status_tx,
                        )
                        .await?;
                        return Ok(true);
                    }
                    _ => {}
                },
                ActionResult::CloseConnection => {
                    return Ok(false);
                }
                _ => {
                    warn!("Unexpected action result for STARTUP");
                }
            }
        }

        // Fail closed: a CQL session is granted by an explicit `cassandra_ready` or
        // `cassandra_authenticate` and by nothing else. Falling through to READY here handed
        // out an unauthenticated session on silence, so the model had no way to make a refusal
        // distinguishable from a backend outage.
        self.send_no_answer_error(
            frame.stream_id,
            "STARTUP",
            stream,
            connection_id,
            status_tx,
        )
        .await?;
        Ok(false)
    }

    /// Handle OPTIONS frame
    async fn handle_options(
        &self,
        frame: Envelope,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        debug!("Handling OPTIONS from connection {}", connection_id);

        let protocol =
            CassandraProtocol::new(connection_id, self.app_state.clone(), status_tx.clone());

        let event = Event {
            event_type: &CASSANDRA_OPTIONS_EVENT,
            data: json!({}),
        };

        let server_id = self.server_id.context("Server ID not set")?;

        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.send_llm_failure_error(
                    frame.stream_id,
                    "OPTIONS",
                    &e,
                    stream,
                    connection_id,
                    status_tx,
                )
                .await?;
                return Ok(true);
            }
        };

        // Show messages
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        // Execute the protocol actions
        for action_result in execution_result.protocol_results {
            match action_result {
                ActionResult::Custom { name, data } => match name.as_str() {
                    "cassandra_supported" => {
                        let options = data
                            .get("options")
                            .and_then(|v| v.as_object())
                            .cloned()
                            .unwrap_or_default();
                        self.send_supported(frame.stream_id, options, stream, status_tx)
                            .await?;
                        return Ok(true);
                    }
                    "cassandra_error" => {
                        let error_code = data
                            .get("error_code")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0x0000) as u32;
                        let message = data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown error");
                        self.send_model_error(
                            frame.stream_id,
                            "OPTIONS",
                            error_code,
                            message,
                            stream,
                            connection_id,
                            status_tx,
                        )
                        .await?;
                        return Ok(true);
                    }
                    _ => {}
                },
                ActionResult::CloseConnection => {
                    return Ok(false);
                }
                _ => {
                    warn!("Unexpected action result for OPTIONS");
                }
            }
        }

        // If no action was executed, send default SUPPORTED
        self.send_supported(frame.stream_id, serde_json::Map::new(), stream, status_tx)
            .await?;
        Ok(true)
    }

    /// Handle QUERY frame
    async fn handle_query(
        &self,
        frame: Envelope,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        // Parse query from frame body
        let query_str = self.parse_query(&frame)?;

        Log::new(Some(status_tx)).debug(format!(
            "Cassandra ← Query from connection {}: {}",
            connection_id, query_str
        ));

        let protocol =
            CassandraProtocol::new(connection_id, self.app_state.clone(), status_tx.clone());

        let event = Event {
            event_type: &CASSANDRA_QUERY_EVENT,
            data: json!({
                "query": query_str,
                "consistency": "ONE"
            }),
        };

        let server_id = self.server_id.context("Server ID not set")?;

        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.send_llm_failure_error(
                    frame.stream_id,
                    "QUERY",
                    &e,
                    stream,
                    connection_id,
                    status_tx,
                )
                .await?;
                return Ok(true);
            }
        };

        // Show messages
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        // Execute the protocol actions
        for action_result in execution_result.protocol_results {
            match action_result {
                ActionResult::Custom { name, data } => match name.as_str() {
                    "cassandra_result_rows" => {
                        let columns = data
                            .get("columns")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let rows = data
                            .get("rows")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        self.send_result_rows(frame.stream_id, columns, rows, stream, status_tx)
                            .await?;
                        return Ok(true);
                    }
                    "cassandra_error" => {
                        let error_code = data
                            .get("error_code")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0x0000) as u32;
                        let message = data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown error");
                        self.send_model_error(
                            frame.stream_id,
                            "QUERY",
                            error_code,
                            message,
                            stream,
                            connection_id,
                            status_tx,
                        )
                        .await?;
                        return Ok(true);
                    }
                    _ => {}
                },
                ActionResult::CloseConnection => {
                    return Ok(false);
                }
                _ => {
                    warn!("Unexpected action result for QUERY");
                }
            }
        }

        // Fail closed: an empty RESULT/Rows frame means "no rows matched", which is an
        // answer about the data. A missing handler answer is not that.
        self.send_no_answer_error(frame.stream_id, "QUERY", stream, connection_id, status_tx)
            .await?;
        Ok(true)
    }

    /// Parse query string from QUERY frame
    fn parse_query(&self, frame: &Envelope) -> Result<String> {
        // Frame body contains:
        // - query (long string)
        // - query parameters

        // For Phase 1, we do simple parsing
        // The body starts with a [long string] for the query
        let body = &frame.body;
        if body.len() < 4 {
            return Err(anyhow::anyhow!("Query frame too short"));
        }

        // Read long string length (4 bytes, big-endian)
        let query_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;

        if body.len() < 4 + query_len {
            return Err(anyhow::anyhow!("Query frame truncated"));
        }

        let query_bytes = &body[4..4 + query_len];
        let query_str = String::from_utf8_lossy(query_bytes).to_string();

        Ok(query_str)
    }

    /// Send READY response
    async fn send_ready(
        &self,
        stream_id: i16,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::Ready,
            body: vec![],
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace(format!("Cassandra → READY ({} bytes)", bytes.len()));

        Ok(())
    }

    /// Send SUPPORTED response
    async fn send_supported(
        &self,
        stream_id: i16,
        options: serde_json::Map<String, serde_json::Value>,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // Build SUPPORTED body: string multimap
        // For Phase 1, send minimal options
        let mut body = Vec::new();

        // Number of options (2 bytes)
        body.extend_from_slice(&(options.len() as u16).to_be_bytes());

        for (key, value) in options.iter() {
            // Key (string)
            let key_bytes = key.as_bytes();
            body.extend_from_slice(&(key_bytes.len() as u16).to_be_bytes());
            body.extend_from_slice(key_bytes);

            // Value list (string list)
            if let Some(arr) = value.as_array() {
                body.extend_from_slice(&(arr.len() as u16).to_be_bytes());
                for item in arr {
                    if let Some(s) = item.as_str() {
                        let s_bytes = s.as_bytes();
                        body.extend_from_slice(&(s_bytes.len() as u16).to_be_bytes());
                        body.extend_from_slice(s_bytes);
                    }
                }
            } else {
                // Empty list
                body.extend_from_slice(&0u16.to_be_bytes());
            }
        }

        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::Supported,
            body,
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace(format!("Cassandra → SUPPORTED ({} bytes)", bytes.len()));

        Ok(())
    }

    /// Send RESULT with rows
    async fn send_result_rows(
        &self,
        stream_id: i16,
        columns: Vec<serde_json::Value>,
        rows: Vec<serde_json::Value>,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // Build RESULT body (kind=ROWS)
        // This is complex in Cassandra protocol - for Phase 1, we send a simplified response
        let mut body = Vec::new();

        // Result kind (4 bytes): 0x0002 = Rows
        body.extend_from_slice(&0x00000002u32.to_be_bytes());

        // Metadata
        // Flags (4 bytes): 0x0001 = GlobalTablesSpec
        body.extend_from_slice(&0x00000001u32.to_be_bytes());

        // Column count (4 bytes)
        body.extend_from_slice(&(columns.len() as u32).to_be_bytes());

        // Global keyspace and table (for simplicity)
        let keyspace = b"system";
        let table = b"local";
        body.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
        body.extend_from_slice(keyspace);
        body.extend_from_slice(&(table.len() as u16).to_be_bytes());
        body.extend_from_slice(table);

        // Column specs
        for col in &columns {
            let name = col.get("name").and_then(|v| v.as_str()).unwrap_or("col");
            let col_type = col
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("varchar");

            // Column name
            body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            body.extend_from_slice(name.as_bytes());

            // Column type (simplified: 0x000D = varchar, 0x0009 = int)
            let type_code: u16 = match col_type {
                "int" => 0x0009,
                "boolean" => 0x0004,
                _ => 0x000D, // varchar
            };
            body.extend_from_slice(&type_code.to_be_bytes());
        }

        // Rows count (4 bytes)
        body.extend_from_slice(&(rows.len() as u32).to_be_bytes());

        // Row data.
        //
        // A driver reads exactly columns.len() cells per row and takes whatever follows as
        // the next row. A handler that returns a row with the wrong number of cells therefore
        // does not just mangle that row - it desynchronizes the rest of the result set. Rows
        // are padded with NULL and truncated to the declared column count so a wrong-arity
        // answer stays parseable.
        for row in &rows {
            let empty = Vec::new();
            let row_arr = row.as_array().unwrap_or(&empty);
            if row_arr.len() != columns.len() {
                warn!(
                    "Cassandra row has {} cell(s) but {} column(s) were declared; padding/truncating",
                    row_arr.len(),
                    columns.len()
                );
            }
            for i in 0..columns.len() {
                let col_type = columns[i].get("type").and_then(|v| v.as_str());
                let cell_bytes = row_arr
                    .get(i)
                    .and_then(|cell| self.serialize_cell_value(cell, col_type));

                if let Some(bytes) = cell_bytes {
                    body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    body.extend_from_slice(&bytes);
                } else {
                    // NULL value (-1)
                    body.extend_from_slice(&(-1i32).to_be_bytes());
                }
            }
        }

        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::Result,
            body,
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace(format!(
            "Cassandra → RESULT ({} rows, {} bytes)",
            rows.len(),
            bytes.len()
        ));

        Ok(())
    }

    /// Serialize a cell for the column type that was declared for it.
    ///
    /// The column type must drive the encoding, not the JSON type. The column spec on the wire
    /// tells the driver how to read the bytes, so a column declared `int` whose value arrives
    /// as the JSON string `"5"` has to go out as a 4-byte big-endian 5 - emitting the ASCII
    /// "5" made the driver read a 1-byte int and fail. This function previously ignored the
    /// column type entirely (the parameter was `_col_type`) and switched on the JSON type
    /// alone, so any handler that quoted a number, or answered a varchar column with a number,
    /// produced a result set the driver rejected.
    ///
    /// Returns `None` for a NULL cell.
    fn serialize_cell_value(
        &self,
        value: &serde_json::Value,
        col_type: Option<&str>,
    ) -> Option<Vec<u8>> {
        if value.is_null() {
            return None;
        }

        match col_type.unwrap_or("varchar") {
            "int" => {
                let n = match value {
                    serde_json::Value::Number(n) => n.as_i64(),
                    serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
                    serde_json::Value::Bool(b) => Some(if *b { 1 } else { 0 }),
                    _ => None,
                };
                match n {
                    // Out-of-range values are NULL rather than silently wrapped: a truncated
                    // integer is a wrong answer the client cannot detect.
                    Some(n) if n >= i32::MIN as i64 && n <= i32::MAX as i64 => {
                        Some((n as i32).to_be_bytes().to_vec())
                    }
                    _ => {
                        warn!("Cassandra int column got {:?}, sending NULL", value);
                        None
                    }
                }
            }
            "boolean" => {
                let b = match value {
                    serde_json::Value::Bool(b) => Some(*b),
                    serde_json::Value::Number(n) => n.as_i64().map(|n| n != 0),
                    serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" => Some(true),
                        "false" | "0" | "no" => Some(false),
                        _ => None,
                    },
                    _ => None,
                };
                match b {
                    Some(b) => Some(vec![u8::from(b)]),
                    None => {
                        warn!("Cassandra boolean column got {:?}, sending NULL", value);
                        None
                    }
                }
            }
            // varchar/text and every unrecognized type are sent as UTF-8, matching the
            // 0x000D type code send_result_rows writes for them. Strings go out verbatim;
            // anything else is rendered rather than dropped.
            _ => match value {
                serde_json::Value::String(s) => Some(s.as_bytes().to_vec()),
                other => Some(other.to_string().into_bytes()),
            },
        }
    }

    /// Handle PREPARE frame
    async fn handle_prepare(
        &self,
        frame: Envelope,
        conn_state: &mut CassandraConnectionState,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        debug!("Handling PREPARE from connection {}", connection_id);

        // Parse query from frame
        let query = self.parse_query(&frame)?;
        trace!("PREPARE query: {}", query);

        // Generate statement ID from query hash
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        query.hash(&mut hasher);
        let hash = hasher.finish();
        let statement_id = hash.to_be_bytes().to_vec();

        // Count parameters in query (simple heuristic: count '?' occurrences)
        let param_count = query.matches('?').count();

        // Store prepared statement
        if conn_state.prepared_statements.len() >= MAX_PREPARED_STATEMENTS
            && !conn_state.prepared_statements.contains_key(&statement_id)
        {
            warn!(
                "Cassandra connection {} reached {} prepared statements; rejecting PREPARE",
                connection_id, MAX_PREPARED_STATEMENTS
            );
            self.send_error(
                frame.stream_id,
                0x2200,
                "Too many prepared statements on this connection",
                stream,
                status_tx,
            )
            .await?;
            return Ok(true);
        }
        conn_state
            .prepared_statements
            .insert(statement_id.clone(), (query.clone(), param_count));

        debug!(
            "Prepared statement ID {:?} with {} params",
            statement_id, param_count
        );

        // Call LLM to decide response
        let protocol =
            CassandraProtocol::new(connection_id, self.app_state.clone(), status_tx.clone());

        let event = Event {
            event_type: &CASSANDRA_PREPARE_EVENT,
            data: json!({
                "query": query,
                "statement_id": hex::encode(&statement_id),
                "param_count": param_count,
            }),
        };

        let server_id = self.server_id.context("Server ID not set")?;

        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.send_llm_failure_error(
                    frame.stream_id,
                    "PREPARE",
                    &e,
                    stream,
                    connection_id,
                    status_tx,
                )
                .await?;
                return Ok(true);
            }
        };

        // Show messages
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        // Execute the protocol actions
        for action_result in execution_result.protocol_results {
            match action_result {
                ActionResult::Custom { name, data } => match name.as_str() {
                    "cassandra_prepared" => {
                        let columns = data
                            .get("columns")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let params = data.get("params").and_then(|v| v.as_array()).cloned();
                        self.send_prepared(
                            frame.stream_id,
                            statement_id,
                            columns,
                            params,
                            param_count,
                            stream,
                            status_tx,
                        )
                        .await?;
                        return Ok(true);
                    }
                    "cassandra_error" => {
                        let error_code = data
                            .get("error_code")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0x0000) as u32;
                        let message = data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown error");
                        self.send_model_error(
                            frame.stream_id,
                            "PREPARE",
                            error_code,
                            message,
                            stream,
                            connection_id,
                            status_tx,
                        )
                        .await?;
                        return Ok(true);
                    }
                    _ => {}
                },
                ActionResult::CloseConnection => {
                    return Ok(false);
                }
                _ => {
                    warn!("Unexpected action result for PREPARE");
                }
            }
        }

        // Fail closed: handing back a valid statement id would let a later EXECUTE run off a
        // preparation nobody approved.
        self.send_no_answer_error(frame.stream_id, "PREPARE", stream, connection_id, status_tx)
            .await?;
        Ok(true)
    }

    /// Handle EXECUTE frame
    async fn handle_execute(
        &self,
        frame: Envelope,
        conn_state: &mut CassandraConnectionState,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        debug!("Handling EXECUTE from connection {}", connection_id);

        // Parse statement ID and parameters from frame
        let (statement_id, params) = self.parse_execute(&frame)?;

        // Look up prepared statement
        let (query, expected_param_count) = conn_state
            .prepared_statements
            .get(&statement_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown prepared statement ID"))?
            .clone();

        trace!("EXECUTE statement: {} with {} params", query, params.len());

        // Validate parameter count
        if params.len() != expected_param_count {
            let err_msg = format!(
                "Expected {} parameters, got {}",
                expected_param_count,
                params.len()
            );
            self.send_error(frame.stream_id, 0x2200, &err_msg, stream, status_tx)
                .await?;
            return Ok(true);
        }

        // Call LLM with query and bound parameters
        let protocol =
            CassandraProtocol::new(connection_id, self.app_state.clone(), status_tx.clone());

        let event = Event {
            event_type: &CASSANDRA_EXECUTE_EVENT,
            data: json!({
                "query": query,
                "statement_id": hex::encode(&statement_id),
                "parameters": params,
            }),
        };

        let server_id = self.server_id.context("Server ID not set")?;

        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.send_llm_failure_error(
                    frame.stream_id,
                    "EXECUTE",
                    &e,
                    stream,
                    connection_id,
                    status_tx,
                )
                .await?;
                return Ok(true);
            }
        };

        // Show messages
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        // Execute the protocol actions
        for action_result in execution_result.protocol_results {
            match action_result {
                ActionResult::Custom { name, data } => match name.as_str() {
                    "cassandra_result_rows" => {
                        let columns = data
                            .get("columns")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let rows = data
                            .get("rows")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        self.send_result_rows(frame.stream_id, columns, rows, stream, status_tx)
                            .await?;
                        return Ok(true);
                    }
                    "cassandra_error" => {
                        let error_code = data
                            .get("error_code")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0x0000) as u32;
                        let message = data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown error");
                        self.send_model_error(
                            frame.stream_id,
                            "EXECUTE",
                            error_code,
                            message,
                            stream,
                            connection_id,
                            status_tx,
                        )
                        .await?;
                        return Ok(true);
                    }
                    _ => {}
                },
                ActionResult::CloseConnection => {
                    return Ok(false);
                }
                _ => {
                    warn!("Unexpected action result for EXECUTE");
                }
            }
        }

        // Fail closed: an empty RESULT/Rows frame means "no rows matched", which is an
        // answer about the data. A missing handler answer is not that.
        self.send_no_answer_error(frame.stream_id, "EXECUTE", stream, connection_id, status_tx)
            .await?;
        Ok(true)
    }

    /// Parse statement ID and parameters from EXECUTE frame
    fn parse_execute(&self, frame: &Envelope) -> Result<(Vec<u8>, Vec<serde_json::Value>)> {
        let body = &frame.body;
        if body.len() < 2 {
            return Err(anyhow::anyhow!("EXECUTE frame too short"));
        }

        // Read statement ID (short bytes)
        let id_len = u16::from_be_bytes([body[0], body[1]]) as usize;
        if body.len() < 2 + id_len {
            return Err(anyhow::anyhow!("EXECUTE frame truncated (statement ID)"));
        }

        let statement_id = body[2..2 + id_len].to_vec();
        let mut offset = 2 + id_len;

        // Parse query parameters (Phase 2: basic types only)
        // Skip consistency level (2 bytes)
        if body.len() < offset + 2 {
            return Err(anyhow::anyhow!("EXECUTE frame truncated (consistency)"));
        }
        offset += 2;

        // Skip flags (1 byte)
        if body.len() < offset + 1 {
            return Err(anyhow::anyhow!("EXECUTE frame truncated (flags)"));
        }
        let flags = body[offset];
        offset += 1;

        let mut params = Vec::new();

        // If VALUES flag is set, parse parameter values
        if flags & 0x01 != 0 {
            // Read parameter count (2 bytes)
            if body.len() < offset + 2 {
                return Err(anyhow::anyhow!("EXECUTE frame truncated (param count)"));
            }
            let param_count = u16::from_be_bytes([body[offset], body[offset + 1]]) as usize;
            offset += 2;

            // Parse each parameter (bytes or null)
            for _ in 0..param_count {
                if body.len() < offset + 4 {
                    return Err(anyhow::anyhow!("EXECUTE frame truncated (param length)"));
                }

                let param_len = i32::from_be_bytes([
                    body[offset],
                    body[offset + 1],
                    body[offset + 2],
                    body[offset + 3],
                ]);
                offset += 4;

                if param_len < 0 {
                    // Null value
                    params.push(serde_json::Value::Null);
                } else {
                    let param_len = param_len as usize;
                    if body.len() < offset + param_len {
                        return Err(anyhow::anyhow!("EXECUTE frame truncated (param data)"));
                    }

                    let param_bytes = &body[offset..offset + param_len];
                    // For Phase 2, treat all params as strings
                    let param_str = String::from_utf8_lossy(param_bytes).to_string();
                    params.push(json!(param_str));
                    offset += param_len;
                }
            }
        }

        Ok((statement_id, params))
    }

    /// Send RESULT (Prepared) response
    async fn send_prepared(
        &self,
        stream_id: i16,
        statement_id: Vec<u8>,
        columns: Vec<serde_json::Value>,
        params: Option<Vec<serde_json::Value>>,
        param_count: usize,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let mut body = Vec::new();

        // Result kind: Prepared (0x0004)
        body.extend_from_slice(&0x00000004u32.to_be_bytes());

        // Statement ID (short bytes)
        body.extend_from_slice(&(statement_id.len() as u16).to_be_bytes());
        body.extend_from_slice(&statement_id);

        // FIRST: Metadata for bound variables (parameters) - describes bind markers for EXECUTE
        let keyspace = b"netget";
        let table = b"data";

        // Flags: 0x0001 (GLOBAL_TABLES_SPEC)
        body.extend_from_slice(&0x00000001u32.to_be_bytes());

        // Parameter count
        body.extend_from_slice(&(param_count as u32).to_be_bytes());

        // Partition key count (protocol v4+)
        body.extend_from_slice(&0u32.to_be_bytes()); // No PK indexes (token routing not supported)
                                                     // Note: pk_indexes would follow here if pk_count > 0

        // Global keyspace and table for parameters
        body.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
        body.extend_from_slice(keyspace);
        body.extend_from_slice(&(table.len() as u16).to_be_bytes());
        body.extend_from_slice(table);

        // Parameter specifications
        for i in 0..param_count {
            let param_name = format!("param{}", i);
            let param_bytes = param_name.as_bytes();
            body.extend_from_slice(&(param_bytes.len() as u16).to_be_bytes());
            body.extend_from_slice(param_bytes);

            // Get type from params array if available, otherwise default to varchar
            let param_type = params
                .as_ref()
                .and_then(|p| p.get(i))
                .and_then(|param| param.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("varchar");

            // Type code
            let type_code: u16 = match param_type {
                "int" => 0x0009,
                "varchar" | "text" => 0x000D,
                "boolean" => 0x0004,
                _ => 0x000D, // Default to varchar
            };
            body.extend_from_slice(&type_code.to_be_bytes());
        }

        // SECOND: Result metadata - describes columns returned when this statement is executed
        // Uses ROWS metadata format (Section 4.2.5.2) - does NOT include pk_count
        // Flags: 0x0001 (GLOBAL_TABLES_SPEC)
        body.extend_from_slice(&0x00000001u32.to_be_bytes());

        // Column count
        body.extend_from_slice(&(columns.len() as u32).to_be_bytes());

        // NOTE: No pk_count field in result metadata (only in parameters metadata)

        // Global keyspace and table
        body.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
        body.extend_from_slice(keyspace);
        body.extend_from_slice(&(table.len() as u16).to_be_bytes());
        body.extend_from_slice(table);

        // Column specifications
        for col in columns {
            let name = col.get("name").and_then(|v| v.as_str()).unwrap_or("col");
            let col_type = col
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("varchar");

            // Column name
            let name_bytes = name.as_bytes();
            body.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
            body.extend_from_slice(name_bytes);

            // Column type (simple types only for Phase 2)
            let type_code: u16 = match col_type {
                "int" => 0x0009,
                "varchar" | "text" => 0x000D,
                "boolean" => 0x0004,
                _ => 0x000D, // Default to varchar
            };
            body.extend_from_slice(&type_code.to_be_bytes());
        }

        trace!(
            "PREPARED response body hex (first 200 bytes): {}",
            hex::encode(&body[..std::cmp::min(200, body.len())])
        );

        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::Result,
            body,
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace(format!(
            "Cassandra → RESULT (Prepared: {} params, {} bytes)",
            param_count,
            bytes.len()
        ));

        Ok(())
    }

    /// Send the ERROR frame that answers a failed LLM call.
    ///
    /// The native protocol has an ERROR opcode with a numeric code, and a driver surfaces it as
    /// an exception on the exact request that produced it - so unlike silence it cannot be
    /// mistaken for a slow query, and unlike a RESULT frame it cannot be mistaken for an empty
    /// result set, which in CQL means "no rows matched".
    ///
    /// 0x1001 `Overloaded` is the protocol's own "the coordinator cannot take this right now",
    /// and every driver treats it as retryable - so capacity exhaustion gets a signal the
    /// client already knows how to act on. Everything else is 0x0000 `Server error`.
    ///
    /// Note what is *not* used for the AUTH_RESPONSE stage: 0x0100 `Bad credentials`. The
    /// credentials were never examined, and saying they were bad both misattributes the
    /// failure and would make a driver stop retrying with correct ones. An ERROR frame of any
    /// code is a refusal - AUTH_SUCCESS is the only thing that authenticates - so this stays
    /// fail-closed either way.
    async fn send_llm_failure_error(
        &self,
        stream_id: i16,
        stage: &str,
        err: &anyhow::Error,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let overloaded = crate::llm::is_overload_error(err);
        let (code, label) = if overloaded {
            (CASSANDRA_ERROR_OVERLOADED, "Overloaded")
        } else {
            (CASSANDRA_ERROR_SERVER_ERROR, "Server error")
        };
        let message = crate::utils::WireFailure::classify(err).prefixed_text();
        Log::new(Some(status_tx)).warn(format!(
            "Cassandra connection {} answering {} with ERROR 0x{:04X} ({}): {}",
            connection_id, stage, code, label, message
        ));
        self.send_error(stream_id, code, &message, stream, status_tx)
            .await
    }

    /// Answer a stage the handler left unanswered.
    ///
    /// A no-answer is not permission. Silence used to fall through to the success frame for
    /// the stage - READY for STARTUP, an empty RESULT/Rows for QUERY and EXECUTE, a valid
    /// statement id for PREPARE - so an LLM outage, a handler that returned nothing, and a
    /// model that deliberately refused were indistinguishable on the wire, and two of the
    /// three granted the client exactly what it asked for.
    ///
    /// Every success frame here has to be produced by an explicit action; nothing synthesises
    /// one. An empty Rows result in particular is a statement about the data ("no rows
    /// matched") and must never be manufactured by a failure.
    ///
    /// The log carries `decision=fail_closed_no_answer`, distinct from
    /// `decision=fail_closed_llm_error` (backend failed) and `decision=model_reject` (the
    /// handler asked for an ERROR), because the CQL wire cannot tell the three apart.
    async fn send_no_answer_error(
        &self,
        stream_id: i16,
        stage: &str,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        Log::new(Some(status_tx)).warn(format!(
            "Cassandra connection {} decision=fail_closed_no_answer stage={}: handler produced \
             no recognised action, answering ERROR 0x{:04X} (Server error)",
            connection_id, stage, CASSANDRA_ERROR_SERVER_ERROR
        ));
        self.send_error(
            stream_id,
            CASSANDRA_ERROR_SERVER_ERROR,
            "netget: no handler answer for this request",
            stream,
            status_tx,
        )
        .await
    }

    /// Send the ERROR frame a handler explicitly asked for.
    ///
    /// Logged as `decision=model_reject` so a deliberate refusal is separable from the two
    /// failure paths above, which reach the same opcode.
    async fn send_model_error(
        &self,
        stream_id: i16,
        stage: &str,
        error_code: u32,
        message: &str,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        Log::new(Some(status_tx)).info(format!(
            "Cassandra connection {} decision=model_reject stage={}: ERROR 0x{:04X}",
            connection_id, stage, error_code
        ));
        self.send_error(stream_id, error_code, message, stream, status_tx)
            .await
    }

    /// Send ERROR response
    async fn send_error(
        &self,
        stream_id: i16,
        error_code: u32,
        message: &str,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let mut body = Vec::new();

        // Error code (4 bytes)
        body.extend_from_slice(&error_code.to_be_bytes());

        // Error message (string)
        let msg_bytes = message.as_bytes();
        body.extend_from_slice(&(msg_bytes.len() as u16).to_be_bytes());
        body.extend_from_slice(msg_bytes);

        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::Error,
            body,
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace(format!(
            "Cassandra → ERROR 0x{:04X} - {}",
            error_code, message
        ));

        Ok(())
    }

    /// Handle AUTH_RESPONSE frame (Phase 3)
    async fn handle_auth_response(
        &self,
        frame: Envelope,
        conn_state: &mut CassandraConnectionState,
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        debug!("Handling AUTH_RESPONSE from connection {}", connection_id);

        // Parse credentials from frame body (SASL PLAIN format: \0username\0password)
        let body = &frame.body;

        // Extract username and password from SASL PLAIN format
        let (username, password) = self.parse_sasl_plain(body)?;

        trace!("AUTH_RESPONSE: username={}", username);

        // Call LLM to decide whether to accept authentication
        let protocol =
            CassandraProtocol::new(connection_id, self.app_state.clone(), status_tx.clone());

        let event = Event {
            event_type: &CASSANDRA_AUTH_EVENT,
            data: json!({
                "username": username,
                "password": password,
            }),
        };

        let server_id = self.server_id.context("Server ID not set")?;

        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.send_llm_failure_error(
                    frame.stream_id,
                    "AUTH_RESPONSE",
                    &e,
                    stream,
                    connection_id,
                    status_tx,
                )
                .await?;
                return Ok(true);
            }
        };

        // Show messages
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        // Execute the protocol actions
        for action_result in execution_result.protocol_results {
            match action_result {
                ActionResult::Custom { name, data } => {
                    match name.as_str() {
                        "cassandra_auth_success" => {
                            conn_state.authenticated = true;
                            conn_state.username = Some(username);
                            self.send_auth_success(frame.stream_id, stream, status_tx)
                                .await?;
                            return Ok(true);
                        }
                        "cassandra_error" => {
                            let error_code =
                                data.get("error_code")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0x0000) as u32;
                            let message = data
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Unknown error");
                            self.send_model_error(
                                frame.stream_id,
                                "AUTH_RESPONSE",
                                error_code,
                                message,
                                stream,
                                connection_id,
                                status_tx,
                            )
                            .await?;
                            return Ok(false); // Close connection on auth failure
                        }
                        _ => {}
                    }
                }
                ActionResult::CloseConnection => {
                    return Ok(false);
                }
                _ => {
                    warn!("Unexpected action result for AUTH_RESPONSE");
                }
            }
        }

        // Default: deny authentication
        self.send_error(
            frame.stream_id,
            0x0100,
            "Authentication failed",
            stream,
            status_tx,
        )
        .await?;
        Ok(false)
    }

    /// Parse SASL PLAIN credentials (format: \0username\0password)
    fn parse_sasl_plain(&self, body: &[u8]) -> Result<(String, String)> {
        if body.is_empty() {
            return Err(anyhow::anyhow!("Empty AUTH_RESPONSE body"));
        }

        // Skip optional authorization identity (first \0-terminated string)
        let mut idx = 0;
        while idx < body.len() && body[idx] != 0 {
            idx += 1;
        }
        idx += 1; // Skip the \0

        if idx >= body.len() {
            return Err(anyhow::anyhow!("Invalid SASL PLAIN format"));
        }

        // Extract username
        let username_start = idx;
        while idx < body.len() && body[idx] != 0 {
            idx += 1;
        }
        let username = String::from_utf8_lossy(&body[username_start..idx]).to_string();
        idx += 1; // Skip the \0

        if idx >= body.len() {
            return Err(anyhow::anyhow!(
                "Invalid SASL PLAIN format - missing password"
            ));
        }

        // Extract password
        let password = String::from_utf8_lossy(&body[idx..]).to_string();

        Ok((username, password))
    }

    /// Send AUTHENTICATE response (request authentication)
    async fn send_authenticate(
        &self,
        stream_id: i16,
        authenticator: &str,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let mut body = Vec::new();

        // Authenticator name (string)
        let auth_bytes = authenticator.as_bytes();
        body.extend_from_slice(&(auth_bytes.len() as u16).to_be_bytes());
        body.extend_from_slice(auth_bytes);

        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::Authenticate,
            body,
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace(format!("Cassandra → AUTHENTICATE ({})", authenticator));

        Ok(())
    }

    /// Send AUTH_SUCCESS response
    async fn send_auth_success(
        &self,
        stream_id: i16,
        stream: &mut TcpStream,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // AUTH_SUCCESS body can contain optional token (we send empty for SASL PLAIN)
        let body = vec![];

        let response = Envelope {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags::empty(),
            stream_id: stream_id,
            opcode: Opcode::AuthSuccess,
            body,
            tracing_id: None,
            warnings: vec![],
        };

        let bytes = response.encode_with(Compression::None)?;
        stream.write_all(&bytes).await?;

        Log::new(Some(status_tx)).trace("Cassandra → AUTH_SUCCESS");

        Ok(())
    }
}
