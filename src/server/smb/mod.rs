//! SMB/CIFS server implementation
//!
//! Provides an SMB2 file server where the LLM controls the virtual filesystem.
//! Uses guest-only authentication (no password required).

pub mod actions;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::SmbProtocol;
use crate::state::app_state::AppState;
use crate::state::server::{
    ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
};
use crate::state::ServerId;

use crate::logging::emit::Log;
use actions::SMB_OPERATION_EVENT;

// NTSTATUS codes used in SMB2 response headers (MS-ERREF 2.3.1).
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_DATA_ERROR: u32 = 0xC000_003E;
/// "Insufficient system resources exist to complete the API." The closest NTSTATUS to
/// "retryable", used when the LLM failure was capacity exhaustion rather than a fault.
const STATUS_INSUFFICIENT_RESOURCES: u32 = 0xC000_009A;
/// "An internal error occurred." Used for every other LLM failure.
const STATUS_INTERNAL_ERROR: u32 = 0xC000_00E5;

// File attributes in the CREATE response (MS-SMB2 2.2.14 / MS-FSCC 2.6).
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

/// SMB server that provides LLM-controlled file system
pub struct SmbServer;

/// SMB2 session state
#[derive(Debug, Clone)]
struct SmbSession {
    session_id: u64,
    username: String,
    _authenticated: bool,
}

/// SMB2 tree connection state
#[derive(Debug, Clone)]
struct SmbTreeConnect {
    _tree_id: u32,
    _share_name: String,
}

/// SMB2 file handle state
#[derive(Debug, Clone)]
struct SmbFileHandle {
    _file_id: Vec<u8>, // 16-byte GUID
    path: String,
    _is_directory: bool,
}

/// Per-connection SMB state
struct SmbConnectionState {
    sessions: HashMap<u64, SmbSession>,
    trees: HashMap<u32, SmbTreeConnect>,
    files: HashMap<Vec<u8>, SmbFileHandle>,
    next_session_id: u64,
    next_tree_id: u32,
}

impl SmbConnectionState {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            trees: HashMap::new(),
            files: HashMap::new(),
            next_session_id: 1,
            next_tree_id: 1,
        }
    }
}

impl SmbServer {
    /// Spawn SMB server with integrated LLM actions
    #[cfg(feature = "smb")]
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
    ) -> Result<SocketAddr> {
        Log::new(Some(&status_tx)).info(format!(
            "SMB server (LLM-controlled, guest-only) starting on {}",
            listen_addr
        ));

        let protocol = Arc::new(SmbProtocol::new());

        // Bind TCP listener
        let listener = TcpListener::bind(listen_addr)
            .await
            .context("Failed to bind SMB TCP listener")?;

        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("SMB server listening on {}", actual_addr));

        // Spawn connection acceptor
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            info!("SMB server connection acceptor started");

            loop {
                trace!("SMB acceptor: waiting for connection");

                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        Log::new(Some(&status_tx))
                            .info(format!("SMB connection accepted from {}", peer_addr));

                        // Spawn per-connection handler
                        let llm_client = llm_client.clone();
                        let app_state = app_state.clone();
                        let protocol = protocol.clone();
                        let status_tx = status_tx.clone();

                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                stream,
                                peer_addr,
                                llm_client,
                                app_state,
                                server_id,
                                protocol,
                                status_tx.clone(),
                            )
                            .await
                            {
                                Log::new(Some(&status_tx)).error(format!(
                                    "SMB connection error from {}: {}",
                                    peer_addr, e
                                ));
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("SMB accept error: {}", e));
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(actual_addr)
    }

    /// Spawn SMB server without the smb feature (fallback)
    #[cfg(not(feature = "smb"))]
    pub async fn spawn_with_llm_actions(
        _listen_addr: SocketAddr,
        _llm_client: OllamaClient,
        _app_state: Arc<AppState>,
        _status_tx: mpsc::UnboundedSender<String>,
        _server_id: ServerId,
    ) -> Result<SocketAddr> {
        Err(anyhow!("SMB feature not enabled"))
    }

    /// Handle a single SMB connection
    #[cfg(feature = "smb")]
    async fn handle_connection(
        mut stream: TcpStream,
        peer_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        server_id: ServerId,
        protocol: Arc<SmbProtocol>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // Generate connection ID
        let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);

        Log::new(Some(&status_tx)).info(format!(
            "SMB connection {} from {}",
            connection_id, peer_addr
        ));

        // Get local address for tracking
        let local_addr = stream
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());

        // Track connection in app state
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr: peer_addr,
            local_addr,
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

        let state = Arc::new(Mutex::new(SmbConnectionState::new()));

        // SMB2 protocol handling loop
        loop {
            // Read SMB2 message
            // SMB2 header is 64 bytes minimum
            let mut header_buf = vec![0u8; 64];

            match stream.read_exact(&mut header_buf).await {
                Ok(_) => {
                    // Update connection stats for received data
                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            Some(header_buf.len() as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;

                    // Parse SMB2 header
                    if &header_buf[0..4] != b"\xFESMB" {
                        Log::new(Some(&status_tx))
                            .warn(format!("Invalid SMB2 signature from {}", peer_addr));
                        break;
                    }

                    // Extract command from header (offset 12-13, little-endian)
                    let command = u16::from_le_bytes([header_buf[12], header_buf[13]]);
                    debug!("SMB2 command 0x{:04x} from {}", command, peer_addr);

                    // Handle SMB2 command
                    let response = match Self::handle_smb2_command(
                        command,
                        &header_buf,
                        &mut stream,
                        &llm_client,
                        &app_state,
                        server_id,
                        connection_id,
                        &protocol,
                        &state,
                        &status_tx,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!("handle_smb2_command error for 0x{:04x}: {}", command, e);
                            break;
                        }
                    };

                    // Send response
                    if let Some(response_data) = response {
                        match stream.write_all(&response_data).await {
                            Ok(_) => {
                                trace!(
                                    "SMB2 response sent to {}, {} bytes",
                                    peer_addr,
                                    response_data.len()
                                );

                                // Update connection stats for sent data
                                app_state
                                    .update_connection_stats(
                                        server_id,
                                        connection_id,
                                        None,
                                        Some(response_data.len() as u64),
                                        None,
                                        Some(1),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                error!("Failed to send response for 0x{:04x}: {}", command, e);
                                break;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    Log::new(Some(&status_tx))
                        .info(format!("SMB client {} disconnected", peer_addr));
                    break;
                }
                Err(e) => {
                    error!("SMB read error from {}: {}", peer_addr, e);
                    break;
                }
            }
        }

        // Mark connection as closed
        app_state
            .update_connection_status(server_id, connection_id, ConnectionStatus::Closed)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        Log::new(Some(&status_tx)).info(format!("SMB connection {} closed", connection_id));

        Ok(())
    }

    /// Handle SMB2 command
    #[cfg(feature = "smb")]
    #[allow(clippy::too_many_arguments)]
    async fn handle_smb2_command(
        command: u16,
        _header: &[u8],
        _stream: &mut TcpStream,
        _llm_client: &OllamaClient,
        _app_state: &Arc<AppState>,
        _server_id: ServerId,
        _connection_id: ConnectionId,
        _protocol: &Arc<SmbProtocol>,
        _state: &Arc<Mutex<SmbConnectionState>>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Option<Vec<u8>>> {
        // SMB2 command codes
        const SMB2_NEGOTIATE: u16 = 0x0000;
        const SMB2_SESSION_SETUP: u16 = 0x0001;
        const SMB2_TREE_CONNECT: u16 = 0x0003;
        const SMB2_CREATE: u16 = 0x0005;
        const SMB2_CLOSE: u16 = 0x0006;
        const SMB2_READ: u16 = 0x0008;
        const SMB2_WRITE: u16 = 0x0009;
        const SMB2_QUERY_INFO: u16 = 0x0010;
        const SMB2_QUERY_DIRECTORY: u16 = 0x000E;

        match command {
            SMB2_NEGOTIATE => {
                Log::new(Some(status_tx)).debug("SMB2 NEGOTIATE request - offering SMB 2.1");

                // Consume NEGOTIATE request body from the stream
                // NEGOTIATE request body is 36 bytes (structure) + 2 bytes (dialect) = 38 bytes total
                // We read exactly 38 bytes to prevent consuming part of the next message
                let mut body_buf = [0u8; 38];
                match _stream.read_exact(&mut body_buf).await {
                    Ok(_) => {
                        debug!("NEGOTIATE body: 38 bytes consumed");
                    }
                    Err(e) => {
                        // ERROR: If we can't read the body, the stream is now out of sync!
                        // This will cause the next header read to fail
                        warn!("Error reading NEGOTIATE body: {}", e);
                        return Err(e.into());
                    }
                }

                // Build SMB2 Negotiate Response
                // For simplicity, we'll offer SMB 2.1 dialect (0x0210)
                let response = Self::build_negotiate_response(_header)?;
                Ok(Some(response))
            }
            SMB2_SESSION_SETUP => {
                debug!("SMB2 SESSION_SETUP request");

                // Read SESSION_SETUP request body (exactly 24 bytes for guest auth)
                let mut body_buf = [0u8; 24];
                if let Err(e) = _stream.read_exact(&mut body_buf).await {
                    warn!(
                        "Error reading SESSION_SETUP body: {} - continuing anyway",
                        e
                    );
                }
                let bytes_read = body_buf.len();

                // Try to extract username from security blob (simplified)
                // In real SMB2, this would be in the NTLMSSP blob
                // For simplicity, we'll check for a text username or use "guest"
                let username = Self::parse_smb2_username(&body_buf[..bytes_read])
                    .unwrap_or_else(|| "guest".to_string());

                Log::new(Some(status_tx))
                    .info(format!("SMB2 SESSION_SETUP for user: {}", username));

                // Consult LLM to check if this user should be authenticated
                let actions = match Self::consult_llm(
                    _llm_client,
                    _app_state,
                    _server_id,
                    _protocol,
                    "session_setup",
                    serde_json::json!({
                        "username": username,
                        "auth_type": if username == "guest" { "guest" } else { "password" }
                    }),
                    status_tx,
                )
                .await
                {
                    Ok(actions) => actions,
                    Err(e) => {
                        Log::new(Some(status_tx)).warn(format!(
                            "LLM error during SMB authentication for user {} - denying auth: {}",
                            username, e
                        ));

                        // Send AUTH_DENIED response instead of closing connection
                        let response = Self::build_auth_denied_response(_header)?;
                        return Ok(Some(response));
                    }
                };

                // Check if LLM allowed the authentication
                let auth_allowed = actions.iter().any(|a| {
                    a.get("type").and_then(|t| t.as_str()) == Some("smb_auth_success")
                        || a.get("type").and_then(|t| t.as_str()) == Some("allow_auth")
                });

                if !auth_allowed {
                    Log::new(Some(status_tx))
                        .warn(format!("SMB authentication denied for user: {}", username));

                    // Return ACCESS_DENIED response
                    let response = Self::build_auth_denied_response(_header)?;
                    return Ok(Some(response));
                }

                Log::new(Some(status_tx)).info(format!(
                    "SMB authentication successful for user: {}",
                    username
                ));

                // Build successful session setup response
                let response =
                    Self::build_session_setup_response_with_user(_header, _state, username.clone())
                        .await?;

                // Get the session info from state to update connection tracking
                let (session_id, _auth_username) = {
                    let s = _state.lock().await;
                    if let Some(session) = s.sessions.values().last() {
                        (Some(session.session_id), Some(session.username.clone()))
                    } else {
                        (None, None)
                    }
                };

                // TODO: Update connection tracking with authentication info
                // Note: update_connection_protocol_info method doesn't exist yet
                if let Some(sid) = session_id {
                    // Future: add method to update SMB connection state
                    // For now, connection is tracked with initial protocol info
                    let _ = status_tx.send("__UPDATE_UI__".to_string());

                    info!(
                        "SMB session {} established for connection {}",
                        sid, _connection_id
                    );
                }

                Ok(Some(response))
            }
            SMB2_TREE_CONNECT => {
                Log::new(Some(status_tx)).debug("SMB2 TREE_CONNECT request - accepting share");

                // For simplicity, accept any tree connect with share name "share"
                let response =
                    Self::build_tree_connect_response(_header, _state, "share".to_string()).await?;
                Ok(Some(response))
            }
            SMB2_CREATE => {
                debug!("SMB2 CREATE request");

                // Read CREATE request body (variable length)
                // Structure size is at offset 0-1 of body (should be 57)
                let mut body_buf = vec![0u8; 512]; // Sufficient for most paths
                let bytes_read = _stream.read(&mut body_buf).await?;

                // Extract file path from request (simplified parsing)
                // Path is UTF-16LE encoded starting at offset 120 in the CREATE request
                let path = Self::parse_smb2_path(&body_buf[..bytes_read])
                    .unwrap_or_else(|| "/unknown".to_string());

                Log::new(Some(status_tx)).info(format!("SMB2 CREATE request for: {}", path));

                // Consult LLM to check if file exists and get info. An LLM failure must
                // answer in SMB2, not drop the connection: a client that gets no reply
                // hangs until its own timeout with no way to tell an outage from a
                // black hole.
                let actions = match Self::consult_llm(
                    _llm_client,
                    _app_state,
                    _server_id,
                    _protocol,
                    "create",
                    serde_json::json!({
                        "path": path,
                        "operation": "open_or_create"
                    }),
                    status_tx,
                )
                .await
                {
                    Ok(actions) => actions,
                    Err(e) => {
                        return Ok(Some(Self::llm_failure_response(
                            _header,
                            SMB2_CREATE,
                            "CREATE",
                            &path,
                            &e,
                            status_tx,
                        )?));
                    }
                };

                // Opening a handle is an access decision, so it takes an affirmative answer.
                // The model's vocabulary for CREATE is exactly `smb_create_file` and
                // `smb_create_directory` (see the prompt in actions.rs); which one it picks
                // also reaches the wire, because FILE_ATTRIBUTE_DIRECTORY in the CREATE
                // response is what makes a client issue QUERY_DIRECTORY instead of READ.
                //
                // Neither action present used to mean "regular file", so an answer carrying
                // no create action at all — a model that refused, a static handler with an
                // empty list, an unparseable reply that still deserialised — handed the peer
                // STATUS_SUCCESS and a live file handle. That is the fail-open shape: silence
                // became consent for an admission decision. Refuse instead.
                let action_type = |a: &serde_json::Value| {
                    a.get("type").and_then(|t| t.as_str()).map(String::from)
                };
                let is_directory = actions
                    .iter()
                    .any(|a| action_type(a).as_deref() == Some("smb_create_directory"));
                let is_file = actions
                    .iter()
                    .any(|a| action_type(a).as_deref() == Some("smb_create_file"));
                if !is_directory && !is_file {
                    // Kept apart in the log because the wire cannot carry the difference:
                    // both answers deny the handle, but only one of them is a decision.
                    let decision = if actions.is_empty() {
                        "fail_closed_no_action"
                    } else {
                        "model_reject"
                    };
                    Log::new(Some(status_tx)).warn(format!(
                        "SMB2 CREATE refused for {} (decision={}): no smb_create_file or \
                         smb_create_directory in the answer; replying STATUS_ACCESS_DENIED",
                        path, decision
                    ));
                    let response =
                        Self::build_error_response(_header, SMB2_CREATE, STATUS_ACCESS_DENIED)?;
                    return Ok(Some(response));
                }

                // Generate file handle (16-byte GUID)
                let file_id = Self::generate_file_handle();

                // Store file handle in state
                {
                    let mut s = _state.lock().await;
                    s.files.insert(
                        file_id.clone(),
                        SmbFileHandle {
                            _file_id: file_id.clone(),
                            path: path.clone(),
                            _is_directory: is_directory,
                        },
                    );
                }

                debug!(
                    "SMB2 CREATE: allocated {} handle for {}",
                    if is_directory { "directory" } else { "file" },
                    path
                );
                let response = Self::build_create_response(_header, &file_id, is_directory)?;
                Ok(Some(response))
            }
            SMB2_CLOSE => {
                debug!("SMB2 CLOSE request");

                // Read CLOSE request body
                let mut body_buf = vec![0u8; 24]; // CLOSE body is 24 bytes
                _stream.read_exact(&mut body_buf).await?;

                // Extract file ID (16 bytes at offset 8)
                let file_id = body_buf[8..24].to_vec();

                // Remove file handle from state
                let path = {
                    let mut s = _state.lock().await;
                    s.files.remove(&file_id).map(|h| h.path)
                };

                if let Some(path) = path {
                    Log::new(Some(status_tx)).info(format!("SMB2 CLOSE: {}", path));
                } else {
                    Log::new(Some(status_tx)).warn("SMB2 CLOSE: unknown file handle");
                }

                let response = Self::build_close_response(_header)?;
                Ok(Some(response))
            }
            SMB2_READ => {
                debug!("SMB2 READ request");

                // Read READ request body (49 bytes)
                let mut body_buf = vec![0u8; 49];
                _stream.read_exact(&mut body_buf).await?;

                // Extract file ID (16 bytes at offset 16)
                let file_id = body_buf[16..32].to_vec();

                // Extract read offset and length
                let offset = u64::from_le_bytes(body_buf[8..16].try_into().unwrap());
                let length = u32::from_le_bytes(body_buf[4..8].try_into().unwrap());

                // Look up file path from handle
                let path = {
                    let s = _state.lock().await;
                    s.files.get(&file_id).map(|h| h.path.clone())
                };

                let path = path.unwrap_or_else(|| "/unknown".to_string());
                Log::new(Some(status_tx)).info(format!(
                    "SMB2 READ: {} (offset={}, length={})",
                    path, offset, length
                ));

                // Consult LLM for file content. On an LLM failure the READ is refused
                // with an NTSTATUS rather than answered with invented content.
                let actions = match Self::consult_llm(
                    _llm_client,
                    _app_state,
                    _server_id,
                    _protocol,
                    "read",
                    serde_json::json!({
                        "path": path,
                        "offset": offset,
                        "length": length
                    }),
                    status_tx,
                )
                .await
                {
                    Ok(actions) => actions,
                    Err(e) => {
                        return Ok(Some(Self::llm_failure_response(
                            _header, SMB2_READ, "READ", &path, &e, status_tx,
                        )?));
                    }
                };

                // Extract file content from the LLM response, honouring the action's
                // `encoding` field. Decoding is explicit: `content` is only base64 or hex
                // when the action says so, because "SGVsbG8=" is simultaneously valid text
                // and valid base64 and only the sender knows which it means.
                let read_action = actions
                    .iter()
                    .find(|a| a.get("type").and_then(|t| t.as_str()) == Some("smb_read_file"));

                let content = match read_action {
                    Some(action) => {
                        let payload = action
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or_default();
                        let encoding = action.get("encoding").and_then(|e| e.as_str());
                        match crate::server::smb::actions::decode_smb_payload(payload, encoding) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                // Refuse rather than putting the undecodable string on the
                                // wire, which is exactly the failure this field exists to
                                // prevent.
                                Log::new(Some(status_tx)).warn(format!(
                                    "SMB read: {} - refusing with STATUS_DATA_ERROR",
                                    e
                                ));
                                let response = Self::build_error_response(
                                    _header,
                                    SMB2_READ,
                                    STATUS_DATA_ERROR,
                                )?;
                                return Ok(Some(response));
                            }
                        }
                    }
                    None => {
                        // No `smb_read_file` in the answer. Returning STATUS_SUCCESS with the
                        // literal bytes "File not found or empty" told the client the read
                        // succeeded and that those 23 bytes are the file's contents — a
                        // successful read of fabricated data, and indistinguishable from a
                        // file that genuinely holds that text. Refuse instead; the client can
                        // tell a refusal from content.
                        let decision = if actions.is_empty() {
                            "fail_closed_no_action"
                        } else {
                            "model_reject"
                        };
                        Log::new(Some(status_tx)).warn(format!(
                            "SMB2 READ refused for {} (decision={}): no smb_read_file in the \
                             answer; replying STATUS_ACCESS_DENIED",
                            path, decision
                        ));
                        let response =
                            Self::build_error_response(_header, SMB2_READ, STATUS_ACCESS_DENIED)?;
                        return Ok(Some(response));
                    }
                };

                debug!("SMB2 READ: returning {} bytes for {}", content.len(), path);
                let response = Self::build_read_response(_header, &content)?;
                Ok(Some(response))
            }
            SMB2_WRITE => {
                debug!("SMB2 WRITE request");

                // Read WRITE request body (49 bytes + data)
                let mut body_buf = vec![0u8; 49];
                _stream.read_exact(&mut body_buf).await?;

                // Extract file ID (16 bytes at offset 16)
                let file_id = body_buf[16..32].to_vec();

                // Extract write offset and length.
                //
                // MS-SMB2 2.2.21: StructureSize(2) DataOffset(2) Length(4) Offset(8)
                // FileId(16) ... so Length lives at body offset 4, not 0. Reading it from
                // 0 picked up StructureSize+DataOffset (0x00700031 for a well-formed
                // request) and then blocked in read_exact waiting for 7 MB that never
                // arrived, hanging the connection on the first WRITE.
                let length = u32::from_le_bytes(body_buf[4..8].try_into().unwrap());
                let offset = u64::from_le_bytes(body_buf[8..16].try_into().unwrap());

                // `length` is attacker-controlled, so cap the allocation instead of
                // trusting a peer to be honest about a 4 GB write.
                const MAX_WRITE_LEN: u32 = 8 * 1024 * 1024;
                if length > MAX_WRITE_LEN {
                    Log::new(Some(status_tx)).warn(format!(
                        "SMB2 WRITE: refusing {} byte write (max {})",
                        length, MAX_WRITE_LEN
                    ));
                    let response =
                        Self::build_error_response(_header, SMB2_WRITE, STATUS_ACCESS_DENIED)?;
                    return Ok(Some(response));
                }

                // Read data to write (variable length)
                let mut data = vec![0u8; length as usize];
                _stream.read_exact(&mut data).await?;

                // Look up file path from handle
                let path = {
                    let s = _state.lock().await;
                    s.files.get(&file_id).map(|h| h.path.clone())
                };

                let path = path.unwrap_or_else(|| "/unknown".to_string());
                Log::new(Some(status_tx)).info(format!(
                    "SMB2 WRITE: {} (offset={}, length={})",
                    path, offset, length
                ));

                // Render the written bytes for the model. Printable payloads stay readable;
                // anything else is base64 rather than `from_utf8_lossy`, which silently
                // replaced every non-UTF-8 byte with U+FFFD and made the round trip
                // impossible. `encoding` says which of the two the model is looking at, and
                // matches what smb_read_file accepts, so the model can hand the same bytes
                // back on a later read.
                let (content, data_encoding) =
                    crate::server::smb::actions::encode_smb_payload(&data);

                // Consult the LLM. The write is refused unless the model returns
                // smb_write_file: an LLM outage or a model that says nothing must not read
                // as an approval. An outage is reported as its own NTSTATUS so it stays
                // distinguishable from the model's explicit denial (ACCESS_DENIED).
                let actions = match Self::consult_llm(
                    _llm_client,
                    _app_state,
                    _server_id,
                    _protocol,
                    "write",
                    serde_json::json!({
                        "path": path,
                        "offset": offset,
                        "data": content,
                        "encoding": data_encoding
                    }),
                    status_tx,
                )
                .await
                {
                    Ok(actions) => actions,
                    Err(e) => {
                        return Ok(Some(Self::llm_failure_response(
                            _header, SMB2_WRITE, "WRITE", &path, &e, status_tx,
                        )?));
                    }
                };

                let write_action = actions
                    .iter()
                    .find(|a| a.get("type").and_then(|t| t.as_str()) == Some("smb_write_file"));

                let Some(write_action) = write_action else {
                    Log::new(Some(status_tx)).warn(format!(
                        "SMB2 WRITE: no smb_write_file action for {} - refusing with \
                         STATUS_ACCESS_DENIED",
                        path
                    ));
                    let response =
                        Self::build_error_response(_header, SMB2_WRITE, STATUS_ACCESS_DENIED)?;
                    return Ok(Some(response));
                };

                // The model may report a short write; clamp to what the client actually sent.
                let bytes_written = write_action
                    .get("bytes_written")
                    .and_then(|v| v.as_u64())
                    .map(|v| v.min(length as u64) as u32)
                    .unwrap_or(length);

                debug!(
                    "SMB2 WRITE: accepted {} of {} bytes to {}",
                    bytes_written, length, path
                );
                let response = Self::build_write_response(_header, bytes_written)?;
                Ok(Some(response))
            }
            SMB2_QUERY_INFO => {
                debug!("SMB2 QUERY_INFO request");

                // Read QUERY_INFO request body (variable length)
                let mut body_buf = vec![0u8; 256];
                let bytes_read = _stream.read(&mut body_buf).await?;

                // Extract file ID (16 bytes at offset 16)
                if bytes_read >= 32 {
                    let file_id = body_buf[16..32].to_vec();

                    // Look up file path
                    let path = {
                        let s = _state.lock().await;
                        s.files.get(&file_id).map(|h| h.path.clone())
                    };

                    let path = path.unwrap_or_else(|| "/unknown".to_string());
                    Log::new(Some(status_tx)).info(format!("SMB2 QUERY_INFO: {}", path));

                    // Consult LLM for file info. On an LLM failure the client is told the
                    // query failed rather than being handed the 4096-byte default as if
                    // the model had answered.
                    let actions = match Self::consult_llm(
                        _llm_client,
                        _app_state,
                        _server_id,
                        _protocol,
                        "query_info",
                        serde_json::json!({
                            "path": path
                        }),
                        status_tx,
                    )
                    .await
                    {
                        Ok(actions) => actions,
                        Err(e) => {
                            return Ok(Some(Self::llm_failure_response(
                                _header,
                                SMB2_QUERY_INFO,
                                "QUERY_INFO",
                                &path,
                                &e,
                                status_tx,
                            )?));
                        }
                    };

                    // Extract file info from LLM response (or use defaults)
                    let size = actions
                        .iter()
                        .find(|a| {
                            a.get("type").and_then(|t| t.as_str()) == Some("smb_get_file_info")
                        })
                        .and_then(|a| a.get("size"))
                        .and_then(|s| s.as_u64())
                        .unwrap_or(4096);

                    let response = Self::build_query_info_response(_header, size)?;
                    Ok(Some(response))
                } else {
                    warn!("SMB2 QUERY_INFO: invalid request size");
                    Ok(None)
                }
            }
            SMB2_QUERY_DIRECTORY => {
                debug!("SMB2 QUERY_DIRECTORY request");

                // Read QUERY_DIRECTORY request body (variable length)
                let mut body_buf = vec![0u8; 512];
                let bytes_read = _stream.read(&mut body_buf).await?;

                // Extract file ID (directory handle, 16 bytes at offset 8)
                if bytes_read >= 24 {
                    let file_id = body_buf[8..24].to_vec();

                    // Look up directory path
                    let path = {
                        let s = _state.lock().await;
                        s.files.get(&file_id).map(|h| h.path.clone())
                    };

                    let path = path.unwrap_or_else(|| "/".to_string());
                    Log::new(Some(status_tx)).info(format!("SMB2 QUERY_DIRECTORY: {}", path));

                    // Consult LLM for directory listing. On an LLM failure the client is
                    // told the enumeration failed rather than being handed an empty
                    // listing, which reads as "the directory is empty".
                    let actions = match Self::consult_llm(
                        _llm_client,
                        _app_state,
                        _server_id,
                        _protocol,
                        "query_directory",
                        serde_json::json!({
                            "path": path
                        }),
                        status_tx,
                    )
                    .await
                    {
                        Ok(actions) => actions,
                        Err(e) => {
                            return Ok(Some(Self::llm_failure_response(
                                _header,
                                SMB2_QUERY_DIRECTORY,
                                "QUERY_DIRECTORY",
                                &path,
                                &e,
                                status_tx,
                            )?));
                        }
                    };

                    // Extract file list from LLM response
                    let files = actions
                        .iter()
                        .find(|a| {
                            a.get("type").and_then(|t| t.as_str()) == Some("smb_list_directory")
                        })
                        .and_then(|a| a.get("files"))
                        .and_then(|f| f.as_array())
                        .cloned()
                        .unwrap_or_default();

                    debug!("SMB2 QUERY_DIRECTORY: returning {} files", files.len());
                    let response = Self::build_query_directory_response(_header, &files)?;
                    Ok(Some(response))
                } else {
                    warn!("SMB2 QUERY_DIRECTORY: invalid request size");
                    Ok(None)
                }
            }
            _ => {
                Log::new(Some(status_tx)).warn(format!("Unknown SMB2 command: 0x{:04x}", command));
                Ok(None)
            }
        }
    }

    /// Consult the LLM for SMB file system operations
    #[cfg(feature = "smb")]
    async fn consult_llm(
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        server_id: ServerId,
        protocol: &Arc<SmbProtocol>,
        operation: &str,
        params: serde_json::Value,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Vec<serde_json::Value>> {
        Log::new(Some(status_tx)).debug(format!(
            "Consulting LLM for SMB {} operation: {:?}",
            operation, params
        ));

        // Create SMB operation event
        // Extract path from params if available, otherwise use empty string
        let path = params.get("path").and_then(|p| p.as_str()).unwrap_or("");

        let mut event_data = serde_json::json!({
            "operation": operation,
        });

        // Add path if it's not empty
        if !path.is_empty() {
            event_data["path"] = serde_json::json!(path);
        }

        // Add all params as additional fields for the LLM context
        if let Some(obj) = params.as_object() {
            for (key, value) in obj {
                if key != "operation" && key != "path" {
                    event_data[key] = value.clone();
                }
            }
        }

        let event = Event::new(&SMB_OPERATION_EVENT, event_data);

        Log::new(Some(status_tx)).trace(format!("Calling LLM for SMB {} operation", operation));

        // Call LLM with Event-based approach
        let execution_result = call_llm(
            llm_client,
            app_state,
            server_id,
            None, // SMB doesn't use connection-specific context yet
            &event,
            protocol.as_ref(),
        )
        .await?;

        // Display messages from LLM
        for message in &execution_result.messages {
            Log::new(Some(status_tx)).info(format!("{}", message));
        }

        debug!(
            "LLM returned {} actions for SMB {}",
            execution_result.raw_actions.len(),
            operation
        );

        // Return raw actions for manual processing
        Ok(execution_result.raw_actions)
    }

    /// Build SMB2 Negotiate Response
    /// Simplified implementation - offers SMB 2.1 dialect (0x0210)
    #[cfg(feature = "smb")]
    fn build_negotiate_response(request_header: &[u8]) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header (64 bytes)
        response.extend_from_slice(b"\xFESMB"); // Protocol ID
        response.extend_from_slice(&[64, 0]); // Structure size (64 bytes)
        response.extend_from_slice(&[0, 0]); // Credit charge
        response.extend_from_slice(&[0, 0, 0, 0]); // Status (STATUS_SUCCESS)
        response.extend_from_slice(&[0x00, 0x00]); // Command (NEGOTIATE)
        response.extend_from_slice(&[1, 0]); // Credit (grant 1 credit)
        response.extend_from_slice(&[0, 0, 0, 0]); // Flags

        // Copy message ID from request (offset 24-31)
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]); // Reserved (process ID)
        response.extend_from_slice(&[0; 8]); // Tree ID
        response.extend_from_slice(&[0; 16]); // Session ID + Signature

        // SMB2 Negotiate Response body
        response.extend_from_slice(&[65, 0]); // Structure size (65 bytes)
        response.extend_from_slice(&[0, 0]); // Security mode
        response.extend_from_slice(&[0x10, 0x02]); // Dialect revision (SMB 2.1 = 0x0210)
        response.extend_from_slice(&[0, 0]); // Negotiate context count

        // Server GUID (16 bytes) - fixed for simplicity
        response.extend_from_slice(&[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF,
        ]);

        response.extend_from_slice(&[0x07, 0x00, 0x00, 0x00]); // Capabilities (DFS)
        response.extend_from_slice(&[0x00, 0x00, 0x10, 0x00]); // Max transaction size
        response.extend_from_slice(&[0x00, 0x00, 0x10, 0x00]); // Max read size
        response.extend_from_slice(&[0x00, 0x00, 0x10, 0x00]); // Max write size

        // System time (current time in Windows FILETIME format)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let filetime = (now / 100) + 116444736000000000; // Convert to FILETIME
        response.extend_from_slice(&filetime.to_le_bytes());

        response.extend_from_slice(&filetime.to_le_bytes()); // Server start time (same)
        response.extend_from_slice(&[0; 2]); // Security buffer offset (0 = no security)
        response.extend_from_slice(&[0; 2]); // Security buffer length

        response.extend_from_slice(&[0; 4]); // Negotiate context offset

        Ok(response)
    }

    /// Build SMB2 Tree Connect Response
    /// Accepts all tree connects
    #[cfg(feature = "smb")]
    async fn build_tree_connect_response(
        request_header: &[u8],
        state: &Arc<Mutex<SmbConnectionState>>,
        share_name: String,
    ) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // Allocate tree ID
        let tree_id = {
            // `.lock().await`, never `blocking_lock()`: this runs inside the connection
            // task, and tokio's Mutex::blocking_lock panics when called from a runtime
            // thread. See the note on build_session_setup_response_with_user.
            let mut s = state.lock().await;
            let tid = s.next_tree_id;
            s.next_tree_id += 1;

            s.trees.insert(
                tid,
                SmbTreeConnect {
                    _tree_id: tid,
                    _share_name: share_name,
                },
            );
            tid
        };

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x03, 0x00]); // Command (TREE_CONNECT)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&tree_id.to_le_bytes()); // Tree ID
        response.extend_from_slice(&[0; 8]); // Session ID (should copy from request)
        response.extend_from_slice(&[0; 16]); // Signature

        // Tree Connect Response body
        response.extend_from_slice(&[16, 0]); // Structure size
        response.extend_from_slice(&[1]); // Share type (disk)
        response.extend_from_slice(&[0]); // Reserved
        response.extend_from_slice(&[0; 4]); // Share flags
        response.extend_from_slice(&[0; 4]); // Capabilities
        response.extend_from_slice(&[0x01, 0xF0, 0x1F, 0x00]); // Max access rights

        Ok(response)
    }

    /// Parse SMB2 file path from CREATE request
    /// Simplified parser - looks for UTF-16LE encoded path
    #[cfg(feature = "smb")]
    fn parse_smb2_path(body: &[u8]) -> Option<String> {
        // MS-SMB2 2.2.13, CREATE request body (`body` starts after the 64-byte header):
        //   44..46  NameOffset  - offset of the name from the start of the SMB2 *header*
        //   46..48  NameLength  - length of the name in bytes
        //   56..    Buffer      - where NameOffset normally points (64 + 56 = 120)
        //
        // The name must be located through those two fields. This used to index the
        // body-relative slice at 120, which is the *absolute* offset of the buffer: for a
        // well-formed request from a real client that lands 64 bytes past the name, so
        // every CREATE resolved to "/unknown" and every subsequent READ/WRITE on the
        // handle carried the wrong path to the model.
        const FIXED_BODY_LEN: usize = 56;
        const HEADER_LEN: usize = 64;

        if body.len() < FIXED_BODY_LEN {
            return None;
        }

        let name_offset = u16::from_le_bytes([body[44], body[45]]) as usize;
        let name_length = u16::from_le_bytes([body[46], body[47]]) as usize;

        // Convert the header-relative offset to one within `body`, defaulting to the
        // start of the buffer when the client left NameOffset zero.
        let start = if name_offset >= HEADER_LEN {
            name_offset - HEADER_LEN
        } else {
            FIXED_BODY_LEN
        };

        if name_length == 0 || name_length % 2 != 0 {
            return None;
        }
        let end = start.checked_add(name_length)?;
        if end > body.len() {
            return None;
        }

        let utf16_chars: Vec<u16> = body[start..end]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            // A name is not null-terminated on the wire, but tolerate a client that
            // includes the terminator in NameLength.
            .take_while(|&c| c != 0)
            .collect();

        String::from_utf16(&utf16_chars).ok()
    }

    /// Generate a 16-byte file handle (GUID)
    #[cfg(feature = "smb")]
    fn generate_file_handle() -> Vec<u8> {
        use std::time::SystemTime;

        // Simple file handle generation using timestamp + random-ish data
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        let mut handle = Vec::with_capacity(16);
        handle.extend_from_slice(&now.to_le_bytes());
        handle.extend_from_slice(&(now.wrapping_mul(0x123456789ABCDEF)).to_le_bytes());
        handle
    }

    /// Build SMB2 CREATE Response
    #[cfg(feature = "smb")]
    fn build_create_response(
        request_header: &[u8],
        file_id: &[u8],
        is_directory: bool,
    ) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x05, 0x00]); // Command (CREATE)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags (response)

        // Copy message ID from request
        response.extend_from_slice(&request_header[24..32]);

        // Copy tree ID and session ID from request (should parse properly)
        response.extend_from_slice(&[0; 8]); // Reserved
        response.extend_from_slice(&[1, 0, 0, 0]); // Tree ID
        response.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // Session ID
        response.extend_from_slice(&[0; 16]); // Signature

        // CREATE Response body (89 bytes)
        response.extend_from_slice(&[89, 0]); // Structure size
        response.extend_from_slice(&[0]); // Oplock level (none)
        response.extend_from_slice(&[0]); // Flags
        response.extend_from_slice(&[0, 0, 0, 0]); // Create action (file opened)

        // Timestamps (all zeros for simplicity)
        response.extend_from_slice(&[0; 8]); // Creation time
        response.extend_from_slice(&[0; 8]); // Last access time
        response.extend_from_slice(&[0; 8]); // Last write time
        response.extend_from_slice(&[0; 8]); // Change time

        response.extend_from_slice(&[0; 8]); // Allocation size
        response.extend_from_slice(&[0, 0x10, 0, 0, 0, 0, 0, 0]); // End of file (4096 bytes)

        // File attributes (MS-SMB2 2.2.14): FILE_ATTRIBUTE_DIRECTORY (0x10) or
        // FILE_ATTRIBUTE_NORMAL (0x80). This is the field a client reads to decide
        // whether to follow up with QUERY_DIRECTORY or READ.
        let file_attributes: u32 = if is_directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        response.extend_from_slice(&file_attributes.to_le_bytes());

        response.extend_from_slice(&[0; 4]); // Reserved

        // File ID (16 bytes - our handle)
        response.extend_from_slice(file_id);

        response.extend_from_slice(&[0; 4]); // Create contexts offset
        response.extend_from_slice(&[0; 4]); // Create contexts length
        response.push(0); // Buffer - StructureSize 89 counts one byte of it

        Ok(response)
    }

    /// Build SMB2 CLOSE Response
    #[cfg(feature = "smb")]
    fn build_close_response(request_header: &[u8]) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x06, 0x00]); // Command (CLOSE)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&[1, 0, 0, 0]); // Tree ID
        response.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // Session ID
        response.extend_from_slice(&[0; 16]);

        // CLOSE Response body (60 bytes)
        response.extend_from_slice(&[60, 0]); // Structure size
        response.extend_from_slice(&[0, 0]); // Flags
        response.extend_from_slice(&[0; 4]); // Reserved

        // Timestamps (all zeros)
        response.extend_from_slice(&[0; 8]); // Creation time
        response.extend_from_slice(&[0; 8]); // Last access
        response.extend_from_slice(&[0; 8]); // Last write
        response.extend_from_slice(&[0; 8]); // Change time

        response.extend_from_slice(&[0; 8]); // Allocation size
        response.extend_from_slice(&[0; 8]); // End of file
        response.extend_from_slice(&[0; 4]); // File attributes

        Ok(response)
    }

    /// Build SMB2 READ Response
    #[cfg(feature = "smb")]
    fn build_read_response(request_header: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x08, 0x00]); // Command (READ)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&[1, 0, 0, 0]); // Tree ID
        response.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // Session ID
        response.extend_from_slice(&[0; 16]);

        // READ Response body (MS-SMB2 2.2.20): StructureSize(2) DataOffset(1)
        // Reserved(1) DataLength(4) DataRemaining(4) Reserved2(4) = 16 bytes, then
        // the payload. DataOffset is measured from the start of the SMB2 header, so
        // 64 + 16 = 80 = 0x50.
        //
        // This used to write four extra Reserved bytes after DataOffset, putting the
        // payload at 84 while still advertising 80 - so a client reading at the offset
        // the server itself declared got four bytes of zero padding followed by a
        // truncated file.
        response.extend_from_slice(&[17, 0]); // StructureSize
        response.push(0x50); // DataOffset (1 byte)
        response.push(0); // Reserved
        let data_len = data.len() as u32;
        response.extend_from_slice(&data_len.to_le_bytes()); // DataLength
        response.extend_from_slice(&[0; 4]); // DataRemaining
        response.extend_from_slice(&[0; 4]); // Reserved2
        debug_assert_eq!(response.len(), 80, "READ payload must start at DataOffset");

        // Data (variable length)
        response.extend_from_slice(data);

        Ok(response)
    }

    /// Build SMB2 WRITE Response
    #[cfg(feature = "smb")]
    fn build_write_response(request_header: &[u8], bytes_written: u32) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x09, 0x00]); // Command (WRITE)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&[1, 0, 0, 0]); // Tree ID
        response.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // Session ID
        response.extend_from_slice(&[0; 16]);

        // WRITE Response body (17 bytes)
        response.extend_from_slice(&[17, 0]); // Structure size
        response.extend_from_slice(&[0, 0]); // Reserved
        response.extend_from_slice(&bytes_written.to_le_bytes()); // Count (bytes written)
        response.extend_from_slice(&[0; 4]); // Remaining
        response.extend_from_slice(&[0, 0]); // Write channel info offset
        response.extend_from_slice(&[0, 0]); // Write channel info length
        response.push(0); // Buffer - StructureSize 17 counts one byte of it

        Ok(response)
    }

    /// Build SMB2 QUERY_INFO Response
    #[cfg(feature = "smb")]
    fn build_query_info_response(request_header: &[u8], file_size: u64) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x10, 0x00]); // Command (QUERY_INFO)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&[1, 0, 0, 0]); // Tree ID
        response.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // Session ID
        response.extend_from_slice(&[0; 16]);

        // QUERY_INFO Response body (9 bytes + data)
        response.extend_from_slice(&[9, 0]); // Structure size
        response.extend_from_slice(&[0x48, 0]); // Output buffer offset (72 bytes from start)
        let info_size = 96u32; // FILE_ALL_INFORMATION size
        response.extend_from_slice(&info_size.to_le_bytes()); // Output buffer length

        // FILE_ALL_INFORMATION structure (simplified)
        // Creation time, access time, write time, change time (all zeros)
        response.extend_from_slice(&[0; 32]);
        // File attributes (normal file)
        response.extend_from_slice(&[0x80, 0, 0, 0]);
        // Reserved
        response.extend_from_slice(&[0; 4]);
        // Allocation size
        response.extend_from_slice(&file_size.to_le_bytes());
        // End of file (actual size)
        response.extend_from_slice(&file_size.to_le_bytes());
        // Number of links
        response.extend_from_slice(&[1, 0, 0, 0]);
        // Delete pending
        response.extend_from_slice(&[0]);
        // Is directory
        response.extend_from_slice(&[0]);
        // Reserved
        response.extend_from_slice(&[0; 2]);
        // File name length and name (empty for now)
        response.extend_from_slice(&[0; 44]); // Padding to reach 96 bytes

        Ok(response)
    }

    /// Build SMB2 QUERY_DIRECTORY Response
    #[cfg(feature = "smb")]
    fn build_query_directory_response(
        request_header: &[u8],
        files: &[serde_json::Value],
    ) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x0E, 0x00]); // Command (QUERY_DIRECTORY)
        response.extend_from_slice(&[1, 0]);
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&[1, 0, 0, 0]); // Tree ID
        response.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // Session ID
        response.extend_from_slice(&[0; 16]);

        // Build directory entries (simplified - just returns file names)
        let mut entries = Vec::new();

        for file in files {
            let name = file
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown.txt");
            let size = file.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            let is_dir = file
                .get("is_directory")
                .and_then(|d| d.as_bool())
                .unwrap_or(false);

            // FILE_DIRECTORY_INFORMATION entry
            let mut entry = Vec::new();
            entry.extend_from_slice(&[0; 4]); // Next entry offset (0 = last)
            entry.extend_from_slice(&[0; 4]); // File index
            entry.extend_from_slice(&[0; 32]); // Timestamps
            entry.extend_from_slice(&size.to_le_bytes()); // End of file
            entry.extend_from_slice(&size.to_le_bytes()); // Allocation size

            // File attributes
            let attrs = if is_dir { 0x10u32 } else { 0x80u32 };
            entry.extend_from_slice(&attrs.to_le_bytes());

            // File name (UTF-16LE)
            let name_utf16: Vec<u16> = name.encode_utf16().collect();
            let name_bytes = (name_utf16.len() * 2) as u32;
            entry.extend_from_slice(&name_bytes.to_le_bytes());

            // Convert UTF-16 to bytes
            for ch in name_utf16 {
                entry.extend_from_slice(&ch.to_le_bytes());
            }

            entries.extend_from_slice(&entry);
        }

        // QUERY_DIRECTORY Response body (9 bytes + entries)
        response.extend_from_slice(&[9, 0]); // Structure size
        response.extend_from_slice(&[0x48, 0]); // Output buffer offset
        let entries_len = entries.len() as u32;
        response.extend_from_slice(&entries_len.to_le_bytes()); // Output buffer length

        // Directory entries
        response.extend_from_slice(&entries);

        Ok(response)
    }

    /// Parse username from SMB2 SESSION_SETUP request (simplified)
    /// In real SMB2, username is in NTLMSSP blob. This is a simplified version.
    #[cfg(feature = "smb")]
    fn parse_smb2_username(body: &[u8]) -> Option<String> {
        // Look for printable ASCII username in the body
        // This is a simplified approach - real SMB2 would parse NTLMSSP
        if body.len() < 24 {
            return None;
        }

        // Try to find ASCII username (basic heuristic)
        let mut username_bytes = Vec::new();
        for &b in body.iter().take(body.len().min(200)).skip(24) {
            if (32..=126).contains(&b) {
                username_bytes.push(b);
            } else if !username_bytes.is_empty() {
                break;
            }
        }

        if username_bytes.len() >= 3 {
            String::from_utf8(username_bytes).ok()
        } else {
            None
        }
    }

    /// Build an SMB2 ERROR Response (MS-SMB2 2.2.2) for an arbitrary command.
    ///
    /// The header carries the failing `status`; the body is the 9-byte error body with an
    /// empty error-data buffer, which is what a client parses when the status is a failure.
    /// Used to refuse an operation outright rather than answering STATUS_SUCCESS with
    /// whatever the LLM did or did not say - a refusal must be distinguishable from silence.
    ///
    /// The MessageId, TreeId and SessionId are echoed from the request. A client matches a
    /// reply to its outstanding request by MessageId, so an error carrying the wrong one is
    /// as good as no error at all.
    #[cfg(feature = "smb")]
    fn build_error_response(request_header: &[u8], command: u16, status: u32) -> Result<Vec<u8>> {
        // MS-SMB2 2.2.1.2 (SYNC header) field offsets:
        //   0 ProtocolId(4)  4 StructureSize(2)  6 CreditCharge(2)  8 Status(4)
        //  12 Command(2)    14 CreditResponse(2) 16 Flags(4)       20 NextCommand(4)
        //  24 MessageId(8)  32 Reserved(4)       36 TreeId(4)      40 SessionId(8)
        //  48 Signature(16)
        const HEADER_LEN: usize = 64;
        if request_header.len() < HEADER_LEN {
            return Err(anyhow::anyhow!(
                "SMB2 request header too short to answer: {} bytes",
                request_header.len()
            ));
        }

        let mut response = Vec::with_capacity(HEADER_LEN + 9);

        response.extend_from_slice(b"\xFESMB"); // 0  ProtocolId
        response.extend_from_slice(&[64, 0]); // 4  StructureSize (always 64)
        response.extend_from_slice(&[0, 0]); // 6  CreditCharge
        response.extend_from_slice(&status.to_le_bytes()); // 8  NTSTATUS
        response.extend_from_slice(&command.to_le_bytes()); // 12 Command
        response.extend_from_slice(&[1, 0]); // 14 CreditResponse
        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // 16 Flags (SERVER_TO_REDIR)
        response.extend_from_slice(&[0; 4]); // 20 NextCommand (not compounded)
        response.extend_from_slice(&request_header[24..32]); // 24 MessageId (echoed)
        response.extend_from_slice(&[0; 4]); // 32 Reserved
        response.extend_from_slice(&request_header[36..40]); // 36 TreeId (echoed)
        response.extend_from_slice(&request_header[40..48]); // 40 SessionId (echoed)
        response.extend_from_slice(&[0; 16]); // 48 Signature (unsigned)
        debug_assert_eq!(response.len(), HEADER_LEN, "SMB2 header must be 64 bytes");

        // SMB2 ERROR Response body
        response.extend_from_slice(&[9, 0]); // Structure size (9)
        response.extend_from_slice(&[0, 0]); // ErrorContextCount + Reserved
        response.extend_from_slice(&[0; 4]); // ByteCount = 0
        response.push(0); // ErrorData (one padding byte when ByteCount is 0)

        Ok(response)
    }

    /// Refuse an operation because the LLM call failed, in SMB2's own vocabulary.
    ///
    /// Fail closed: the request is answered with an NTSTATUS failure, never with
    /// STATUS_SUCCESS and invented content, and never with silence. Before this existed,
    /// five of the six `consult_llm` call sites propagated the error with `?`, which broke
    /// the connection loop - the client saw a dead socket and waited out its own timeout.
    ///
    /// `STATUS_INSUFFICIENT_RESOURCES` (0xC000009A) is used when
    /// `crate::llm::is_overload_error` identifies capacity exhaustion, because it is the
    /// closest NTSTATUS to "retryable"; every other failure is `STATUS_INTERNAL_ERROR`
    /// (0xC00000E5). Both stay distinguishable from the model's own refusal
    /// (STATUS_ACCESS_DENIED) and from an undecodable payload (STATUS_DATA_ERROR).
    #[cfg(feature = "smb")]
    fn llm_failure_response(
        request_header: &[u8],
        command: u16,
        operation: &str,
        path: &str,
        err: &anyhow::Error,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Vec<u8>> {
        let overloaded = crate::llm::is_overload_error(err);
        let status = if overloaded {
            STATUS_INSUFFICIENT_RESOURCES
        } else {
            STATUS_INTERNAL_ERROR
        };

        Log::new(Some(status_tx)).warn(format!(
            "SMB {} {}: LLM {} - refusing with NTSTATUS 0x{:08X}: {}",
            operation,
            path,
            if overloaded {
                "overloaded"
            } else {
                "backend failure"
            },
            status,
            err
        ));

        Self::build_error_response(request_header, command, status)
    }

    /// Build SMB2 ACCESS_DENIED response for SESSION_SETUP
    #[cfg(feature = "smb")]
    fn build_auth_denied_response(request_header: &[u8]) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // SMB2 Header with ACCESS_DENIED status
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]); // Header length
        response.extend_from_slice(&[0, 0]); // Credit charge
        response.extend_from_slice(&[0x16, 0x00, 0x00, 0xC0]); // STATUS_ACCESS_DENIED (0xC0000016)
        response.extend_from_slice(&[0x01, 0x00]); // Command (SESSION_SETUP)
        response.extend_from_slice(&[0, 0]); // Credits

        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags (response)

        // Copy message ID from request
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]); // Reserved
        response.extend_from_slice(&[0; 4]); // Tree ID
        response.extend_from_slice(&[0; 8]); // Session ID (0 = denied)
        response.extend_from_slice(&[0; 16]); // Signature

        // Minimal Session Setup Response body (9 bytes for error)
        response.extend_from_slice(&[9, 0]); // Structure size
        response.extend_from_slice(&[0; 2]); // Session flags
        response.extend_from_slice(&[0; 2]); // Security buffer offset
        response.extend_from_slice(&[0; 2]); // Security buffer length
        response.extend_from_slice(&[0]); // Padding

        Ok(response)
    }

    /// Build SESSION_SETUP response with specific username
    #[cfg(feature = "smb")]
    ///
    /// Takes the state lock with `.lock().await`. It used to call
    /// `tokio::sync::Mutex::blocking_lock()`, which **panics** when called from a runtime
    /// thread - so every SESSION_SETUP killed its connection task the moment the LLM
    /// approved the login. The panic is swallowed by `tokio::spawn`, so the server stayed
    /// in Running, the access log showed the auth succeeding, and the client simply hung
    /// until its own timeout. TREE_CONNECT had the identical bug.
    async fn build_session_setup_response_with_user(
        request_header: &[u8],
        state: &Arc<Mutex<SmbConnectionState>>,
        username: String,
    ) -> Result<Vec<u8>> {
        let mut response = Vec::new();

        // Allocate session ID
        let session_id = {
            let mut s = state.lock().await;
            let sid = s.next_session_id;
            s.next_session_id += 1;

            // Create session with specified username
            s.sessions.insert(
                sid,
                SmbSession {
                    session_id: sid,
                    username: username.clone(),
                    _authenticated: true,
                },
            );
            sid
        };

        // SMB2 Header
        response.extend_from_slice(b"\xFESMB");
        response.extend_from_slice(&[64, 0]);
        response.extend_from_slice(&[0, 0]);
        response.extend_from_slice(&[0, 0, 0, 0]); // STATUS_SUCCESS
        response.extend_from_slice(&[0x01, 0x00]); // Command (SESSION_SETUP)
        response.extend_from_slice(&[1, 0]);

        response.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // Flags (response)

        // Copy message ID
        response.extend_from_slice(&request_header[24..32]);

        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&[0; 4]);
        response.extend_from_slice(&session_id.to_le_bytes()); // Session ID
        response.extend_from_slice(&[0; 16]); // Signature

        // Session Setup Response body
        response.extend_from_slice(&[9, 0]); // Structure size
        response.extend_from_slice(&[0x01, 0x00]); // Session flags (logged in)
        response.extend_from_slice(&[0; 2]); // Security buffer offset
        response.extend_from_slice(&[0; 2]); // Security buffer length
        response.extend_from_slice(&[0]); // Padding

        Ok(response)
    }
}
