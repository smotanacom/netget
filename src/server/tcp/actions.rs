//! TCP protocol actions implementation

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;

/// Connection data for TCP protocol
pub struct ConnectionData {
    pub write_half: Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
}

/// TCP protocol action handler
pub struct TcpProtocol {
    /// Map of active connections (for async actions)
    connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
}

impl Default for TcpProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpProtocol {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_connections(
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
    ) -> Self {
        Self { connections }
    }

    /// Add a connection to the protocol handler
    pub async fn add_connection(
        &self,
        connection_id: ConnectionId,
        write_half: Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
    ) {
        self.connections
            .lock()
            .await
            .insert(connection_id, ConnectionData { write_half });
    }

    /// Remove a connection from the protocol handler
    pub async fn remove_connection(&self, connection_id: &ConnectionId) {
        self.connections.lock().await.remove(connection_id);
    }

    /// Get list of active connection IDs
    pub async fn list_connection_ids(&self) -> Vec<ConnectionId> {
        self.connections.lock().await.keys().copied().collect()
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for TcpProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        vec![
                crate::llm::actions::ParameterDefinition {
                    name: "send_first".to_string(),
                    type_hint: "boolean".to_string(),
                    description: "Whether the server should send the first message after connection (e.g., for FTP/SMTP greeting banners)".to_string(),
                    required: false,
                    example: serde_json::json!(false),
                },
            ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            send_to_connection_action(),
            close_connection_action(),
            list_connections_action(),
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_tcp_data_action(),
            wait_for_more_action(),
            close_this_connection_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "TCP"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_tcp_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        // No "ftp": it was here from before a real FTP protocol existed, and a request for an
        // FTP server resolved to raw TCP. FTP is its own protocol now and owns that keyword.
        vec!["tcp", "raw", "custom"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Beta)
            .implementation("Manual TCP socket handling with tokio")
            .llm_control("Full byte stream control - all sent/received data")
            .e2e_testing("tokio::net::TcpStream")
            .notes("Basis for FTP, SMTP, custom protocols")
            .build()
    }
    fn description(&self) -> &'static str {
        "Raw TCP socket server for custom protocols"
    }
    fn example_prompt(&self) -> &'static str {
        "Pretend to be FTP server on port 2121; serve file accounts.csv with 'balance,0'"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // The textbook deterministic case: echo received bytes straight back.
        // Inbound 'data' is hex-encoded when the bytes are not printable, so the
        // handler echoes the 'encoding' field back unchanged to stay symmetric.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "tcp_data_received":
    actions = [{"type": "send_tcp_data",
                "data": event.get("data", ""),
                "encoding": event.get("encoding", "utf8")}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: dynamic, per-line reasoning over a raw TCP stream.
            json!({
                "type": "open_server",
                "port": 9000,
                "base_stack": "tcp",
                "instruction": "Act as a line-based chat server: send a greeting when a client connects, and for each line of text the client sends, reply in character with a short response that references what they said."
            }),
            // Script mode: a pure echo server, no LLM call.
            json!({
                "type": "open_server",
                "port": 9000,
                "base_stack": "tcp",
                "event_handlers": [{
                    "event_pattern": "tcp_data_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }]
            }),
            // Static mode: Fixed responses
            json!({
                "type": "open_server",
                "port": 9000,
                "base_stack": "tcp",
                "event_handlers": [
                    {
                        "event_pattern": "tcp_connection_opened",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "send_tcp_data",
                                "data": "220 Welcome\r\n"
                            }]
                        }
                    },
                    {
                        "event_pattern": "tcp_data_received",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "send_tcp_data",
                                "data": "OK\r\n"
                            }]
                        }
                    }
                ]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for TcpProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            // Extract send_first from startup_params
            let send_first = ctx
                .startup_params
                .as_ref()
                .map(|p| p.get_optional_bool("send_first"))
                .transpose()?
                .flatten()
                .unwrap_or(false);

            use crate::server::tcp::TcpServer;
            let listen_addr = ctx.legacy_listen_addr();
            TcpServer::spawn_with_llm_actions(
                listen_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                send_first,
                ctx.server_id,
            )
            .await
        })
    }
    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "send_to_connection" => {
                // Async action - not fully implemented here, needs to be handled by caller
                // because we need async context to send data
                let connection_id_str = action
                    .get("connection_id")
                    .and_then(|v| v.as_str())
                    .context("Missing 'connection_id' parameter")?;

                let data = action
                    .get("data")
                    .and_then(|v| v.as_str())
                    .context("Missing 'data' parameter")?;

                let _connection_id = ConnectionId::from_string(connection_id_str)
                    .context("Invalid connection_id format")?;

                // Return the data with connection ID embedded
                // The caller will need to handle actually sending it
                Ok(ActionResult::Output(decode_outbound_data(data, &action)?))
            }
            "close_connection" => {
                // Async action - signal that connection should be closed
                let connection_id_str = action
                    .get("connection_id")
                    .and_then(|v| v.as_str())
                    .context("Missing 'connection_id' parameter")?;

                let _connection_id = ConnectionId::from_string(connection_id_str)
                    .context("Invalid connection_id format")?;

                Ok(ActionResult::CloseConnection)
            }
            "list_connections" => {
                // This needs to be handled specially by the caller
                Ok(ActionResult::NoAction)
            }
            "send_tcp_data" => self.execute_send_tcp_data(action),
            "wait_for_more" => Ok(ActionResult::WaitForMore),
            "close_this_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown TCP action: {action_type}")),
        }
    }
}

impl TcpProtocol {
    /// Execute send_tcp_data sync action
    fn execute_send_tcp_data(&self, action: serde_json::Value) -> Result<ActionResult> {
        let data = action
            .get("data")
            .and_then(|v| v.as_str())
            .context("Missing 'data' parameter")?;

        Ok(ActionResult::Output(decode_outbound_data(data, &action)?))
    }
}

/// Turn the `data` field of an outbound action into the exact bytes to put on the wire,
/// honouring the action's optional `encoding` field.
///
/// - `encoding` absent or `"utf8"`: the string's UTF-8 bytes are sent verbatim (default,
///   backwards compatible).
/// - `encoding` = `"hex"`: `data` is decoded as hex, so `"48656c6c6f"` sends the 5 bytes
///   `Hello`.
///
/// There is deliberately no auto-detection: `"48656c6c6f"` is both valid text and valid
/// hex, so the caller must say which it means.
fn decode_outbound_data(data: &str, action: &serde_json::Value) -> Result<Vec<u8>> {
    let encoding = action
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("utf8");

    match encoding {
        "utf8" => Ok(data.as_bytes().to_vec()),
        "hex" => {
            // Tolerate whitespace/`0x` grouping that models frequently emit.
            let cleaned: String = data
                .chars()
                .filter(|c| !c.is_ascii_whitespace() && *c != ':')
                .collect();
            let cleaned = cleaned.strip_prefix("0x").unwrap_or(&cleaned);

            if cleaned.len() % 2 != 0 {
                return Err(anyhow::anyhow!(
                    "Invalid hex in 'data': expected an even number of hex digits, got {} \
                     ({data:?}). Each byte is two hex digits, e.g. \"48656c6c6f\" = \"Hello\".",
                    cleaned.len()
                ));
            }

            hex::decode(cleaned).map_err(|e| {
                anyhow::anyhow!(
                    "Invalid hex in 'data' ({data:?}): {e}. Use only 0-9/a-f, two digits per \
                     byte, e.g. \"48656c6c6f\" = \"Hello\". To send this string as literal \
                     text, omit 'encoding' or set it to \"utf8\"."
                )
            })
        }
        other => Err(anyhow::anyhow!(
            "Invalid 'encoding' value {other:?}. Valid values are \"utf8\" (default, send the \
             string's characters as-is) and \"hex\" (decode the string as hex-encoded bytes)."
        )),
    }
}

/// Shared `encoding` parameter for every action that carries an outbound `data` field.
///
/// Declared as a closed choice set (`with_choices`): the executor accepts exactly
/// `utf8` and `hex` (see [`decode_outbound_data`]), so the TUI composer offers a
/// selector and the native tool schemas advertise the pair as an enum.
fn encoding_parameter() -> Parameter {
    Parameter {
        name: "encoding".to_string(),
        type_hint: "string".to_string(),
        description: "How to convert 'data' into the bytes put on the wire. \"utf8\" (the default when omitted) sends the characters of 'data' unchanged - use it for text protocols such as FTP/SMTP/HTTP. \"hex\" decodes 'data' as hex-encoded bytes, two hex digits per byte - use it for binary protocols, e.g. {\"data\": \"48656c6c6f\", \"encoding\": \"hex\"} sends the 5 bytes 'Hello', whereas the same 'data' without \"encoding\": \"hex\" sends the 10 characters 4-8-6-5-6-c-6-c-6-f. No other values are accepted".to_string(),
        required: false,
    }
    .with_choices(["utf8", "hex"])
}

/// Action definition for send_to_connection (async)
fn send_to_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_to_connection".to_string(),
        description: "Send data to a specific TCP connection (async action)".to_string(),
        parameters: vec![
            Parameter {
                name: "connection_id".to_string(),
                type_hint: "string".to_string(),
                description: "Connection ID to send to".to_string(),
                required: true,
            },
            Parameter {
                name: "data".to_string(),
                type_hint: "string".to_string(),
                description: "Data to send. Interpreted according to 'encoding': by default the characters of this string are sent as-is (UTF-8)".to_string(),
                required: true,
            },
            encoding_parameter(),
        ],
        example: json!({
            "type": "send_to_connection",
            "connection_id": "conn-12345",
            "data": "Hello from TCP",
            "encoding": "utf8"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> TCP to {connection_id}")
                .with_debug("TCP send_to_connection: connection_id={connection_id}"),
        ),
    }
}

/// Action definition for close_connection (async)
fn close_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_connection".to_string(),
        description: "Close a specific TCP connection (async action)".to_string(),
        parameters: vec![Parameter {
            name: "connection_id".to_string(),
            type_hint: "string".to_string(),
            description: "Connection ID to close".to_string(),
            required: true,
        }],
        example: json!({
            "type": "close_connection",
            "connection_id": "conn-12345"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("TCP close connection {connection_id}")
                .with_debug("TCP close_connection: connection_id={connection_id}"),
        ),
    }
}

/// Action definition for list_connections (async)
fn list_connections_action() -> ActionDefinition {
    ActionDefinition {
        name: "list_connections".to_string(),
        description: "List all active TCP connections (async action)".to_string(),
        parameters: vec![],
        example: json!({
            "type": "list_connections"
        }),
        log_template: Some(LogTemplate::new().with_debug("TCP list_connections")),
    }
}

/// Action definition for send_tcp_data (sync)
fn send_tcp_data_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_tcp_data".to_string(),
        description: "IMPORTANT: Use this action to send data over TCP connections. This is the ONLY correct action for TCP responses - do NOT use generic 'send_data' or 'show_message' actions. The 'data' field holds the payload and the optional 'encoding' field says how to turn it into bytes: omit 'encoding' (or use \"utf8\") to send the string's characters as-is, or set \"encoding\": \"hex\" to send 'data' decoded from hex. There is no auto-detection - a string like \"48656c6c6f\" is sent literally unless you set \"encoding\": \"hex\".".to_string(),
        parameters: vec![
            Parameter {
                name: "data".to_string(),
                type_hint: "string".to_string(),
                description: "Payload to send over the TCP connection. Interpreted according to 'encoding': as literal text by default, or as hex-encoded bytes when \"encoding\": \"hex\"".to_string(),
                required: true,
            },
            encoding_parameter(),
        ],
        example: json!({
            "type": "send_tcp_data",
            "data": "220 Welcome\r\n",
            "encoding": "utf8"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> {output_bytes}B")
                .with_debug("TCP send {output_bytes}B")
                .with_trace("TCP send: {preview(data,200)}"),
        ),
    }
}

/// Action definition for wait_for_more (sync)
fn wait_for_more_action() -> ActionDefinition {
    ActionDefinition {
        name: "wait_for_more".to_string(),
        // The old text was "accumulate incomplete protocol data", which promises something the
        // server does not do: WaitForMore only sets ConnectionState::Accumulating, and the
        // payload you were shown is dropped. Only bytes arriving *during* the LLM call are
        // merged in, via queued_data. Kept word-for-word in step with the QUIC action of the
        // same name - the two raw-stream protocols should not drift apart on this.
        description: "Answer this event with nothing and wait for the peer to send more. Use \
            it when the bytes you were given are an incomplete message. IMPORTANT: the bytes \
            in THIS event are not repeated in the next one - each event carries only what \
            has arrived since the last. If you need this fragment to make sense of what comes \
            next, put it in your memory now, because you will not be shown it again."
            .to_string(),
        parameters: vec![],
        example: json!({
            "type": "wait_for_more"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_debug("TCP waiting for more data")
                .with_trace("wait_for_more action"),
        ),
    }
}

/// Action definition for close_this_connection (sync)
fn close_this_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Close the current TCP connection".to_string(),
        parameters: vec![],
        example: json!({
            "type": "close_this_connection"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("TCP connection closed")
                .with_debug("TCP closing connection"),
        ),
    }
}

// ============================================================================
// TCP Action Constants
// ============================================================================

pub static SEND_TCP_DATA_ACTION: LazyLock<ActionDefinition> = LazyLock::new(send_tcp_data_action);
pub static WAIT_FOR_MORE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(wait_for_more_action);
pub static CLOSE_THIS_CONNECTION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(close_this_connection_action);

// ============================================================================
// TCP Event Type Constants
// ============================================================================

/// TCP connection opened event - triggered when new connection is established
pub static TCP_CONNECTION_OPENED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "tcp_connection_opened",
        "New TCP connection established (send initial greeting/banner if needed)",
        serde_json::json!({
            "type": "send_tcp_data",
            "data": "220 Welcome to server\r\n"
        }),
    )
    // No parameters - just connection opened notification
    .with_actions(vec![
        SEND_TCP_DATA_ACTION.clone(),
        CLOSE_THIS_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("TCP connection from {client_ip}:{client_port}")
            .with_debug("TCP connection opened from {client_ip}:{client_port}")
            .with_trace("TCP connection: {json_pretty(.)}"),
    )
});

/// TCP data received event - triggered when data is received on connection
pub static TCP_DATA_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "tcp_data_received",
        "Data received on TCP connection",
        serde_json::json!({
            "type": "send_tcp_data",
            "data": "48656c6c6f",
            "encoding": "hex"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "The data received from the client. Read it according to the 'encoding' field of this event".to_string(),
            required: true,
        },
        Parameter {
            name: "encoding".to_string(),
            type_hint: "string".to_string(),
            description: "How to read 'data': \"utf8\" means 'data' is the received bytes as literal text, \"hex\" means 'data' is the received bytes hex-encoded (two hex digits per byte, used whenever the bytes are not all printable ASCII). To echo the received bytes back unchanged, pass the same 'data' and 'encoding' to send_tcp_data".to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        SEND_TCP_DATA_ACTION.clone(),
        WAIT_FOR_MORE_ACTION.clone(),
        CLOSE_THIS_CONNECTION_ACTION.clone(),
    ])
    .with_alternative_example(serde_json::json!({
        "type": "wait_for_more"
    }))
    .with_alternative_example(serde_json::json!({
        "type": "close_connection"
    }))
    .with_log_template(
        LogTemplate::new()
            .with_info("{client_ip}:{client_port} <- {data_len}B -> {response_bytes}B")
            .with_debug("TCP received {data_len}B from {client_ip}:{client_port}")
            .with_trace("TCP data: {preview(data,200)}"),
    )
});

/// Get TCP event types
pub fn get_tcp_event_types() -> Vec<EventType> {
    vec![
        TCP_CONNECTION_OPENED_EVENT.clone(),
        TCP_DATA_RECEIVED_EVENT.clone(),
    ]
}
