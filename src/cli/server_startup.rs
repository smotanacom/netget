//! Server startup logic for TUI mode
//!
//! Handles spawning TCP and HTTP servers based on application state

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::events::ActionExecutionError;
use crate::llm::OllamaClient;
use crate::protocol::metadata::DevelopmentState;
use crate::state::app_state::AppState;
use crate::state::ServerId;

/// Format the refusal message when a protocol is below the operator's
/// `--min-stability` floor.
///
/// Names the protocol, its actual declared state and the required minimum, so
/// the reason is legible in the TUI, the status stream and any MCP tool error —
/// mirroring how the privilege and dependency gates report a refusal. Shared
/// with `client_startup` so servers and clients speak with one voice.
pub(crate) fn min_stability_refusal(
    kind: &str,
    protocol: &str,
    actual: DevelopmentState,
    min: DevelopmentState,
) -> String {
    format!(
        "Cannot start {protocol} {kind}: its stability is {} but --min-stability requires at least \
         {}. Raise the protocol's maturity or lower --min-stability.",
        actual.as_str(),
        min.as_str()
    )
}

/// Did `spawn()` actually bind a listening socket?
///
/// `ProtocolServer::spawn` returns a `SocketAddr` for every protocol, but not
/// every protocol listens: WebRTC is peer-to-peer and returns a placeholder
/// `0.0.0.0:0`, and a couple of others fall back to the same placeholder when
/// they have no address to report. Port 0 is never a real bound port (the OS
/// resolves it to a concrete one at bind time), so it is an exact test for the
/// placeholder - and printing "listening on 0.0.0.0:0" for those servers told
/// the user, the TUI and every log reader something untrue.
fn is_bound_addr(addr: &SocketAddr) -> bool {
    addr.port() != 0
}

/// Check if an error is due to address already in use
fn is_addr_in_use_error(err: &anyhow::Error) -> bool {
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        io_err.kind() == std::io::ErrorKind::AddrInUse
    } else {
        false
    }
}

/// Start a specific server by ID
pub async fn start_server_by_id(
    state: &AppState,
    server_id: ServerId,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Result<(), ActionExecutionError> {
    // Get server info
    let server = match state.get_server(server_id).await {
        Some(s) => s,
        None => {
            let _ = status_tx.send(format!("[ERROR] Server #{} not found", server_id.as_u32()));
            return Ok(());
        }
    };

    // Build listen address
    let listen_addr: SocketAddr = format!("127.0.0.1:{}", server.port)
        .parse()
        .map_err(|e| ActionExecutionError::Fatal(anyhow::anyhow!("Invalid address: {}", e)))?;

    let protocol_name = server.protocol_name.clone();

    // Actually spawn the server using the registry
    use crate::state::server::ServerStatus;

    // Get protocol implementation from registry
    let protocol = crate::protocol::server_registry::registry()
        .resolve(&protocol_name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Check privilege requirements before spawning
    let metadata = protocol.metadata();

    // === --min-stability gate ===
    // Refuse a protocol below the operator's maturity floor before any spawn.
    // This is the authoritative enforcement point: even if the LLM was somehow
    // offered a forbidden protocol, it cannot be started.
    if let Some(min) = state.get_min_stability().await {
        if metadata.state < min {
            let full_error = min_stability_refusal("server", &protocol_name, metadata.state, min);
            state
                .update_server_status(server_id, ServerStatus::Error(full_error.clone()))
                .await;
            let _ = status_tx.send(format!("[ERROR] {}", full_error));
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            return Err(ActionExecutionError::Fatal(anyhow::anyhow!(full_error)));
        }
    }

    let system_caps = state.get_system_capabilities().await;

    // Decide whether this start is blocked for lack of privilege.
    //
    // `is_met_by()` is the single source of truth: it maps each requirement onto
    // the capability that actually satisfies it (`RawSockets` -> raw-socket
    // access, `Root` -> root, `PrivilegedPort` -> privileged-port binding).
    // Anything else ANDed in here silently overrides it - in particular, gating a
    // `RawSockets` protocol on `can_bind_privileged_ports` let it through on a
    // host that cannot open raw sockets.
    let privilege_met = metadata.privilege_requirement.is_met_by(&system_caps);

    let requires_privileges = match &metadata.privilege_requirement {
        crate::protocol::metadata::PrivilegeRequirement::PrivilegedPort(_) => {
            // Only require privileges if actually binding to a privileged port.
            // Port 0 means OS-assigned port, which will always be unprivileged (>1024)
            server.port > 0 && server.port < 1024 && !privilege_met
        }
        // RawSockets, Root, None: the requirement itself is the whole test.
        _ => !privilege_met,
    };

    if requires_privileges {
        let error_msg = format!(
            "Cannot start {} server on port {}: {}. Current capabilities: {}",
            protocol_name,
            server.port,
            metadata.privilege_requirement.description(),
            system_caps.description()
        );

        // Provide helpful suggestion based on platform
        let suggestion = if cfg!(target_os = "linux") {
            match &metadata.privilege_requirement {
                crate::protocol::metadata::PrivilegeRequirement::PrivilegedPort(port) => {
                    format!("\nSuggestion: Run as root (sudo) or use a port >= 1024 (e.g., {}, {}, {})",
                        port + 8000, port + 10000, 8080)
                }
                crate::protocol::metadata::PrivilegeRequirement::RawSockets => {
                    "\nSuggestion: Run as root or grant CAP_NET_RAW: sudo setcap cap_net_raw+ep /path/to/netget".to_string()
                }
                crate::protocol::metadata::PrivilegeRequirement::Root => {
                    "\nSuggestion: Run as root (sudo netget ...)".to_string()
                }
                _ => String::new(),
            }
        } else if cfg!(target_os = "macos") {
            "\nSuggestion: Run as root (sudo netget ...)".to_string()
        } else {
            "\nSuggestion: Run as Administrator".to_string()
        };

        let full_error = format!("{}{}", error_msg, suggestion);

        state
            .update_server_status(server_id, ServerStatus::Error(full_error.clone()))
            .await;
        let _ = status_tx.send(format!("[ERROR] {}", full_error));
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        return Err(ActionExecutionError::PrivilegeDenied {
            requirement: metadata.privilege_requirement.description(),
            message: full_error,
        });
    }

    // Non-privilege dependencies: a missing system library or tool in PATH.
    //
    // `Protocol::get_dependencies()` derives from `privilege_requirement`, which the block
    // above already enforced — re-checking those here would report the same failure twice with
    // a worse message. What it cannot express is a library or binary the protocol needs at
    // runtime, which a protocol supplies by overriding `get_dependencies()`. Those had no gate
    // at all: the TUI and the model's protocol list would exclude such a protocol while a
    // direct `start_server` still attempted it and failed with whatever raw error the
    // underlying library produced.
    //
    // Only definitively-unmet dependencies refuse. `DeviceAccess` deliberately derives no
    // dependency because no probe can answer honestly for an adapter or reader, and this gate
    // must never turn "we could not tell" into "no".
    let missing: Vec<_> = protocol
        .get_dependencies()
        .into_iter()
        .filter(|dep| {
            matches!(
                dep,
                crate::protocol::dependencies::ProtocolDependency::SystemLibrary(_)
                    | crate::protocol::dependencies::ProtocolDependency::ToolInPath(_)
            )
        })
        .filter(|dep| !dep.is_available(&system_caps))
        .collect();

    if let Some(dep) = missing.first() {
        let full_error = format!(
            "Cannot start {} server on port {}: {}. {}",
            protocol_name,
            server.port,
            dep.description(),
            dep.installation_hint()
        );

        state
            .update_server_status(server_id, ServerStatus::Error(full_error.clone()))
            .await;
        let _ = status_tx.send(format!("[ERROR] {}", full_error));
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        return Err(ActionExecutionError::PrivilegeDenied {
            requirement: dep.name(),
            message: full_error,
        });
    }

    // Build type-safe startup params if provided
    //
    // The JSON comes from the LLM or an MCP client, so a bad key must become a
    // reported error (and an `Error` server status), never a panic.
    let startup_params = if let Some(params_json) = server.startup_params.clone() {
        // Get the parameter schema from the protocol
        let schema = protocol.get_startup_parameters();
        // Create validated StartupParams
        match crate::protocol::StartupParams::new(params_json, schema) {
            Ok(p) => Some(p),
            Err(e) => {
                let msg = format!("Invalid startup_params for {}: {}", protocol_name, e);
                state
                    .update_server_status(server_id, ServerStatus::Error(msg.clone()))
                    .await;
                let _ = status_tx.send(format!("[ERROR] {}", msg));
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                return Err(ActionExecutionError::Fatal(anyhow::anyhow!(msg)));
            }
        }
    } else {
        None
    };

    // Build spawn context
    // NOTE: This is the legacy path for unmigrated protocols
    // Migrated protocols should be started via start_server_from_action
    #[allow(deprecated)]
    let spawn_ctx = crate::protocol::SpawnContext {
        listen_addr,
        mac_address: None,
        interface: None,
        host: None,
        port: None,
        // Attach the per-server status channel so event-template lifecycle logs
        // (rendered via EventLogContext) reach the TUI, not just netget.log.
        llm_client: llm_client.clone().with_status_tx(status_tx.clone()),
        state: Arc::new(state.clone()),
        status_tx: status_tx.clone(),
        server_id,
        startup_params,
    };

    // Spawn the server using the protocol's spawn method
    match protocol.spawn(spawn_ctx).await {
        Ok(actual_addr) => {
            let bound = is_bound_addr(&actual_addr);

            // Send startup message with actual port
            let msg = if bound {
                format!(
                    "[SERVER] Starting server #{} ({}) on {}",
                    server_id.as_u32(),
                    protocol_name,
                    actual_addr
                )
            } else {
                format!(
                    "[SERVER] Starting server #{} ({}) (no listening socket)",
                    server_id.as_u32(),
                    protocol_name
                )
            };
            let _ = status_tx.send(msg);

            // Update server with actual listen address (only if one was bound;
            // recording the 0.0.0.0:0 placeholder would surface it in the TUI
            // and in server_status as though it were an endpoint)
            if bound {
                state.update_server_local_addr(server_id, actual_addr).await;
            }
            state
                .update_server_status(server_id, ServerStatus::Running)
                .await;
            // Send update message with actual bound address (for tests that use port 0)
            if bound && (server.port == 0 || server.port != actual_addr.port()) {
                let update_msg = format!(
                    "[SERVER] Server #{} ({}) listening on {}",
                    server_id.as_u32(),
                    protocol_name,
                    actual_addr
                );
                let _ = status_tx.send(update_msg);
            }
            // Note: protocol-specific "listening on" message is also sent by the protocol's spawn method to tracing
            let _ = status_tx.send("__UPDATE_UI__".to_string());
        }
        Err(e) => {
            // Check if error is due to port already in use
            if is_addr_in_use_error(&e) {
                // Return retryable error with context for LLM
                let _ = status_tx.send(format!(
                    "[INFO] Port {} is already in use for {} server, will retry with LLM suggestion",
                    server.port,
                    protocol_name
                ));
                return Err(ActionExecutionError::PortConflict {
                    port: server.port,
                    protocol: protocol_name.clone(),
                    underlying_error: e.to_string(),
                });
            }

            // For other errors, fail immediately
            state
                .update_server_status(server_id, ServerStatus::Error(e.to_string()))
                .await;
            let _ = status_tx.send(format!(
                "[ERROR] Failed to start {} server #{}: {}",
                protocol_name,
                server_id.as_u32(),
                e
            ));
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            return Err(ActionExecutionError::Fatal(e));
        }
    }

    Ok(())
}

/// Start a server from action parameters (used by /load command)
/// Returns the server ID on success
#[allow(clippy::too_many_arguments)]
pub async fn start_server_from_action(
    state: &AppState,
    // NEW: Flexible binding parameters (all optional)
    mac_address: Option<String>,
    interface: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    protocol: &str,
    send_first: bool,
    initial_memory: Option<String>,
    instruction: String,
    startup_params: Option<serde_json::Value>,
    event_handlers: Option<Vec<serde_json::Value>>,
    scheduled_tasks: Option<Vec<crate::llm::actions::common::ServerTaskDefinition>>,
    feedback_instructions: Option<String>,
    status_tx: mpsc::UnboundedSender<String>,
) -> Result<ServerId> {
    use crate::state::server::ServerStatus;

    // Resolve the protocol from the registry. `resolve()` is case-insensitive and
    // distinguishes "compiled out of this build" from "no such protocol", with a
    // did-you-mean suggestion for the latter.
    let registry = crate::protocol::server_registry::registry();
    let protocol_impl = registry
        .resolve(protocol)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // === --min-stability gate ===
    // Refuse a protocol below the operator's maturity floor before building
    // anything — no server instance exists yet, so there is nothing to strand.
    if let Some(min) = state.get_min_stability().await {
        let actual = protocol_impl.metadata().state;
        if actual < min {
            let msg = min_stability_refusal("server", protocol, actual, min);
            let _ = status_tx.send(format!("[ERROR] {}", msg));
            return Err(anyhow::anyhow!(msg));
        }
    }

    // === send_first ===
    //
    // `send_first` is a top-level parameter of this function (and of the
    // `open_server` action / MCP `start_server` tool), but protocols consume it
    // as a startup parameter: TCP, Redis, PostgreSQL, MySQL, MongoDB, JSON-RPC,
    // IRC and LDAP all read `startup_params.send_first`. Fold the top-level flag
    // into `startup_params` so the two spellings mean the same thing.
    //
    // Only inject when the protocol actually declares the parameter -
    // `StartupParams` rejects undeclared keys. An explicit value already present
    // in `startup_params` wins, so callers can still set it per-protocol.
    let declares_send_first = protocol_impl
        .get_startup_parameters()
        .iter()
        .any(|p| p.name == "send_first");

    let startup_params = if send_first && declares_send_first {
        let mut params = startup_params.unwrap_or_else(|| serde_json::json!({}));
        match params.as_object_mut() {
            Some(map) => {
                map.entry("send_first")
                    .or_insert(serde_json::Value::Bool(true));
                Some(params)
            }
            None => {
                return Err(anyhow::anyhow!(
                    "startup_params must be a JSON object, got: {}",
                    params
                ));
            }
        }
    } else {
        if send_first && !declares_send_first {
            let _ = status_tx.send(format!(
                "[WARN] Protocol '{}' does not support send_first; ignoring it",
                protocol
            ));
            tracing::warn!(
                "send_first requested for protocol '{}', which declares no send_first startup parameter - ignoring",
                protocol
            );
        }
        startup_params
    };

    // === Validate startup params BEFORE registering the server ===
    //
    // This JSON comes straight from the LLM (`open_server`) or an MCP client
    // (`start_server`), so it is untrusted. Validating here means a malformed
    // request returns an error to the caller and leaves no `ServerInstance`
    // stranded in `ServerStatus::Starting`.
    let startup_params_obj = match startup_params.clone() {
        Some(params_json) => {
            let schema = protocol_impl.get_startup_parameters();
            Some(
                crate::protocol::StartupParams::new(params_json, schema).map_err(|e| {
                    anyhow::anyhow!("Invalid startup_params for {}: {}", protocol, e)
                })?,
            )
        }
        None => None,
    };

    // === DUAL PATH LOGIC: Migrated vs Unmigrated Protocols ===
    //
    // Migrated protocols return Some(...) from default_binding()
    // Unmigrated protocols return None and use legacy listen_addr
    //
    let (final_mac, final_interface, final_host, final_port, _use_new_path, listen_addr) =
        if let Some(defaults) = protocol_impl.default_binding() {
            // NEW PATH: Protocol has been migrated, use flexible binding
            let (mac, iface, host_str, port_num) =
                defaults.apply(mac_address.clone(), interface.clone(), host.clone(), port);

            // For port-based protocols with port 0, find available port
            //
            // KNOWN RACE, and the same one exists in the unmigrated path below. This binds
            // a probe listener, reads the port it was given, drops it, and lets the
            // protocol bind that port a moment later. Between the drop and the real bind
            // the port belongs to nobody, so a concurrent starter can take it: under a
            // hundred parallel tests, `ServerForm::create` fails roughly once every few
            // runs with "Address already in use (os error 48)" for a request that asked
            // for port 0, which is the one request that should never be able to collide.
            //
            // The fix is to stop resolving it here and pass 0 through to the protocol's
            // own bind, so the OS assigns it atomically -- `spawn()` already returns the
            // bound address, and the reconciliation further down already records a port
            // that differs from what was asked for. It is left alone here only because it
            // is on the startup path of all 116 protocols and the server suite cannot be
            // run to completion in this environment to prove the change out.
            let final_port_num = if let Some(p) = port_num {
                if p == 0 {
                    // Find available port
                    use tokio::net::TcpListener;
                    let bind_host = host_str.as_deref().unwrap_or("127.0.0.1");
                    let listener = TcpListener::bind(format!("{}:0", bind_host))
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to find available port: {}", e))?;
                    let found_port = listener
                        .local_addr()
                        .map_err(|e| anyhow::anyhow!("Failed to get local address: {}", e))?
                        .port();
                    drop(listener);
                    Some(found_port)
                } else {
                    Some(p)
                }
            } else {
                None
            };

            // Construct legacy listen_addr for backwards compatibility
            // (protocols still receive this field, but new protocols should ignore it)
            let legacy_addr = match (&host_str, final_port_num) {
                (Some(h), Some(p)) => format!("{}:{}", h, p)
                    .parse()
                    .unwrap_or_else(|_| "127.0.0.1:0".parse().unwrap()),
                _ => "127.0.0.1:0".parse().unwrap(),
            };

            (mac, iface, host_str, final_port_num, true, legacy_addr)
        } else {
            // OLD PATH: Protocol hasn't been migrated, use backwards-compatible behavior
            // Port field is required for unmigrated protocols
            let port_value = port.ok_or_else(|| {
                anyhow::anyhow!(
                    "Protocol '{}' requires 'port' parameter (unmigrated protocol)",
                    protocol
                )
            })?;

            // If port is 0, find an available port automatically
            let actual_port = if port_value == 0 {
                use tokio::net::TcpListener;
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to find available port: {}", e))?;
                let found_port = listener
                    .local_addr()
                    .map_err(|e| anyhow::anyhow!("Failed to get local address: {}", e))?
                    .port();
                drop(listener);
                found_port
            } else {
                port_value
            };

            // Get default listen address (always 127.0.0.1 for security)
            let listen_addr: SocketAddr = format!("127.0.0.1:{}", actual_port)
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid port: {}", e))?;

            (
                None,
                None,
                Some("127.0.0.1".to_string()),
                Some(actual_port),
                false,
                listen_addr,
            )
        };

    // Check privilege requirements
    let metadata = protocol_impl.metadata();
    let system_caps = state.get_system_capabilities().await;

    // Decide whether this start is blocked for lack of privilege.
    //
    // As in `start_server_by_id`, `is_met_by()` is the only test: it already maps
    // each requirement onto the capability that satisfies it. ANDing an unrelated
    // capability on top (e.g. `can_bind_privileged_ports` for a `RawSockets`
    // protocol) let unprivileged starts through.
    let privilege_met = metadata.privilege_requirement.is_met_by(&system_caps);

    let requires_privileges = match &metadata.privilege_requirement {
        crate::protocol::metadata::PrivilegeRequirement::PrivilegedPort(_) => {
            // Only require privileges if actually binding to a privileged port.
            // Port 0 means OS-assigned port, which will always be unprivileged (>1024)
            // final_port has already been resolved to an actual port number
            let privileged_port = match final_port {
                Some(p) => p > 0 && p < 1024,
                None => false, // Interface-based protocols don't use ports
            };
            privileged_port && !privilege_met
        }
        // RawSockets, Root, None: the requirement itself is the whole test.
        _ => !privilege_met,
    };

    if requires_privileges {
        let error_msg = format!(
            "Cannot start {} server: {}. Current capabilities: {}",
            protocol,
            metadata.privilege_requirement.description(),
            system_caps.description()
        );
        return Err(anyhow::anyhow!(error_msg));
    }

    // Create server instance
    // NOTE: For unmigrated protocols, port is always Some(_)
    // For migrated protocols, port may be None (interface-based protocols like ICMP)
    let display_port = final_port.unwrap_or(0);

    let server = crate::state::server::ServerInstance {
        id: ServerId::new(0), // Will be assigned by add_server
        port: display_port,
        protocol_name: protocol.to_string(),
        instruction: instruction.clone(),
        memory: String::new(),
        status: ServerStatus::Starting,
        connections: Default::default(),
        local_addr: None,
        created_at: std::time::Instant::now(),
        status_changed_at: std::time::Instant::now(),
        startup_params: startup_params.clone(),
        event_handler_config: None,
        protocol_data: serde_json::Value::Null,
        log_files: Default::default(),
        feedback_instructions,
        feedback_buffer: Vec::new(),
        last_feedback_processed: None,
        recent_connections: Default::default(),
        connection_opened_at: Default::default(),
    };

    let server_id = state.add_server(server).await;

    // Set initial memory if provided
    if let Some(mem) = initial_memory {
        state.set_memory(server_id, mem).await;
    }

    // Configure event handlers if provided
    if let Some(handlers) = event_handlers {
        // Use proper validation that checks LLM handler instruction field
        match crate::events::handler::EventHandler::parse_event_handlers(handlers) {
            Ok(config) => {
                state
                    .with_server_mut(server_id, |s| {
                        s.event_handler_config = Some(config);
                    })
                    .await;
                let _ = status_tx.send("[INFO] Event handler configuration applied".to_string());
            }
            Err(e) => {
                // Return error instead of just warning - invalid config should fail
                return Err(anyhow::anyhow!(
                    "Invalid event handler configuration: {}",
                    e
                ));
            }
        }
    }

    // Create scheduled tasks if provided
    if let Some(tasks) = scheduled_tasks {
        for task_def in tasks {
            use crate::state::task::{ScheduledTask, TaskId, TaskScope, TaskStatus, TaskType};
            use std::time::{Duration, Instant};

            // Determine task type
            let task_type = if task_def.recurring {
                TaskType::Recurring {
                    interval_secs: task_def.interval_secs.unwrap_or(60),
                    max_executions: task_def.max_executions,
                    executions_count: 0,
                }
            } else {
                TaskType::OneShot {
                    delay_secs: task_def.delay_secs.unwrap_or(0),
                }
            };

            // Calculate next execution time
            let delay = if task_def.recurring {
                Duration::from_secs(0) // Start immediately for recurring
            } else {
                Duration::from_secs(task_def.delay_secs.unwrap_or(0))
            };

            let task_name = task_def.task_id.clone();

            let task = ScheduledTask {
                id: TaskId::new(rand::random()),
                name: task_def.task_id,
                scope: TaskScope::Server(server_id),
                task_type,
                instruction: task_def.instruction,
                context: task_def.context,
                status: TaskStatus::Scheduled,
                created_at: Instant::now(),
                next_execution: Instant::now() + delay,
                last_error: None,
                failure_count: 0,
            };

            let task_id_num = state.add_task(task).await;

            // Report it exactly as the standalone `schedule_task` action does
            // (src/events/handler.rs). Creating a task through `open_server`'s
            // `scheduled_tasks` array used to be completely silent - nothing in
            // the TUI, nothing over MCP - so the two paths disagreed about
            // whether task creation is an observable event. It is.
            if task_def.recurring {
                let interval = task_def.interval_secs.unwrap_or(60);
                let max_info = match task_def.max_executions {
                    Some(max) => format!(" (max {} executions)", max),
                    None => String::new(),
                };
                let msg = format!(
                    "[TASK] Scheduled recurring task '{}' (ID: {}) to execute every {}s{}",
                    task_name, task_id_num, interval, max_info
                );
                tracing::info!("{}", msg);
                let _ = status_tx.send(msg);
            } else {
                let msg = format!(
                    "[TASK] Scheduled one-shot task '{}' (ID: {}) to execute in {}s",
                    task_name,
                    task_id_num,
                    task_def.delay_secs.unwrap_or(0)
                );
                tracing::info!("{}", msg);
                let _ = status_tx.send(msg);
            }
        }
    }

    // `startup_params_obj` was built and validated above, before `add_server`.

    // Build spawn context
    // Use the configured LLM client from state (includes mock config, lock settings, etc.)
    // If not available (shouldn't happen), fall back to creating a new client
    let llm_client = if let Some(client) = state.get_llm_client().await {
        client
    } else {
        let ollama_url = state.get_ollama_url().await;
        OllamaClient::new(ollama_url)
    }
    // Attach the per-server status channel so event-template lifecycle logs
    // (rendered via EventLogContext) reach the TUI, not just netget.log.
    .with_status_tx(status_tx.clone());

    #[allow(deprecated)]
    let spawn_ctx = crate::protocol::SpawnContext {
        listen_addr,
        mac_address: final_mac,
        interface: final_interface,
        host: final_host,
        port: final_port,
        llm_client,
        state: Arc::new(state.clone()),
        status_tx: status_tx.clone(),
        server_id,
        startup_params: startup_params_obj,
    };

    // Spawn the server
    match protocol_impl.spawn(spawn_ctx).await {
        Ok(actual_addr) => {
            let bound = is_bound_addr(&actual_addr);

            // Send startup message with actual address
            let msg = if bound {
                format!(
                    "[SERVER] Starting server #{} ({}) on {}",
                    server_id.as_u32(),
                    protocol,
                    actual_addr
                )
            } else {
                format!(
                    "[SERVER] Starting server #{} ({}) (no listening socket)",
                    server_id.as_u32(),
                    protocol
                )
            };
            let _ = status_tx.send(msg);

            // Update server with actual listen address (only if one was bound;
            // see is_bound_addr)
            if bound {
                state.update_server_local_addr(server_id, actual_addr).await;
            }
            state
                .update_server_status(server_id, ServerStatus::Running)
                .await;

            // Send update message with actual bound address (for tests that use port 0)
            if bound
                && (final_port.unwrap_or(0) == 0 || final_port.unwrap_or(0) != actual_addr.port())
            {
                let update_msg = format!(
                    "[SERVER] Server #{} ({}) listening on {}",
                    server_id.as_u32(),
                    protocol,
                    actual_addr
                );
                let _ = status_tx.send(update_msg);
            }

            Ok(server_id)
        }
        Err(e) => {
            // The server never came up, so nothing about it is inspectable or
            // controllable: it has no listener, no connections and no task. Leaving
            // the instance registered only makes `list_servers` accumulate zombie
            // `Error` rows that can never transition anywhere, each holding a
            // ServerId and any log files it opened. The error is returned to the
            // caller (and, for MCP, becomes the tool error), so dropping the
            // registration loses nothing.
            state
                .update_server_status(server_id, ServerStatus::Error(e.to_string()))
                .await;
            state.remove_server(server_id).await;
            let _ = status_tx.send(format!(
                "[ERROR] Server #{} ({}) failed to start: {}",
                server_id.as_u32(),
                protocol,
                e
            ));
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            Err(e)
        }
    }
}
