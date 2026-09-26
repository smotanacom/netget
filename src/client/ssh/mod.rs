//! SSH client implementation
pub mod actions;

pub use actions::SshClientProtocol;

use anyhow::{Context, Result};
use russh::client::{self, Handle};
use russh::*;
use russh_keys::*;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::ssh::actions::{SSH_CLIENT_CONNECTED_EVENT, SSH_CLIENT_OUTPUT_RECEIVED_EVENT};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// Per-client data for LLM handling
struct ClientData {
    memory: String,
}

/// What applying one action to the live SSH session actually did.
///
/// SSH never yields [`crate::state::client_handles::ClientSendOutcome::Sent`]:
/// russh owns the encrypted transport, so NetGet never sees a byte count on the
/// wire. A command that really ran reports `Executed` with its exit status and
/// the size of the output it produced — which is the honest thing this client
/// can say.
pub enum SshApplied {
    /// The action ran; the string describes what it did.
    Executed(String),
    /// The session was disconnected.
    Disconnected,
}

/// How the client authenticates, decided from the startup parameters before connecting.
enum SshCredential {
    Password(String),
    PublicKey(Arc<key::KeyPair>),
}

/// How many action -> command -> output -> action turns one client will take.
///
/// Every command's output goes back to the model, whose answer may be another command, so the
/// chain is self-referential and needs a bound rather than silence. Four matches the database
/// and directory clients; the output at the bound is shown to the model and its answer is
/// dropped with a warning.
const MAX_FOLLOWUP_DEPTH: usize = 4;

/// SSH client handler
struct ClientHandler;

#[async_trait::async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Accept all server keys (for testing)
        // In production, this should verify against known_hosts
        Ok(true)
    }
}

/// SSH client that connects to a remote SSH server
pub struct SshClient;

impl SshClient {
    /// Connect to an SSH server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        // Parse startup parameters
        let params =
            startup_params.context("Missing required startup parameters for SSH client")?;

        let username = params.get_string("username")?;
        let password = params.get_optional_string("password")?;
        let private_key_path = params.get_optional_string("private_key_path")?;
        let private_key_passphrase = params.get_optional_string("private_key_passphrase")?;
        // A key path with no explicit method means public-key auth: naming a key and then
        // being asked for a password would be the surprising reading.
        let auth_method = params
            .get_optional_string("auth_method")?
            .unwrap_or_else(|| {
                if private_key_path.is_some() {
                    "publickey".to_string()
                } else {
                    "password".to_string()
                }
            });

        // Decide the credential before touching the network, so a missing password or an
        // unreadable key is reported as that rather than as a failed handshake.
        let credential = match auth_method.as_str() {
            "password" => SshCredential::Password(
                password
                    .context("auth_method 'password' needs the 'password' startup parameter")?,
            ),
            "publickey" => {
                let path = private_key_path.context(
                    "auth_method 'publickey' needs the 'private_key_path' startup parameter",
                )?;
                // The key is the operator's file, read once here; its contents never reach
                // the model, an event or the log.
                let key = russh_keys::load_secret_key(&path, private_key_passphrase.as_deref())
                    .with_context(|| format!("Failed to load SSH private key from {path}"))?;
                SshCredential::PublicKey(Arc::new(key))
            }
            other => {
                return Err(anyhow::anyhow!(
                    "Unknown SSH auth_method '{other}': expected 'password' or 'publickey'"
                ))
            }
        };

        info!(
            "SSH client {} connecting to {} as user '{}'",
            client_id, remote_addr, username
        );
        let _ = status_tx.send(format!(
            "[CLIENT] SSH connecting to {} as {}",
            remote_addr, username
        ));

        // Parse address
        let addr = match remote_addr.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(_) => {
                // Try to resolve hostname
                let parts: Vec<&str> = remote_addr.split(':').collect();
                if parts.len() != 2 {
                    return Err(anyhow::anyhow!("Invalid address format: {}", remote_addr));
                }
                let host = parts[0];
                let port: u16 = parts[1].parse().context("Invalid port number")?;

                let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
                    .await
                    .context(format!("Failed to resolve hostname: {}", host))?
                    .collect();

                addrs
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("No addresses found for {}", host))?
            }
        };

        // Create SSH config
        let config = client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(300)),
            ..<_>::default()
        };

        // Connect to SSH server
        let mut session = client::connect(Arc::new(config), addr, ClientHandler)
            .await
            .context("Failed to connect to SSH server")?;

        // Authenticate
        let auth_result = match credential {
            SshCredential::Password(password) => session
                .authenticate_password(username.clone(), password)
                .await
                .context("SSH authentication failed")?,
            SshCredential::PublicKey(key) => session
                .authenticate_publickey(username.clone(), key)
                .await
                .context("SSH authentication failed")?,
        };

        if !auth_result {
            return Err(anyhow::anyhow!(
                "SSH authentication failed: the server refused the {} credential for '{}'",
                auth_method,
                username
            ));
        }

        info!("SSH client {} authenticated successfully", client_id);
        let _ = status_tx.send(format!("[CLIENT] SSH client {} authenticated", client_id));

        // Update client status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Get local address (use the connected socket address)
        let local_addr = addr; // russh doesn't expose local addr easily, using remote for now

        // Trigger connected event to LLM
        let protocol = Arc::new(SshClientProtocol::new());
        let connected_event = Event::new(
            &SSH_CLIENT_CONNECTED_EVENT,
            serde_json::json!({
                "remote_addr": remote_addr,
                "username": username,
            }),
        );

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            memory: String::new(),
        }));

        // Clone for the spawned task
        let session_arc = Arc::new(Mutex::new(session));

        // Command channel for injected actions (the dashboard's [ send ]).
        // Registered BEFORE the connected-event LLM call below: a dashboard-created
        // client defaults to a `*` -> manual rule, so that call can park for minutes
        // waiting for a human, and the operator must be able to reach the session
        // while it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn({
            let session_arc = session_arc.clone();
            let protocol = protocol.clone();
            let llm_client = llm_client.clone();
            let app_state = app_state.clone();
            let status_tx = status_tx.clone();
            let client_data = client_data.clone();
            async move {
                Self::command_loop(
                    command_rx,
                    session_arc,
                    protocol,
                    client_id,
                    llm_client,
                    app_state,
                    status_tx,
                    client_data,
                )
                .await;
            }
        });
        app_state.register_client_task(client_id, cmd_task).await;

        let client_data_clone = client_data.clone();
        let protocol_clone = protocol.clone();
        let llm_client_clone = llm_client.clone();
        let app_state_clone = app_state.clone();
        let status_tx_clone = status_tx.clone();

        // Call LLM with connected event
        let task_registrar = app_state.clone();
        let handle = tokio::spawn(async move {
            if let Some(instruction) = app_state_clone.get_instruction_for_client(client_id).await {
                // Copy the memory out before the call: the command loop shares this
                // mutex, and a guard held across an LLM round-trip (which a `*` manual
                // rule can park for minutes) would stall every injected command.
                let memory = client_data_clone.lock().await.memory.clone();

                // Call LLM with connected event
                match call_llm_for_client(
                    &llm_client_clone,
                    &app_state_clone,
                    client_id.to_string(),
                    &instruction,
                    &memory,
                    Some(&connected_event),
                    protocol_clone.as_ref(),
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
                            client_data_clone.lock().await.memory = mem;
                        }

                        // Execute initial actions
                        for action in actions {
                            if let Err(e) = Self::execute_ssh_action(
                                &session_arc,
                                &protocol_clone,
                                action,
                                client_id,
                                &llm_client_clone,
                                &app_state_clone,
                                &status_tx_clone,
                                &client_data_clone,
                                0,
                            )
                            .await
                            {
                                error!("Error executing SSH action: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("LLM error for SSH client {}: {}", client_id, e);
                    }
                }
            }
        });
        task_registrar.register_client_task(client_id, handle).await;

        Ok(local_addr)
    }

    /// Drain injected commands until the channel closes (the client was removed,
    /// which drops the handle) or an injected `disconnect` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot serve this
    /// client: it owns no socket to write to, and `execute_command` yields a
    /// `ClientActionResult::Custom` that only russh can carry out. So the action goes
    /// through [`Self::apply_ssh_result`] — the exact function the LLM path uses,
    /// including the `ssh_output_received` follow-up event — and the outcome is
    /// recorded and replied the way the generic arm does it.
    #[allow(clippy::too_many_arguments)]
    async fn command_loop(
        mut command_rx: tokio::sync::mpsc::Receiver<crate::state::client_handles::ClientCommand>,
        session_arc: Arc<Mutex<Handle<ClientHandler>>>,
        protocol: Arc<SshClientProtocol>,
        client_id: ClientId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_data: Arc<Mutex<ClientData>>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::client_handles::ClientSendOutcome;
        use crate::state::AccessLogOwner;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();

            // `execute_action` is the only step that can fail before the session is
            // touched, so its error is a rejection (unknown verb / bad params) rather
            // than a transport failure.
            let outcome = match protocol.as_ref().execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => Self::apply_ssh_result(
                    result,
                    &session_arc,
                    &protocol,
                    client_id,
                    &llm_client,
                    &app_state,
                    &status_tx,
                    &client_data,
                    0,
                )
                .await
                .map(|applied| match applied {
                    SshApplied::Executed(detail) => ClientSendOutcome::Executed { detail },
                    SshApplied::Disconnected => ClientSendOutcome::Disconnected,
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
                error!("SSH client {} injected action failed: {}", client_id, e);
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

        // Every exit path lands here: drop the command handle so the dashboard stops
        // offering [ send ] on a dead session and a late send fails fast.
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Execute an SSH action (helper function). `depth` counts the follow-up turns so far.
    #[allow(clippy::too_many_arguments)]
    async fn execute_ssh_action(
        session_arc: &Arc<Mutex<Handle<ClientHandler>>>,
        protocol: &Arc<SshClientProtocol>,
        action: serde_json::Value,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        depth: usize,
    ) -> Result<SshApplied> {
        let result = protocol.as_ref().execute_action(action)?;
        Self::apply_ssh_result(
            result,
            session_arc,
            protocol,
            client_id,
            llm_client,
            app_state,
            status_tx,
            client_data,
            depth,
        )
        .await
    }

    /// Carry one already-decoded action out against the live session. Shared by the
    /// connected-event path, the `ssh_output_received` follow-up path and injected
    /// commands, so the channel/exec machinery exists exactly once.
    #[allow(clippy::too_many_arguments)]
    async fn apply_ssh_result(
        action_result: ClientActionResult,
        session_arc: &Arc<Mutex<Handle<ClientHandler>>>,
        protocol: &Arc<SshClientProtocol>,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        depth: usize,
    ) -> Result<SshApplied> {
        match action_result {
            ClientActionResult::Custom { name, data } if name == "execute_command" => {
                let command = data
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing command in action data")?;

                info!("SSH client {} executing command: {}", client_id, command);
                let _ = status_tx.send(format!("[CLIENT] SSH executing: {}", command));

                // Open channel and execute command. The session guard is released as
                // soon as the channel exists: `Channel` is owned, not borrowed from the
                // handle, and holding the guard across the exec + the follow-up LLM call
                // would (a) deadlock the recursive follow-up below, which locks the same
                // mutex, and (b) block every injected command for the whole round-trip.
                let mut channel = {
                    let session = session_arc.lock().await;
                    session
                        .channel_open_session()
                        .await
                        .context("Failed to open SSH channel")?
                };

                channel
                    .exec(true, command)
                    .await
                    .context("Failed to execute command")?;

                // Read output until the channel is done. EOF only says the server will send no
                // more *data*: OpenSSH sends `exit-status` after its EOF and then closes the
                // channel, so stopping at EOF loses the exit status of every command. Stop at
                // CLOSE, at the end of the channel, or at EOF once the status is already in
                // hand (a server may send it first).
                let mut output = Vec::new();
                let mut stderr = Vec::new();
                let mut exit_code: Option<u32> = None;
                let mut eof = false;

                loop {
                    match channel.wait().await {
                        Some(ChannelMsg::Data { ref data }) => {
                            output.extend_from_slice(data);
                            trace!(
                                "SSH client {} received {} bytes of output",
                                client_id,
                                data.len()
                            );
                        }
                        // Extended data type 1 is SSH_EXTENDED_DATA_STDERR (RFC 4254 §5.2).
                        Some(ChannelMsg::ExtendedData { ref data, ext: 1 }) => {
                            stderr.extend_from_slice(data);
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            exit_code = Some(exit_status);
                            debug!("SSH command exit status: {}", exit_status);
                            if eof {
                                break;
                            }
                        }
                        Some(ChannelMsg::Eof) => {
                            debug!("SSH channel EOF");
                            eof = true;
                            if exit_code.is_some() {
                                break;
                            }
                        }
                        Some(ChannelMsg::Close) | None => break,
                        Some(_) => {}
                    }
                }

                let output_str = String::from_utf8_lossy(&output).to_string();
                let stderr_str = String::from_utf8_lossy(&stderr).to_string();
                trace!("SSH command output: {}", output_str);

                let applied = SshApplied::Executed(format!(
                    "execute_command {:?}: exit_code={}, {} bytes of output, {} of stderr",
                    command,
                    exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    output.len(),
                    stderr.len()
                ));

                // Call LLM with output
                if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
                    let mut event_data = serde_json::json!({
                        "command": command,
                        "output": output_str,
                    });

                    if !stderr_str.is_empty() {
                        event_data["stderr"] = serde_json::json!(stderr_str);
                    }
                    if let Some(code) = exit_code {
                        event_data["exit_code"] = serde_json::json!(code);
                    }

                    let output_event = Event::new(&SSH_CLIENT_OUTPUT_RECEIVED_EVENT, event_data);

                    // Copy the memory out before the call: the command loop shares this
                    // mutex, and a guard held across an LLM round-trip would stall it.
                    let memory = client_data.lock().await.memory.clone();

                    match call_llm_for_client(
                        llm_client,
                        app_state,
                        client_id.to_string(),
                        &instruction,
                        &memory,
                        Some(&output_event),
                        protocol.as_ref(),
                        status_tx,
                    )
                    .await
                    {
                        Ok(ClientLlmResult {
                            actions,
                            memory_updates,
                        }) => {
                            // Update memory
                            if let Some(mem) = memory_updates {
                                client_data.lock().await.memory = mem;
                            }

                            if !actions.is_empty() && depth >= MAX_FOLLOWUP_DEPTH {
                                warn!(
                                    "SSH client {} reached the follow-up depth bound ({}); \
                                     dropping {} action(s) rather than looping",
                                    client_id,
                                    MAX_FOLLOWUP_DEPTH,
                                    actions.len()
                                );
                                return Ok(applied);
                            }

                            // Execute follow-up actions
                            for next_action in actions {
                                // Recursive call for follow-up commands (boxed to avoid infinite size)
                                let session_clone = session_arc.clone();
                                let protocol_clone = protocol.clone();
                                let llm_clone = llm_client.clone();
                                let app_clone = app_state.clone();
                                let status_clone = status_tx.clone();
                                let data_clone = client_data.clone();

                                if let Err(e) = Box::pin(Self::execute_ssh_action(
                                    &session_clone,
                                    &protocol_clone,
                                    next_action,
                                    client_id,
                                    &llm_clone,
                                    &app_clone,
                                    &status_clone,
                                    &data_clone,
                                    depth + 1,
                                ))
                                .await
                                {
                                    error!("Error executing follow-up SSH action: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("LLM error for SSH client {}: {}", client_id, e);
                        }
                    }
                }

                Ok(applied)
            }
            ClientActionResult::Custom { name, .. } => {
                warn!(
                    "SSH client {} has no handler for custom result '{}'",
                    client_id, name
                );
                Ok(SshApplied::Executed(format!(
                    "custom result '{name}' has no SSH handler"
                )))
            }
            ClientActionResult::Disconnect => {
                info!("SSH client {} disconnecting", client_id);
                let _ = status_tx.send(format!("[CLIENT] SSH client {} disconnecting", client_id));
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                // Drop the command handle here rather than only in the command loop:
                // the LLM can disconnect too, and a handle left behind would offer
                // [ send ] into a closed session.
                app_state.remove_client_handle(client_id).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());

                // Close session
                let session = session_arc.lock().await;
                session
                    .disconnect(Disconnect::ByApplication, "", "en")
                    .await?;

                Ok(SshApplied::Disconnected)
            }
            ClientActionResult::WaitForMore => {
                // No-op for SSH (commands are discrete)
                Ok(SshApplied::Executed("wait_for_more".to_string()))
            }
            other => {
                warn!("Unexpected action result for SSH client");
                Ok(SshApplied::Executed(format!(
                    "unhandled action result {other:?}"
                )))
            }
        }
    }
}
