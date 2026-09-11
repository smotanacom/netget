//! SOCKS5 proxy server implementation with LLM control
//!
//! This module implements a SOCKS5 proxy server with:
//! - SOCKS5 protocol handshake and authentication
//! - LLM-controlled connection decisions (allow/deny)
//! - Pattern-based filtering for selective LLM involvement
//! - Optional MITM mode for traffic inspection
//! - Support for IPv4, IPv6, and domain name targets

pub mod actions;
pub mod filter;

use crate::server::connection::ConnectionId;
use anyhow::{bail, Context, Result};
use filter::{FilterMode, Socks5FilterConfig};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::Socks5Protocol;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use actions::{SOCKS5_AUTH_REQUEST_EVENT, SOCKS5_CONNECT_REQUEST_EVENT};

/// SOCKS5 protocol constants
const SOCKS5_VERSION: u8 = 0x05;
const AUTH_METHOD_NO_AUTH: u8 = 0x00;
const AUTH_METHOD_USERNAME_PASSWORD: u8 = 0x02;
const AUTH_METHOD_NO_ACCEPTABLE: u8 = 0xFF;

const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

const REPLY_SUCCESS: u8 = 0x00;
const REPLY_GENERAL_FAILURE: u8 = 0x01;
const REPLY_CONNECTION_NOT_ALLOWED: u8 = 0x02;
const REPLY_NETWORK_UNREACHABLE: u8 = 0x03;
const REPLY_HOST_UNREACHABLE: u8 = 0x04;
const REPLY_CONNECTION_REFUSED: u8 = 0x05;

const _REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const _REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// How long a peer has to complete each read of the SOCKS5 handshake.
///
/// Every handshake read is a `read_exact`, and until this existed none of them had a deadline:
/// a peer that connected and sent one byte held a connection task, a `ServerInstance` entry and
/// a socket for as long as the process lived. No authentication happens before any of it, so
/// the cost of holding one was a single `connect()`.
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;

/// Render relayed bytes for the LLM: printable payloads as text, everything else
/// as hex. Returns the string and the encoding label to put on the event so the
/// model knows which form it is looking at (and which to echo back).
fn encode_relay_data(data: &[u8]) -> (String, &'static str) {
    if data
        .iter()
        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
    {
        (String::from_utf8_lossy(data).to_string(), "utf8")
    } else {
        (hex::encode(data), "hex")
    }
}

/// Map an outbound connection failure onto the SOCKS5 reply code the client
/// expects (RFC 1928 section 6).
fn socks5_reply_for_connect_error(err: &anyhow::Error) -> u8 {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return match io_err.kind() {
                std::io::ErrorKind::ConnectionRefused => REPLY_CONNECTION_REFUSED,
                std::io::ErrorKind::TimedOut => REPLY_HOST_UNREACHABLE,
                std::io::ErrorKind::NetworkUnreachable => REPLY_NETWORK_UNREACHABLE,
                std::io::ErrorKind::HostUnreachable => REPLY_HOST_UNREACHABLE,
                _ => REPLY_GENERAL_FAILURE,
            };
        }
    }
    REPLY_GENERAL_FAILURE
}

/// Target address for SOCKS5 connection
#[derive(Debug, Clone)]
pub enum TargetAddr {
    Ipv4(Ipv4Addr, u16),
    Ipv6(Ipv6Addr, u16),
    Domain(String, u16),
}

impl std::fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetAddr::Ipv4(ip, port) => write!(f, "{}:{}", ip, port),
            TargetAddr::Ipv6(ip, port) => write!(f, "[{}]:{}", ip, port),
            TargetAddr::Domain(domain, port) => write!(f, "{}:{}", domain, port),
        }
    }
}

/// SOCKS5 proxy server that forwards connections via LLM decisions
pub struct Socks5Server;

impl Socks5Server {
    /// Spawn SOCKS5 proxy server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        Log::new(Some(&status_tx)).info(format!("SOCKS5 starting on {}", listen_addr));

        // Get or initialize SOCKS5 filter configuration
        let mut config = app_state
            .get_socks5_filter_config(server_id)
            .await
            .unwrap_or_else(|| {
                info!("No SOCKS5 filter config found, using defaults");
                Socks5FilterConfig::default()
            });

        // Apply startup parameters if provided
        if let Some(ref params) = startup_params {
            Log::new(Some(&status_tx)).info("Applying SOCKS5 startup parameters");

            // Parse auth methods
            if let Some(methods) = params.get_optional_array("auth_methods")? {
                config.auth_methods.clear();
                for method in methods {
                    if let Some(method_str) = method.as_str() {
                        match method_str {
                            "none" => config.auth_methods.push(AUTH_METHOD_NO_AUTH),
                            "username_password" => {
                                config.auth_methods.push(AUTH_METHOD_USERNAME_PASSWORD)
                            }
                            _ => warn!("Unknown auth method: {}", method_str),
                        }
                    }
                }
                Log::new(Some(&status_tx)).info(format!("Auth methods: {:?}", config.auth_methods));
            }

            // Parse default action
            if let Some(action_str) = params.get_optional_string("default_action")? {
                config.default_action = action_str;
                Log::new(Some(&status_tx))
                    .info(format!("Default action: {}", config.default_action));
            }

            // Parse filter configuration
            if let Some(filter) = params.get_optional_object("filter")? {
                if let Some(patterns) = filter
                    .get("target_host_patterns")
                    .and_then(|v| v.as_array())
                {
                    config.target_host_patterns = patterns
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect();
                }
                if let Some(ranges) = filter.get("target_port_ranges").and_then(|v| v.as_array()) {
                    config.target_port_ranges = ranges
                        .iter()
                        .filter_map(|v| v.as_array())
                        .filter_map(|arr| {
                            if arr.len() == 2 {
                                let start = arr[0].as_u64()? as u16;
                                let end = arr[1].as_u64()? as u16;
                                Some((start, end))
                            } else {
                                None
                            }
                        })
                        .collect();
                }
            }

            // Parse filter mode
            if let Some(mode_str) = params.get_optional_string("filter_mode")? {
                config.filter_mode = match mode_str.as_str() {
                    "allow_all" => FilterMode::AllowAll,
                    "deny_all" => FilterMode::DenyAll,
                    "ask_llm" => FilterMode::AskLlm,
                    "selective" => FilterMode::Selective,
                    _ => {
                        warn!("Unknown filter mode: {}, using default", mode_str);
                        config.filter_mode
                    }
                };
                Log::new(Some(&status_tx)).info(format!("Filter mode: {:?}", config.filter_mode));
            }

            // Parse MITM mode
            if let Some(mitm) = params.get_optional_bool("mitm_by_default")? {
                config.mitm_by_default = mitm;
                Log::new(Some(&status_tx))
                    .info(format!("MITM by default: {}", config.mitm_by_default));
            }
        }

        // Store config in app state
        app_state
            .set_socks5_filter_config(server_id, config.clone())
            .await;

        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("SOCKS5 proxy ready on {}", local_addr));

        let protocol = Arc::new(Socks5Protocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let config_clone = config.clone();

                        tokio::spawn(async move {
                            Log::new(Some(&status_clone)).info(format!(
                                "SOCKS5 connection {} from {}",
                                connection_id, remote_addr
                            ));

                            if let Err(e) = Self::handle_connection(
                                stream,
                                connection_id,
                                remote_addr,
                                local_addr_conn,
                                llm_clone,
                                state_clone.clone(),
                                status_clone.clone(),
                                protocol_clone,
                                server_id,
                                config_clone,
                            )
                            .await
                            {
                                Log::new(Some(&status_clone)).error(format!(
                                    "SOCKS5 connection {} error: {}",
                                    connection_id, e
                                ));
                            }

                            // Connection closed - mark as closed
                            state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept SOCKS5 connection: {}", e);
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

    /// Handle individual SOCKS5 connection
    async fn handle_connection(
        mut client_stream: TcpStream,
        connection_id: ConnectionId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<Socks5Protocol>,
        server_id: ServerId,
        config: Socks5FilterConfig,
    ) -> Result<()> {
        // Add connection to ServerInstance
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr,
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

        // Phase 1: Handshake - negotiate auth method
        Log::new(Some(&status_tx)).debug(format!("SOCKS5 {} phase 1: handshake", connection_id));

        let selected_method = tokio::time::timeout(
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            Self::negotiate_auth(&mut client_stream, &config, connection_id, &status_tx),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the SOCKS5 greeting"))??;

        Log::new(Some(&status_tx)).debug(format!(
            "SOCKS5 {} selected auth method: 0x{:02x}",
            connection_id, selected_method
        ));

        // Phase 2: Authentication (if required)
        let username = if selected_method == AUTH_METHOD_USERNAME_PASSWORD {
            Log::new(Some(&status_tx))
                .debug(format!("SOCKS5 {} phase 2: authentication", connection_id));

            let auth_result = Self::authenticate_username_password(
                &mut client_stream,
                connection_id,
                &llm_client,
                &app_state,
                &status_tx,
                &protocol,
                server_id,
            )
            .await?;

            Some(auth_result)
        } else {
            None
        };

        // Phase 3: Process CONNECT request
        Log::new(Some(&status_tx))
            .debug(format!("SOCKS5 {} phase 3: CONNECT request", connection_id));

        let target_addr = tokio::time::timeout(
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            Self::parse_connect_request(&mut client_stream, connection_id, &status_tx),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the SOCKS5 CONNECT request"))??;

        Log::new(Some(&status_tx)).info(format!(
            "SOCKS5 {} CONNECT to {}",
            connection_id, target_addr
        ));

        // Update connection with target address
        app_state
            .update_socks5_target(
                server_id,
                connection_id,
                Some(target_addr.to_string()),
                username.clone(),
            )
            .await;

        // Check if target matches filter
        let matches_filter = Self::check_filter_match(&target_addr, &config);

        Log::new(Some(&status_tx)).debug(format!(
            "SOCKS5 {} filter match: {}",
            connection_id, matches_filter
        ));

        // Decide whether to ask LLM or use default action
        let (should_allow, mitm_enabled) = match (&config.filter_mode, matches_filter) {
            (FilterMode::AllowAll, _) => (true, config.mitm_by_default),
            (FilterMode::DenyAll, _) => (false, false),
            (FilterMode::Selective, true) | (FilterMode::AskLlm, _) => {
                // Ask LLM. A failure here (model unreachable, invalid actions,
                // retries exhausted) must still produce a SOCKS5 reply: returning
                // Err dropped the connection without one, so the client saw an
                // unexplained EOF instead of a refusal. Fail closed.
                match Self::ask_llm_for_decision(
                    &target_addr,
                    username.as_deref(),
                    connection_id,
                    &llm_client,
                    &app_state,
                    &status_tx,
                    &protocol,
                    server_id,
                )
                .await
                {
                    Ok(decision) => decision,
                    Err(e) => {
                        // `decision=` tags follow src/server/radius/: the wire carries only
                        // "not allowed" (REPLY_CONNECTION_NOT_ALLOWED is the single refusal
                        // code SOCKS5 has), so the log is the only place a backend outage can
                        // be told apart from the model deliberately refusing.
                        Log::new(Some(&status_tx)).warn(format!(
                            "SOCKS5 {} decision=fail_closed_llm_error target={} - denying: {}",
                            connection_id, target_addr, e
                        ));
                        (false, false)
                    }
                }
            }
            (FilterMode::Selective, false) => {
                // No filter match, use default action
                let allow = config.default_action == "allow";
                (allow, allow && config.mitm_by_default)
            }
        };

        if !should_allow {
            Log::new(Some(&status_tx)).warn(format!(
                "SOCKS5 {} decision=deny target={} connection denied",
                connection_id, target_addr
            ));

            // Send SOCKS5 reply: connection not allowed
            Self::send_connect_reply(
                &mut client_stream,
                REPLY_CONNECTION_NOT_ALLOWED,
                &target_addr,
            )
            .await?;
            return Ok(());
        }

        // Connect to target.
        //
        // On failure the client must be told with a SOCKS5 reply; returning Err
        // here used to drop the connection without any reply at all, so clients
        // (curl included) sat waiting for the CONNECT response until they timed
        // out instead of reporting the real error.
        let mut target_stream =
            match Self::connect_to_target(&target_addr, connection_id, &status_tx).await {
                Ok(stream) => stream,
                Err(e) => {
                    let reply = socks5_reply_for_connect_error(&e);
                    Log::new(Some(&status_tx)).warn(format!(
                        "SOCKS5 {} connect to {} failed: {} (reply 0x{:02x})",
                        connection_id, target_addr, e, reply
                    ));
                    Self::send_connect_reply(&mut client_stream, reply, &target_addr).await?;
                    return Ok(());
                }
            };

        Log::new(Some(&status_tx)).info(format!(
            "SOCKS5 {} connected to {}",
            connection_id, target_addr
        ));

        // Send SOCKS5 reply: success
        Self::send_connect_reply(&mut client_stream, REPLY_SUCCESS, &target_addr).await?;

        // Phase 4: Relay data bidirectionally
        Log::new(Some(&status_tx)).debug(format!(
            "SOCKS5 {} phase 4: relay (MITM: {})",
            connection_id, mitm_enabled
        ));

        if mitm_enabled {
            // MITM mode: inspect and modify data
            Self::relay_with_mitm(
                client_stream,
                target_stream,
                connection_id,
                &target_addr,
                username.as_deref(),
                &llm_client,
                &app_state,
                &status_tx,
                &protocol,
                server_id,
            )
            .await?;
        } else {
            // Passthrough mode: direct relay
            match tokio::io::copy_bidirectional(&mut client_stream, &mut target_stream).await {
                Ok((client_to_target_bytes, target_to_client_bytes)) => {
                    Log::new(Some(&status_tx)).info(format!(
                        "SOCKS5 {} relay complete: {}↑ {}↓",
                        connection_id, client_to_target_bytes, target_to_client_bytes
                    ));
                    // Without this the rail's ↓/↑ counters sit at zero for the whole life of
                    // a connection that may have relayed gigabytes.
                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            Some(client_to_target_bytes),
                            Some(target_to_client_bytes),
                            None,
                            None,
                        )
                        .await;
                }
                Err(e) => {
                    Log::new(Some(&status_tx))
                        .warn(format!("SOCKS5 {} relay error: {}", connection_id, e));
                }
            }
        }

        Ok(())
    }

    /// Relay with MITM inspection - asks LLM for each data chunk
    async fn relay_with_mitm(
        mut client_stream: TcpStream,
        mut target_stream: TcpStream,
        connection_id: ConnectionId,
        target_addr: &TargetAddr,
        username: Option<&str>,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<Socks5Protocol>,
        server_id: ServerId,
    ) -> Result<()> {
        use actions::{SOCKS5_DATA_FROM_TARGET_EVENT, SOCKS5_DATA_TO_TARGET_EVENT};

        Log::new(Some(status_tx)).info(format!("SOCKS5 {} MITM relay active", connection_id));

        let mut client_buf = vec![0u8; 8192];
        let mut target_buf = vec![0u8; 8192];
        let mut client_to_target_total = 0u64;
        let mut target_to_client_total = 0u64;

        loop {
            tokio::select! {
                // Read from client (data going to target)
                result = client_stream.read(&mut client_buf) => {
                    match result {
                        Ok(0) => {
                            Log::new(Some(status_tx)).debug(format!("SOCKS5 {} client closed", connection_id));
                            break;
                        }
                        Ok(n) => {
                            let data = &client_buf[..n];
                            Log::new(Some(status_tx)).trace(format!("SOCKS5 {} client→target {} bytes: {:?}", connection_id, n, data));

                            // Ask LLM what to do with this data. Binary payloads
                            // are hex-encoded rather than run through
                            // from_utf8_lossy, which replaced every non-UTF-8 byte
                            // with U+FFFD and made the payload unrecoverable.
                            let (data_str, encoding) = encode_relay_data(data);
                            let event = Event::new(&SOCKS5_DATA_TO_TARGET_EVENT, serde_json::json!({
                                "data": data_str,
                                "encoding": encoding,
                                "target": target_addr.to_string(),
                                "username": username,
                            }));

                            let execution_result = call_llm(
                                llm_client,
                                app_state,
                                server_id,
                                Some(connection_id),
                                &event,
                                protocol.as_ref(),
                            ).await?;

                            // Process LLM actions
                            let mut should_close = false;
                            let mut data_to_send: Option<Vec<u8>> = Some(data.to_vec());

                            for result in &execution_result.protocol_results {
                                match result {
                                    ActionResult::NoAction => {
                                        // Forward as-is (already set)
                                    }
                                    ActionResult::Output(modified_data) => {
                                        // Use modified data
                                        data_to_send = Some(modified_data.clone());
                                        Log::new(Some(status_tx)).debug(format!("SOCKS5 {} data modified: {} → {} bytes",
                                                                       connection_id, n, modified_data.len()));
                                    }
                                    ActionResult::CloseConnection => {
                                        should_close = true;
                                        data_to_send = None;
                                        Log::new(Some(status_tx)).warn(format!("SOCKS5 {} LLM close request", connection_id));
                                    }
                                    _ => {}
                                }
                            }

                            if should_close {
                                break;
                            }

                            // Send data to target
                            if let Some(data) = data_to_send {
                                target_stream.write_all(&data).await?;
                                target_stream.flush().await?;
                                client_to_target_total += data.len() as u64;
                            }
                        }
                        Err(e) => {
                            Log::new(Some(status_tx)).error(format!("SOCKS5 {} client read error: {}", connection_id, e));
                            break;
                        }
                    }
                }

                // Read from target (data going to client)
                result = target_stream.read(&mut target_buf) => {
                    match result {
                        Ok(0) => {
                            Log::new(Some(status_tx)).debug(format!("SOCKS5 {} target closed", connection_id));
                            break;
                        }
                        Ok(n) => {
                            let data = &target_buf[..n];
                            Log::new(Some(status_tx)).trace(format!("SOCKS5 {} target→client {} bytes: {:?}", connection_id, n, data));

                            // Ask LLM what to do with this data (see note above
                            // on hex encoding for binary payloads).
                            let (data_str, encoding) = encode_relay_data(data);
                            let event = Event::new(&SOCKS5_DATA_FROM_TARGET_EVENT, serde_json::json!({
                                "data": data_str,
                                "encoding": encoding,
                                "target": target_addr.to_string(),
                                "username": username,
                            }));

                            let execution_result = call_llm(
                                llm_client,
                                app_state,
                                server_id,
                                Some(connection_id),
                                &event,
                                protocol.as_ref(),
                            ).await?;

                            // Process LLM actions
                            let mut should_close = false;
                            let mut data_to_send: Option<Vec<u8>> = Some(data.to_vec());

                            for result in &execution_result.protocol_results {
                                match result {
                                    ActionResult::NoAction => {
                                        // Forward as-is (already set)
                                    }
                                    ActionResult::Output(modified_data) => {
                                        // Use modified data
                                        data_to_send = Some(modified_data.clone());
                                        Log::new(Some(status_tx)).debug(format!("SOCKS5 {} data modified: {} → {} bytes",
                                                                       connection_id, n, modified_data.len()));
                                    }
                                    ActionResult::CloseConnection => {
                                        should_close = true;
                                        data_to_send = None;
                                        Log::new(Some(status_tx)).warn(format!("SOCKS5 {} LLM close request", connection_id));
                                    }
                                    _ => {}
                                }
                            }

                            if should_close {
                                break;
                            }

                            // Send data to client
                            if let Some(data) = data_to_send {
                                client_stream.write_all(&data).await?;
                                client_stream.flush().await?;
                                target_to_client_total += data.len() as u64;
                            }
                        }
                        Err(e) => {
                            Log::new(Some(status_tx)).error(format!("SOCKS5 {} target read error: {}", connection_id, e));
                            break;
                        }
                    }
                }
            }
        }

        Log::new(Some(status_tx)).info(format!(
            "SOCKS5 {} MITM relay complete: {}↑ {}↓",
            connection_id, client_to_target_total, target_to_client_total
        ));

        Ok(())
    }

    /// Negotiate authentication method with client
    async fn negotiate_auth(
        stream: &mut TcpStream,
        config: &Socks5FilterConfig,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<u8> {
        // Read handshake: [VER, NMETHODS, METHODS...]
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await?;

        let version = buf[0];
        let nmethods = buf[1];

        Log::new(Some(status_tx)).trace(format!(
            "SOCKS5 {} handshake: version={}, nmethods={}",
            connection_id, version, nmethods
        ));

        if version != SOCKS5_VERSION {
            bail!("Unsupported SOCKS version: {}", version);
        }

        if nmethods == 0 {
            bail!("No authentication methods provided");
        }

        // Read methods
        let mut methods = vec![0u8; nmethods as usize];
        stream.read_exact(&mut methods).await?;

        Log::new(Some(status_tx)).trace(format!(
            "SOCKS5 {} client methods: {:?}",
            connection_id, methods
        ));

        // Select method based on config
        let selected_method = config
            .auth_methods
            .iter()
            .find(|&&method| methods.contains(&method))
            .copied()
            .unwrap_or(AUTH_METHOD_NO_ACCEPTABLE);

        // Send method selection: [VER, METHOD]
        let response = [SOCKS5_VERSION, selected_method];
        stream.write_all(&response).await?;
        stream.flush().await?;

        if selected_method == AUTH_METHOD_NO_ACCEPTABLE {
            bail!("No acceptable authentication methods");
        }

        Ok(selected_method)
    }

    /// Authenticate using username/password
    async fn authenticate_username_password(
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<Socks5Protocol>,
        server_id: ServerId,
    ) -> Result<String> {
        // Read auth request: [VER(1), ULEN, UNAME, PLEN, PASSWD]
        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).await?;

        let auth_version = buf[0];
        if auth_version != 0x01 {
            bail!(
                "Unsupported username/password auth version: {}",
                auth_version
            );
        }

        // Read username
        stream.read_exact(&mut buf).await?;
        let ulen = buf[0] as usize;
        let mut username_bytes = vec![0u8; ulen];
        stream.read_exact(&mut username_bytes).await?;
        let username = String::from_utf8_lossy(&username_bytes).to_string();

        // Read password
        stream.read_exact(&mut buf).await?;
        let plen = buf[0] as usize;
        let mut password_bytes = vec![0u8; plen];
        stream.read_exact(&mut password_bytes).await?;
        let password = String::from_utf8_lossy(&password_bytes).to_string();

        Log::new(Some(status_tx)).debug(format!(
            "SOCKS5 {} auth request: username={}",
            connection_id, username
        ));

        // Ask LLM to validate credentials
        let event = Event::new(
            &SOCKS5_AUTH_REQUEST_EVENT,
            serde_json::json!({
                "username": username,
                "password": password,
            }),
        );

        // A failed LLM call must still produce an auth response; returning Err
        // left the client waiting on a reply that never came. Fail closed.
        let execution_result = match call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                Log::new(Some(status_tx)).warn(format!(
                    "SOCKS5 {} decision=fail_closed_llm_error user={} auth rejected: {}",
                    connection_id, username, e
                ));
                let _ = stream.write_all(&[0x01, 0x01]).await;
                let _ = stream.flush().await;
                bail!(
                    "Authentication decision failed for user {}: {}",
                    username,
                    e
                );
            }
        };

        // Same reasoning as the CONNECT decision: match on the action the model
        // emitted rather than on ActionResult::NoAction, which several unrelated
        // actions also return.
        let allowed_action = execution_result
            .raw_actions
            .iter()
            .any(|action| action.get("type").and_then(|v| v.as_str()) == Some("allow_socks5_auth"));
        let denied_action = execution_result
            .raw_actions
            .iter()
            .any(|action| action.get("type").and_then(|v| v.as_str()) == Some("deny_socks5_auth"));
        let auth_allowed = allowed_action && !denied_action;

        // Send auth response: [VER(1), STATUS]
        let status = if auth_allowed { 0x00 } else { 0x01 };
        let response = [0x01, status];
        stream.write_all(&response).await?;
        stream.flush().await?;

        if !auth_allowed {
            let decision = if denied_action {
                "model_reject"
            } else {
                "model_silent"
            };
            Log::new(Some(status_tx)).warn(format!(
                "SOCKS5 {} decision={} user={} auth rejected",
                connection_id, decision, username
            ));
            bail!("Authentication failed for user: {}", username);
        }

        Log::new(Some(status_tx)).info(format!(
            "SOCKS5 {} authenticated as {}",
            connection_id, username
        ));

        Ok(username)
    }

    /// Parse CONNECT request from client
    async fn parse_connect_request(
        stream: &mut TcpStream,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<TargetAddr> {
        // Read request: [VER, CMD, RSV(0), ATYP, DST.ADDR, DST.PORT]
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await?;

        let version = buf[0];
        let cmd = buf[1];
        let _rsv = buf[2];
        let atyp = buf[3];

        Log::new(Some(status_tx)).trace(format!(
            "SOCKS5 {} request: version={}, cmd=0x{:02x}, atyp=0x{:02x}",
            connection_id, version, cmd, atyp
        ));

        if version != SOCKS5_VERSION {
            bail!("Unsupported SOCKS version: {}", version);
        }

        if cmd != CMD_CONNECT {
            bail!(
                "Unsupported command: 0x{:02x} (only CONNECT supported)",
                cmd
            );
        }

        // Parse destination address
        let target_addr = match atyp {
            ATYP_IPV4 => {
                let mut addr = [0u8; 4];
                stream.read_exact(&mut addr).await?;
                let ip = Ipv4Addr::from(addr);
                let mut port_buf = [0u8; 2];
                stream.read_exact(&mut port_buf).await?;
                let port = u16::from_be_bytes(port_buf);
                TargetAddr::Ipv4(ip, port)
            }
            ATYP_DOMAIN => {
                let mut len_buf = [0u8; 1];
                stream.read_exact(&mut len_buf).await?;
                let len = len_buf[0] as usize;
                let mut domain_bytes = vec![0u8; len];
                stream.read_exact(&mut domain_bytes).await?;
                let domain = String::from_utf8_lossy(&domain_bytes).to_string();
                let mut port_buf = [0u8; 2];
                stream.read_exact(&mut port_buf).await?;
                let port = u16::from_be_bytes(port_buf);
                TargetAddr::Domain(domain, port)
            }
            ATYP_IPV6 => {
                let mut addr = [0u8; 16];
                stream.read_exact(&mut addr).await?;
                let ip = Ipv6Addr::from(addr);
                let mut port_buf = [0u8; 2];
                stream.read_exact(&mut port_buf).await?;
                let port = u16::from_be_bytes(port_buf);
                TargetAddr::Ipv6(ip, port)
            }
            _ => bail!("Unsupported address type: 0x{:02x}", atyp),
        };

        Ok(target_addr)
    }

    /// Send CONNECT reply to client
    async fn send_connect_reply(
        stream: &mut TcpStream,
        reply_code: u8,
        target_addr: &TargetAddr,
    ) -> Result<()> {
        // Build reply: [VER, REP, RSV(0), ATYP, BND.ADDR, BND.PORT]
        let mut response = vec![SOCKS5_VERSION, reply_code, 0x00];

        // Add bound address (use target address for simplicity)
        match target_addr {
            TargetAddr::Ipv4(ip, port) => {
                response.push(ATYP_IPV4);
                response.extend_from_slice(&ip.octets());
                response.extend_from_slice(&port.to_be_bytes());
            }
            TargetAddr::Ipv6(ip, port) => {
                response.push(ATYP_IPV6);
                response.extend_from_slice(&ip.octets());
                response.extend_from_slice(&port.to_be_bytes());
            }
            TargetAddr::Domain(domain, port) => {
                response.push(ATYP_DOMAIN);
                response.push(domain.len() as u8);
                response.extend_from_slice(domain.as_bytes());
                response.extend_from_slice(&port.to_be_bytes());
            }
        }

        stream.write_all(&response).await?;
        stream.flush().await?;

        Ok(())
    }

    /// Connect to target address
    async fn connect_to_target(
        target_addr: &TargetAddr,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<TcpStream> {
        let target_str = target_addr.to_string();

        Log::new(Some(status_tx)).debug(format!(
            "SOCKS5 {} connecting to {}",
            connection_id, target_str
        ));

        let stream = TcpStream::connect(&target_str)
            .await
            .context(format!("Failed to connect to {}", target_str))?;

        Ok(stream)
    }

    /// Check if target matches filter patterns
    fn check_filter_match(target_addr: &TargetAddr, config: &Socks5FilterConfig) -> bool {
        let target_host = match target_addr {
            TargetAddr::Ipv4(ip, _) => ip.to_string(),
            TargetAddr::Ipv6(ip, _) => ip.to_string(),
            TargetAddr::Domain(domain, _) => domain.clone(),
        };

        let target_port = match target_addr {
            TargetAddr::Ipv4(_, port) => *port,
            TargetAddr::Ipv6(_, port) => *port,
            TargetAddr::Domain(_, port) => *port,
        };

        // Check host patterns
        let host_matches = if config.target_host_patterns.is_empty() {
            true
        } else {
            config.target_host_patterns.iter().any(|pattern| {
                if let Ok(re) = regex::Regex::new(pattern) {
                    re.is_match(&target_host)
                } else {
                    false
                }
            })
        };

        // Check port ranges
        let port_matches = if config.target_port_ranges.is_empty() {
            true
        } else {
            config
                .target_port_ranges
                .iter()
                .any(|(start, end)| target_port >= *start && target_port <= *end)
        };

        host_matches && port_matches
    }

    /// Ask LLM whether to allow connection (returns: allowed, mitm_enabled)
    async fn ask_llm_for_decision(
        target_addr: &TargetAddr,
        username: Option<&str>,
        connection_id: ConnectionId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<Socks5Protocol>,
        server_id: ServerId,
    ) -> Result<(bool, bool)> {
        Log::new(Some(status_tx))
            .debug(format!("SOCKS5 {} asking LLM for decision", connection_id));

        let event = Event::new(
            &SOCKS5_CONNECT_REQUEST_EVENT,
            serde_json::json!({
                "target": target_addr.to_string(),
                "username": username,
            }),
        );

        let execution_result = call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await?;

        // Decide from the action the model actually emitted, not from the shape
        // of the result. Several unrelated SOCKS5 actions also map to
        // ActionResult::NoAction (forward_socks5_data, allow_socks5_auth), so
        // "any NoAction" would have allowed a connection off the back of an
        // action that says nothing about this decision. An explicit deny, or no
        // decision at all, keeps the connection closed.
        let allow_action = execution_result.raw_actions.iter().find(|action| {
            action.get("type").and_then(|v| v.as_str()) == Some("allow_socks5_connect")
        });
        let denied = execution_result.raw_actions.iter().any(|action| {
            action.get("type").and_then(|v| v.as_str()) == Some("deny_socks5_connect")
        });

        let allowed = allow_action.is_some() && !denied;

        // Extract MITM flag from the allow action
        let mitm_enabled = allowed
            && allow_action
                .and_then(|action| action.get("mitm"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

        // Distinguish the three outcomes the wire cannot: the model refused, the model said
        // nothing usable, or the model allowed it. Only the first two look identical to the
        // peer (REPLY_CONNECTION_NOT_ALLOWED either way).
        let decision = if denied {
            "model_reject"
        } else if allow_action.is_some() {
            "model_allow"
        } else {
            "model_silent"
        };
        Log::new(Some(status_tx)).info(format!(
            "SOCKS5 {} decision={} target={} allowed={} mitm={}",
            connection_id, decision, target_addr, allowed, mitm_enabled
        ));

        Ok((allowed, mitm_enabled))
    }
}
