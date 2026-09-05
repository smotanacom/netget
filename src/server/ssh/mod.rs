//! SSH server implementation using russh

pub mod actions;
pub mod sftp_handler;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::{SshProtocol, SSH_AUTH_EVENT, SSH_BANNER_EVENT, SSH_SHELL_COMMAND_EVENT};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId, CryptoVec};
use russh_keys::key::KeyPair;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

/// SSH server configuration
#[derive(Clone, Debug)]
pub struct SshServerConfig {
    /// Enable shell channel support
    pub shell_enabled: bool,
    /// Enable SFTP subsystem support
    pub sftp_enabled: bool,
}

impl Default for SshServerConfig {
    fn default() -> Self {
        Self {
            shell_enabled: true,
            sftp_enabled: true,
        }
    }
}

/// SSH server implementation
pub struct SshServer {
    _config: SshServerConfig,
    _llm_client: OllamaClient,
    _app_state: Arc<AppState>,
    _status_tx: mpsc::UnboundedSender<String>,
    _server_id: Option<crate::state::ServerId>,
}

impl SshServer {
    /// Create a new SSH server
    pub fn new(
        config: SshServerConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
    ) -> Self {
        Self {
            _config: config,
            _llm_client: llm_client,
            _app_state: app_state,
            _status_tx: status_tx,
            _server_id: server_id,
        }
    }

    /// Spawn SSH server with LLM integration
    pub async fn spawn_with_llm(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<SocketAddr> {
        let config = SshServerConfig::default();
        Self::spawn_with_config(listen_addr, config, llm_client, app_state, status_tx, None).await
    }

    /// Spawn SSH server with custom configuration
    pub async fn spawn_with_config(
        listen_addr: SocketAddr,
        config: SshServerConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
    ) -> Result<SocketAddr> {
        // Generate host key
        let key_pair = generate_host_key()?;

        let russh_config = russh::server::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            auth_rejection_time: std::time::Duration::from_secs(3),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![key_pair],
            ..Default::default()
        };

        let russh_config = Arc::new(russh_config);

        // Bind TCP listener. SO_REUSEADDR matches the other TCP protocols, so restarting a
        // server on the same port does not fail with EADDRINUSE while the old socket lingers.
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!(
            "SSH server listening on {} (shell: {}, sftp: {})",
            actual_addr, config.shell_enabled, config.sftp_enabled
        ));

        // Spawn accept loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            Log::new(Some(&status_tx))
                .debug(format!("SSH: Accept loop started on {}", actual_addr));

            // Counter for connection IDs
            let mut connection_counter = 0u64;

            loop {
                match listener.accept().await {
                    Ok((tcp_stream, peer_addr)) => {
                        connection_counter += 1;
                        Log::new(Some(&status_tx)).info(format!(
                            "SSH: Accepted TCP connection #{} from {}",
                            connection_counter, peer_addr
                        ));
                        debug!("SSH: Creating handler for connection from {}", peer_addr);

                        // Get next connection ID
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Track connection in server state
                        if let Some(server_id_val) = server_id {
                            use crate::state::server::{
                                ConnectionState as ServerConnectionState, ConnectionStatus,
                                ProtocolConnectionInfo,
                            };
                            let now = std::time::Instant::now();

                            let conn_state = ServerConnectionState {
                                id: connection_id,
                                remote_addr: peer_addr,
                                local_addr: actual_addr,
                                bytes_sent: 0,
                                bytes_received: 0,
                                packets_sent: 0,
                                packets_received: 0,
                                last_activity: now,
                                status: ConnectionStatus::Active,
                                status_changed_at: now,
                                protocol_info: ProtocolConnectionInfo::empty(),
                            };

                            let app_state_clone = app_state.clone();
                            let status_tx_clone = status_tx.clone();

                            tokio::spawn(async move {
                                app_state_clone
                                    .add_connection_to_server(server_id_val, conn_state)
                                    .await;
                                let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                            });
                        }

                        // Create handler for this connection
                        let handler = SshHandler::new(
                            connection_id,
                            config.clone(),
                            llm_client.clone(),
                            app_state.clone(),
                            status_tx.clone(),
                            server_id,
                            Some(peer_addr),
                        );

                        let config_clone = russh_config.clone();
                        let status_tx_clone = status_tx.clone();

                        // Spawn connection handler
                        tokio::spawn(async move {
                            Log::new(Some(&status_tx_clone))
                                .debug(format!("SSH: Starting SSH protocol for {}", peer_addr));

                            match russh::server::run_stream(config_clone, tcp_stream, handler).await
                            {
                                Ok(_) => {
                                    Log::new(Some(&status_tx_clone))
                                        .info(format!("SSH: Connection closed: {}", peer_addr));
                                }
                                Err(e) => {
                                    Log::new(Some(&status_tx_clone)).error(format!(
                                        "SSH: Connection error from {}: {}",
                                        peer_addr, e
                                    ));
                                }
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("SSH: Accept error: {}", e));
                        break;
                    }
                }
            }

            debug!("SSH: Accept loop ended");
        });

        // Register the accept loop so stop_server can abort it and release the port.
        if let Some(server_id_val) = server_id {
            task_registrar
                .register_server_task(server_id_val, accept_handle)
                .await;
        }

        Ok(actual_addr)
    }

    /// Spawn SSH server with action-based LLM integration
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let config = SshServerConfig::default();
        Self::spawn_with_config(
            listen_addr,
            config,
            llm_client,
            app_state,
            status_tx,
            Some(server_id),
        )
        .await
    }
}

/// Generate a host key for the SSH server
fn generate_host_key() -> Result<KeyPair> {
    // Generate an Ed25519 key pair
    let key =
        KeyPair::generate_ed25519().ok_or_else(|| anyhow!("Failed to generate Ed25519 key"))?;
    Ok(key)
}

/// SSH session handler
pub struct SshHandler {
    connection_id: ConnectionId,
    config: SshServerConfig,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    #[allow(dead_code)] // Used for connection tracking in new_client, not in handler methods
    server_id: Option<crate::state::ServerId>,
    #[allow(dead_code)] // Stored for future use (e.g., logging peer address in errors)
    remote_addr: Option<SocketAddr>,
    /// SSH protocol handler for action execution
    protocol: Arc<SshProtocol>,
    /// Active channels and their types
    channel_types: Arc<Mutex<HashMap<ChannelId, ChannelType>>>,
    /// Active channel objects (for SFTP)
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    /// Input buffers for shell channels (accumulate until newline)
    shell_buffers: Arc<Mutex<HashMap<ChannelId, Vec<u8>>>>,
    /// Track if we've sent initial data for each channel (for banner vs empty enter)
    channel_initialized: Arc<Mutex<HashMap<ChannelId, bool>>>,
}

/// Type of SSH channel
#[derive(Debug, Clone)]
enum ChannelType {
    Session,
    Sftp,
}

impl SshHandler {
    fn new(
        connection_id: ConnectionId,
        config: SshServerConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
        remote_addr: Option<SocketAddr>,
    ) -> Self {
        Self {
            connection_id,
            config,
            llm_client,
            app_state,
            status_tx,
            server_id,
            remote_addr,
            protocol: Arc::new(SshProtocol::new()),
            channel_types: Arc::new(Mutex::new(HashMap::new())),
            channels: Arc::new(Mutex::new(HashMap::new())),
            shell_buffers: Arc::new(Mutex::new(HashMap::new())),
            channel_initialized: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Convert line endings to SSH/terminal format (\r\n)
    /// This is required for proper display in SSH terminals.
    ///
    /// Handles both Unix (\n) and Windows (\r\n) line endings by:
    /// 1. First normalizing to Unix (\n) - removes any existing \r
    /// 2. Then converting all \n to \r\n
    ///
    /// This ensures consistent output regardless of what the LLM generates.
    fn normalize_line_endings(text: &str) -> String {
        // First normalize to Unix line endings, then convert to SSH format
        // This prevents double \r\r\n if LLM already sends \r\n
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    }

    /// Get a channel object from the internal storage
    async fn get_channel(&mut self, channel_id: ChannelId) -> Option<Channel<Msg>> {
        self.channels.lock().await.remove(&channel_id)
    }

    /// Ask the handler/LLM whether to accept a login.
    ///
    /// `auth_type` is exactly "password" or "publickey" so that script and static handlers can
    /// match on it. The password, when there is one, travels in its own field: it used to be
    /// formatted into `auth_type` ("password (user='x', password='y')"), which both broke every
    /// handler comparing `auth_type == "password"` and contradicted the documented parameter.
    async fn llm_auth_decision(
        &self,
        username: &str,
        auth_type: &str,
        password: Option<&str>,
    ) -> Result<bool> {
        let server_id = self
            .server_id
            .unwrap_or_else(|| crate::state::ServerId::new(1));
        info!(
            "SSH: llm_auth_decision() called for user '{}' via {}",
            username, auth_type
        );
        debug!(
            "SSH: llm_auth_decision - server_id={:?}, connection={}",
            server_id, self.connection_id
        );
        Log::new(Some(&self.status_tx)).debug(format!(
            "SSH: llm_auth_decision('{}', '{}')",
            username, auth_type
        ));

        // Create event with auth data
        let mut event_data = serde_json::json!({
            "username": username,
            "auth_type": auth_type,
        });
        if let Some(password) = password {
            event_data["password"] = serde_json::Value::String(password.to_string());
        }
        let event = Event::new(&SSH_AUTH_EVENT, event_data);

        match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => {
                // Look for Custom result with auth decision
                for protocol_result in result.protocol_results {
                    if let ActionResult::Custom { name, data } = protocol_result {
                        if name == "ssh_auth_decision" {
                            if let Some(allowed) = data.get("allowed").and_then(|v| v.as_bool()) {
                                // Three outcomes reach the same SSH_MSG_USERAUTH_FAILURE on the
                                // wire, so the `decision=` tag is the only thing that tells an
                                // operator which one happened. Keep the three distinct, the way
                                // `src/server/radius/` does: an explicit denial, silence, and a
                                // backend failure are different incidents.
                                let decision = if allowed {
                                    "model_accept"
                                } else {
                                    "model_reject"
                                };
                                Log::new(Some(&self.status_tx)).info(format!(
                                    "SSH auth for '{}': decision={}",
                                    username, decision
                                ));
                                return Ok(allowed);
                            }
                        }
                    }
                }

                // If no auth decision found, deny by default. Non-fatal: the client
                // gets a real SSH_MSG_USERAUTH_FAILURE, so this is a WARN, not an error.
                Log::new(Some(&self.status_tx)).warn(format!(
                    "SSH auth for '{}': decision=fail_closed_no_answer (handler returned no \
                     ssh_auth_decision)",
                    username
                ));
                Ok(false)
            }
            Err(e) => {
                // Deny. This is the fail-closed branch and it must stay that way: an
                // unreachable backend is not consent, and the client gets a real
                // SSH_MSG_USERAUTH_FAILURE rather than a hung authentication. Non-fatal
                // (the refusal is a real wire answer), so WARN rather than ERROR.
                //
                // The error is classified, never rendered to the peer — SSH has no way to
                // explain an auth failure anyway, so the category lives only in the log.
                let failure = crate::utils::WireFailure::classify(&e);
                warn!(
                    "SSH auth for '{}' on connection {}: decision=fail_closed_backend_error \
                     category={:?}: {}",
                    username, self.connection_id, failure, e
                );
                Log::new(Some(&self.status_tx)).warn(format!(
                    "SSH auth denied for '{}' on connection {}: \
                     decision=fail_closed_backend_error category={:?}",
                    username, self.connection_id, failure
                ));
                Ok(false)
            }
        }
    }

    /// Ask LLM for shell banner/greeting using action-based framework
    async fn llm_shell_banner(&self) -> Result<Option<String>> {
        let server_id = self
            .server_id
            .unwrap_or_else(|| crate::state::ServerId::new(1));

        debug!("SSH requesting shell banner from LLM");

        // Create banner event with no data
        let event = Event::new(&SSH_BANNER_EVENT, serde_json::json!({}));

        match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => {
                // Look for Output result with banner data
                for protocol_result in result.protocol_results {
                    if let ActionResult::Output(data) = protocol_result {
                        let banner = String::from_utf8_lossy(&data).to_string();
                        // Convert \n to \r\n for proper SSH terminal display
                        let normalized = Self::normalize_line_endings(&banner);
                        debug!("SSH banner received: {} bytes", normalized.len());
                        return Ok(Some(normalized));
                    }
                }

                // No banner in results
                debug!("SSH banner: decision=no_answer (handler returned no banner)");
                Ok(None)
            }
            Err(e) => {
                // A missing banner is cosmetic - the shell still opens and the server writes
                // its own "$ " prompt, so the peer is not left waiting. Nothing derived from
                // the error is shown to the peer; only the log sees it. Non-fatal, so WARN.
                let failure = crate::utils::WireFailure::classify(&e);
                warn!(
                    "SSH banner on connection {}: decision=backend_error category={:?}: {}",
                    self.connection_id, failure, e
                );
                Log::new(Some(&self.status_tx)).warn(format!(
                    "SSH banner unavailable on connection {}: decision=backend_error category={:?}",
                    self.connection_id, failure
                ));
                Ok(None)
            }
        }
    }

    /// Ask LLM to handle shell command using action-based framework
    /// Returns (output, close_connection)
    ///
    /// `first_input` says whether this is the opening Enter of the session. The control-key
    /// flags are derived here so a handler can branch on Ctrl-C/Ctrl-D without scanning the
    /// raw bytes; previously they were computed only for a log line, while the prompt claimed
    /// the model would "see CTRL_C in the context flags".
    async fn llm_shell_command(
        &self,
        command: &[u8],
        first_input: bool,
    ) -> Result<(Option<String>, bool)> {
        let server_id = self
            .server_id
            .unwrap_or_else(|| crate::state::ServerId::new(1));
        let command_str = String::from_utf8_lossy(command);

        debug!("SSH shell command: {:?}", command_str);
        trace!("SSH shell command (full): {}", command_str);

        let mut control: Vec<&str> = Vec::new();
        if command.contains(&0x03) {
            control.push("ctrl_c");
        }
        if command.contains(&0x04) {
            control.push("ctrl_d");
        }
        if command.contains(&0x1A) {
            control.push("ctrl_z");
        }
        let empty_input = command.iter().all(|&b| b == b'\n' || b == b'\r');

        // Create shell command event with command data
        let event = Event::new(
            &SSH_SHELL_COMMAND_EVENT,
            serde_json::json!({
                "command": command_str.to_string(),
                "first_input": first_input,
                "empty_input": empty_input,
                "control": control,
            }),
        );

        match call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => {
                let mut output: Option<String> = None;
                let mut close_connection = false;

                // Process all protocol results
                for protocol_result in result.protocol_results {
                    match protocol_result {
                        ActionResult::Output(data) => {
                            let text = String::from_utf8_lossy(&data).to_string();
                            // Convert \n to \r\n for proper SSH terminal display
                            output = Some(Self::normalize_line_endings(&text));
                        }
                        ActionResult::CloseConnection => {
                            close_connection = true;
                        }
                        _ => {}
                    }
                }

                debug!(
                    "SSH shell response: output={}, close={}",
                    output.is_some(),
                    close_connection
                );

                Ok((output, close_connection))
            }
            Err(e) => {
                // Propagate instead of returning `Ok((None, false))`.
                //
                // That old value was worse than silence: the caller's `if let Ok(..)` matched
                // it, wrote no output, and then wrote the "$ " prompt anyway — so a backend
                // outage was indistinguishable from a command that ran successfully and
                // printed nothing. The caller now sends an SSH disconnect with a reason code
                // instead, which is a real answer the client reports to the user. Non-fatal
                // (the disconnect is a defined wire response), so WARN rather than ERROR.
                //
                // The error is logged in full here and nowhere else; the callers get it only
                // to classify it, and put a `&'static str` category on the wire.
                let failure = crate::utils::WireFailure::classify(&e);
                warn!(
                    "SSH shell command on connection {}: decision=fail_closed_backend_error \
                     category={:?}: {}",
                    self.connection_id, failure, e
                );
                Log::new(Some(&self.status_tx)).warn(format!(
                    "SSH shell command failed on connection {}: \
                     decision=fail_closed_backend_error category={:?}",
                    self.connection_id, failure
                ));
                Err(e)
            }
        }
    }
}

// Note: We no longer implement russh::server::Server trait
// Instead, we manually accept TCP connections and call russh::server::run_stream()

#[async_trait]
impl russh::server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: &russh_keys::key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        info!("SSH: auth_publickey() called for user '{}'", user);
        debug!(
            "SSH: Public key auth for user '{}', connection {}",
            user, self.connection_id
        );
        Log::new(Some(&self.status_tx)).debug(format!("SSH: auth_publickey('{}') called", user));

        // Ask LLM if this user should be allowed
        let allowed = self.llm_auth_decision(user, "publickey", None).await?;

        info!("SSH: auth_publickey result for '{}': {}", user, allowed);
        if allowed {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
            })
        }
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        info!("SSH: auth_password() called for user '{}'", user);
        debug!(
            "SSH: Password auth for user '{}', connection {}",
            user, self.connection_id
        );
        Log::new(Some(&self.status_tx)).debug(format!("SSH: auth_password('{}') called", user));

        // Ask LLM if this user/password should be allowed
        let allowed = self
            .llm_auth_decision(user, "password", Some(password))
            .await?;

        info!("SSH: auth_password result for '{}': {}", user, allowed);
        if allowed {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if !self.config.shell_enabled {
            debug!("SSH shell channel requested but shell is disabled");
            return Ok(false);
        }

        let channel_id = channel.id();
        self.channel_types
            .lock()
            .await
            .insert(channel_id, ChannelType::Session);
        self.channels.lock().await.insert(channel_id, channel);

        debug!("SSH session channel {} opened", channel_id);
        Ok(true)
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // DEBUG: SSH subsystem request summary
        Log::new(Some(&self.status_tx)).debug(format!(
            "SSH request: SUBSYSTEM channel={}, name={}",
            channel_id, name
        ));

        // TRACE: Full SSH subsystem request
        Log::new(Some(&self.status_tx)).trace(format!(
            "SSH SUBSYSTEM request: channel={}, name='{}', connection={}",
            channel_id, name, self.connection_id
        ));

        if name == "sftp" {
            if !self.config.sftp_enabled {
                // Client asked for a disabled subsystem; the server answers with
                // CHANNEL_FAILURE, so this is a WARN, not an error.
                Log::new(Some(&self.status_tx))
                    .warn("SFTP subsystem requested but SFTP is disabled");
                Log::new(Some(&self.status_tx))
                    .debug("SSH response: CHANNEL_FAILURE (SFTP disabled)");

                session.channel_failure(channel_id);
                return Ok(());
            }

            self.channel_types
                .lock()
                .await
                .insert(channel_id, ChannelType::Sftp);

            // INFO: Major lifecycle event
            Log::new(Some(&self.status_tx)).info(format!(
                "SSH SFTP subsystem started on channel {} (connection {})",
                channel_id, self.connection_id
            ));

            // Get the channel object
            if let Some(channel) = self.get_channel(channel_id).await {
                Log::new(Some(&self.status_tx))
                    .debug("SSH response: CHANNEL_SUCCESS (starting SFTP handler)");

                Log::new(Some(&self.status_tx)).trace(format!(
                    "Creating LlmSftpHandler for channel {} on connection {}",
                    channel_id, self.connection_id
                ));

                session.channel_success(channel_id);

                // Create LLM-controlled SFTP handler
                let server_id = self
                    .server_id
                    .unwrap_or_else(|| crate::state::ServerId::new(1));
                let sftp_handler = crate::server::LlmSftpHandler::new(
                    self.connection_id,
                    server_id,
                    self.llm_client.clone(),
                    self.app_state.clone(),
                    self.protocol.clone(),
                    self.status_tx.clone(),
                );

                // Run SFTP protocol (this handles all packet parsing)
                Log::new(Some(&self.status_tx)).trace(format!(
                    "Starting russh_sftp::server::run() for channel {}",
                    channel_id
                ));

                russh_sftp::server::run(channel.into_stream(), sftp_handler).await;

                // INFO: SFTP session ended (normal lifecycle end)
                Log::new(Some(&self.status_tx)).info(format!(
                    "SFTP session ended on channel {} (connection {})",
                    channel_id, self.connection_id
                ));

                Log::new(Some(&self.status_tx)).debug(format!(
                    "SSH: SFTP subsystem terminated on channel {}",
                    channel_id
                ));
            } else {
                Log::new(Some(&self.status_tx))
                    .error(format!("SFTP channel {} not found", channel_id));
                Log::new(Some(&self.status_tx))
                    .debug("SSH response: CHANNEL_FAILURE (channel not found)");

                session.channel_failure(channel_id);
            }
        } else {
            // Client asked for an unknown subsystem; the server rejects it with
            // CHANNEL_FAILURE, so this is a WARN, not an error.
            Log::new(Some(&self.status_tx)).warn(format!(
                "Unknown subsystem requested: '{}' on channel {}",
                name, channel_id
            ));
            Log::new(Some(&self.status_tx)).debug(format!(
                "SSH response: CHANNEL_FAILURE (unknown subsystem '{}')",
                name
            ));
            Log::new(Some(&self.status_tx)).trace(format!(
                "SSH rejecting unknown subsystem: name='{}', channel={}, connection={}",
                name, channel_id, self.connection_id
            ));

            session.channel_failure(channel_id);
        }

        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("SSH shell request on channel {}", channel_id);

        if !self.config.shell_enabled {
            error!("Shell requested but shell is disabled");
            session.channel_failure(channel_id);
            return Ok(());
        }

        session.channel_success(channel_id);

        // Send banner/greeting via LLM
        if let Ok(Some(banner)) = self.llm_shell_banner().await {
            let data = CryptoVec::from_slice(banner.as_bytes());
            session.data(channel_id, data);
            debug!("Sent shell banner ({} bytes)", banner.len());
        }

        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data);
        debug!("SSH exec request on channel {}: {:?}", channel_id, command);

        if !self.config.shell_enabled {
            error!("Exec requested but shell is disabled");
            session.channel_failure(channel_id);
            return Ok(());
        }

        session.channel_success(channel_id);

        // Execute command via LLM. A one-shot `ssh host <cmd>` is always the first (and only)
        // input on this channel.
        //
        // The exit status is the whole answer for a one-shot exec: a script running
        // `ssh host cmd` branches on it and, in the common `$(ssh host cmd)` form, never even
        // sees stderr. So a backend failure has to exit non-zero. This used to send
        // `exit-status 0` with no output on every branch, which is indistinguishable from a
        // command that ran and printed nothing — the same defect the interactive shell path
        // already fixes by disconnecting.
        let exit_status: u32 = match self.llm_shell_command(data, true).await {
            Ok((output, _close)) => {
                if let Some(output_text) = output {
                    let data = CryptoVec::from_slice(output_text.as_bytes());
                    session.data(channel_id, data);
                    debug!("Sent exec output ({} bytes)", output_text.len());
                } else {
                    // The handler answered, but with nothing to print. That is a legitimate
                    // "command succeeded silently", and it stays exit 0 — but it is tagged
                    // separately from the backend-error case so the log can tell them apart.
                    Log::new(Some(&self.status_tx)).debug(format!(
                        "SSH exec on channel {}: decision=model_answer_empty",
                        channel_id
                    ));
                }
                0
            }
            Err(e) => {
                // `llm_shell_command` has already logged the error in full. Here the error is
                // used only to pick a category: `WireFailure::prefixed_text()` is a
                // `&'static str`, so nothing derived from `e` can reach the peer.
                let failure = crate::utils::WireFailure::classify(&e);
                Log::new(Some(&self.status_tx)).warn(format!(
                    "SSH exec on channel {}: decision=fail_closed_backend_error category={:?}",
                    channel_id, failure
                ));

                // stderr, not stdout: `$(ssh host cmd)` captures stdout, and a notice mixed
                // into it would be indistinguishable from the command's own output.
                let notice =
                    CryptoVec::from_slice(format!("{}\r\n", failure.prefixed_text()).as_bytes());
                session.extended_data(channel_id, 1, notice);

                // Distinct statuses so a caller can back off instead of recording a permanent
                // fault: sysexits.h EX_TEMPFAIL (75) for a saturated backend, EX_UNAVAILABLE
                // (69) for anything else. SSH itself has no error code of its own here — the
                // exit status is the only field that can carry the distinction.
                if failure.is_overloaded() {
                    75
                } else {
                    69
                }
            }
        };

        // Close channel after exec
        session.exit_status_request(channel_id, exit_status);
        session.eof(channel_id);
        session.close(channel_id);

        Ok(())
    }

    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let channel_types = self.channel_types.lock().await;
        let channel_type = channel_types.get(&channel_id).cloned();
        drop(channel_types); // Release lock before async operations

        match channel_type {
            Some(ChannelType::Session) => {
                // Shell data - handle backspace, echo properly, and buffer until newline or Ctrl-C
                Log::new(Some(&self.status_tx)).trace(format!(
                    "SSH shell data received on channel {}: hex={:02x?}",
                    channel_id, data
                ));

                // Get or create buffer for this channel
                let mut buffers = self.shell_buffers.lock().await;
                let buffer = buffers.entry(channel_id).or_insert_with(Vec::new);

                // Process each byte
                for &byte in data {
                    match byte {
                        // Backspace (0x7F) or Delete (0x08)
                        0x7F | 0x08 => {
                            if !buffer.is_empty() {
                                buffer.pop();
                                // Echo: backspace + space + backspace (to erase character on screen)
                                let erase = CryptoVec::from_slice(&[0x08, b' ', 0x08]);
                                session.data(channel_id, erase);
                                trace!("SSH shell: backspace, buffer now {} bytes", buffer.len());
                            }
                        }
                        // Tab (0x09) - echo but don't buffer (for tab completion)
                        0x09 => {
                            let echo = CryptoVec::from_slice(&[byte]);
                            session.data(channel_id, echo);
                            // Don't buffer tabs - they should be handled immediately by the client
                            // or used for tab completion which doesn't need buffering
                        }
                        // Newline characters (Enter key)
                        b'\n' | b'\r' => {
                            // Echo newline as \r\n (proper line ending for terminals)
                            let echo = CryptoVec::from_slice(b"\r\n");
                            session.data(channel_id, echo);
                            // Add actual received character to buffer
                            buffer.push(byte);
                        }
                        // Control characters (0x01-0x1F except tab/newline/carriage return)
                        0x01..=0x1F => {
                            // Echo control characters visually as "^X\r\n" (e.g., ^C, ^D, ^Z)
                            // Control character to printable: add 0x40 (e.g., 0x03 + 0x40 = 0x43 = 'C')
                            let ctrl_char = byte + 0x40;
                            let echo_str = format!("^{}\r\n", ctrl_char as char);
                            let echo = CryptoVec::from_slice(echo_str.as_bytes());
                            session.data(channel_id, echo);
                            // Add to buffer for LLM to see
                            buffer.push(byte);
                        }
                        // Printable characters (0x20-0x7E)
                        0x20..=0x7E => {
                            // Echo the character
                            let echo = CryptoVec::from_slice(&[byte]);
                            session.data(channel_id, echo);
                            // Add to buffer
                            buffer.push(byte);
                        }
                        // Other bytes (non-printable, non-control)
                        _ => {
                            // Just add to buffer without echo
                            buffer.push(byte);
                        }
                    }
                }

                // Check if we should process the buffer (Enter or control characters received)
                // NOTE: Echo has already happened above in the byte loop - LLM invocation comes AFTER echo
                // Process on: Enter (\r, \n) or any control character except Tab (0x09)
                let should_process = data.iter().any(|&b| {
                    b == b'\n' || b == b'\r' || ((0x01..=0x1F).contains(&b) && b != 0x09)
                });

                if should_process {
                    // Check if this is the first interaction (for banner) or empty input
                    let mut initialized = self.channel_initialized.lock().await;
                    let is_first_input = !initialized.get(&channel_id).copied().unwrap_or(false);
                    initialized.insert(channel_id, true);
                    drop(initialized);

                    let line = buffer.clone();
                    buffer.clear();
                    drop(buffers);

                    // Only call LLM if:
                    // 1. First input (even if empty) - for banner
                    // 2. Non-empty input (command to process)
                    // 3. Any control character present (Ctrl-C, Ctrl-D, etc.) - always process
                    // 4. Empty Enter - LLM should respond with prompt
                    let has_ctrl_c = line.contains(&0x03);
                    let has_any_ctrl = line
                        .iter()
                        .any(|&b| (0x01..=0x1F).contains(&b) && b != 0x09);
                    let is_empty_cmd = line.iter().all(|&b| b == b'\n' || b == b'\r');

                    // Always process if we have any control character or non-empty command
                    if is_first_input || !is_empty_cmd || has_any_ctrl {
                        Log::new(Some(&self.status_tx)).debug(format!(
                            "SSH shell processing input ({} bytes, first={}, empty={})",
                            line.len(),
                            is_first_input,
                            is_empty_cmd
                        ));

                        Log::new(Some(&self.status_tx))
                            .trace(format!("SSH shell input (hex): {:02x?}", line));

                        Log::new(Some(&self.status_tx)).trace(format!(
                            "SSH shell input (text): {:?}",
                            String::from_utf8_lossy(&line)
                        ));

                        // Log-only summary. The flags the handler actually branches on are
                        // built inside llm_shell_command() and travel in the event itself.
                        let context = format!(
                            " [first_input={}, empty_input={}, ctrl_c={}]",
                            is_first_input, is_empty_cmd, has_ctrl_c
                        );

                        let command_result = self.llm_shell_command(&line, is_first_input).await;

                        // A failed handler call ends the session with a reason code rather
                        // than leaving the peer at a prompt that will never answer.
                        // SSH_DISCONNECT_SERVICE_NOT_AVAILABLE (7) is the closest reason
                        // RFC 4253 §11.1 defines, and ssh(1) prints it verbatim.
                        if let Err(ref e) = command_result {
                            // `llm_shell_command` logged the error in full. Only the category
                            // travels from here on: `prefixed_text()` is a `&'static str`, so
                            // no backend URL, model name or anyhow chain can reach the
                            // terminal of whoever is logged in.
                            let failure = crate::utils::WireFailure::classify(e);
                            let description = failure.prefixed_text();
                            let notice = CryptoVec::from_slice(
                                format!("\r\n{}\r\n", description).as_bytes(),
                            );
                            session.data(channel_id, notice);
                            // sysexits.h: EX_TEMPFAIL (75) says "retry", EX_UNAVAILABLE (69)
                            // says "do not". SSH's disconnect reason codes have no equivalent
                            // pair, so the exit status carries the distinction — matching what
                            // `exec_request` reports for the same failure.
                            session.exit_status_request(
                                channel_id,
                                if failure.is_overloaded() { 75 } else { 69 },
                            );
                            session.eof(channel_id);
                            session.close(channel_id);
                            session.disconnect(
                                russh::Disconnect::ServiceNotAvailable,
                                description,
                                "en",
                            );
                            Log::new(Some(&self.status_tx)).warn(format!(
                                "SSH disconnected channel {}: \
                                 decision=fail_closed_backend_error category={:?}",
                                channel_id, failure
                            ));
                        }

                        if let Ok((output, close_connection)) = command_result {
                            // Send output if present
                            if let Some(output_text) = output {
                                let response = CryptoVec::from_slice(output_text.as_bytes());
                                session.data(channel_id, response);

                                Log::new(Some(&self.status_tx)).debug(format!(
                                    "Sent shell response ({} bytes){}",
                                    output_text.len(),
                                    context
                                ));
                            }

                            // Handle close_connection flag (e.g., from Ctrl-C)
                            if close_connection {
                                Log::new(Some(&self.status_tx)).info(format!(
                                    "LLM requested shell connection close on channel {}",
                                    channel_id
                                ));

                                session.exit_status_request(channel_id, 0);
                                session.eof(channel_id);
                                session.close(channel_id);
                            } else {
                                // Send a prompt after the response so user knows where to type next
                                // This prevents commands from being echoed on the same line as output
                                let prompt = CryptoVec::from_slice(b"$ ");
                                session.data(channel_id, prompt);
                            }
                        }
                    } else {
                        // Empty Enter press after initialization - ignore it
                        Log::new(Some(&self.status_tx))
                            .trace("SSH shell: ignoring empty Enter (already initialized)");
                    }
                } else {
                    // Still accumulating input
                    Log::new(Some(&self.status_tx))
                        .trace(format!("SSH shell buffering: {} bytes total", buffer.len()));
                }
            }
            Some(ChannelType::Sftp) => {
                // SFTP data is handled by russh_sftp::server::run() in subsystem_request()
                // This case shouldn't normally be reached
                debug!(
                    "SFTP data received on channel {} - should be handled by SFTP subsystem",
                    channel_id
                );
            }
            None => {
                debug!("Data received on unknown channel {}", channel_id);
            }
        }

        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("SSH channel {} EOF", channel_id);
        session.close(channel_id);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("SSH channel {} closed", channel_id);
        self.channel_types.lock().await.remove(&channel_id);
        self.channels.lock().await.remove(&channel_id);
        self.shell_buffers.lock().await.remove(&channel_id);
        self.channel_initialized.lock().await.remove(&channel_id);
        Ok(())
    }
}
