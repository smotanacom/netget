//! Redis protocol actions implementation

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
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Redis protocol action handler
pub struct RedisProtocol {
    #[allow(dead_code)]
    connection_id: ConnectionId,
    #[allow(dead_code)]
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
}

impl RedisProtocol {
    pub fn new(
        connection_id: ConnectionId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            connection_id,
            app_state,
            status_tx,
        }
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for RedisProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        vec![
                crate::llm::actions::ParameterDefinition {
                    name: "send_first".to_string(),
                    type_hint: "boolean".to_string(),
                    description: "Whether the server should send the first message after connection (not typically needed for Redis)".to_string(),
                    required: false,
                    example: json!(false),
                },
            ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // No user-triggered actions. (A `list_redis_connections` action used to be declared
        // here; its executor returned a hardcoded empty list, so it only ever misled the model.)
        vec![]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            redis_simple_string_action(),
            redis_bulk_string_action(),
            redis_array_action(),
            redis_integer_action(),
            redis_error_action(),
            redis_null_action(),
            close_this_connection_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "Redis"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_redis_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>Redis"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["redis"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            // Beta: exercised against a real, independent client — redis-rs —
            // covering RESP commands driven by the standard Rust client. Not Stable: Stable additionally wants spec
            // compliance and scripting support reviewed, which has not been done here.
            .state(DevelopmentState::Beta)
            .implementation("redis-protocol v6.0 (RESP2 parsing), manual RESP2 encoding")
            .llm_control("All Redis commands (GET, SET, INCR, etc.)")
            .e2e_testing("redis-rs client")
            .notes("RESP2 only (no RESP3), no AUTH/SELECT/MULTI/pub-sub, no inline commands")
            .build()
    }
    fn description(&self) -> &'static str {
        "Redis in-memory data store"
    }
    fn example_prompt(&self) -> &'static str {
        "Start a Redis server on port 6379"
    }
    fn group_name(&self) -> &'static str {
        "Database"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // Deterministic RESP replies keyed off the command verb. The 'command'
        // event field is the full command line, so split off the first word.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "redis_command":
    parts = event.get("command", "").split()
    verb = parts[0].upper() if parts else ""
    if verb == "PING":
        actions = [{"type": "redis_simple_string", "value": "PONG"}]
    elif verb == "SET":
        actions = [{"type": "redis_simple_string", "value": "OK"}]
    elif verb == "GET":
        actions = [{"type": "redis_bulk_string", "value": "cached-value"}]
    elif verb == "INCR":
        actions = [{"type": "redis_integer", "value": 1}]
    else:
        actions = [{"type": "redis_error", "message": "ERR unknown command"}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: model acts as a coherent key-value store across commands.
            json!({
                "type": "open_server",
                "port": 6379,
                "base_stack": "redis",
                "instruction": "Act as a Redis server backing a session cache. Remember values set with SET and return them on GET, treat INCR/DECR as maintaining running counters, and answer TTL/EXISTS consistently with what has been stored so far in this connection."
            }),
            // Script mode: fixed replies per command verb, no LLM call.
            json!({
                "type": "open_server",
                "port": 6379,
                "base_stack": "redis",
                "event_handlers": [{
                    "event_pattern": "redis_command",
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
                "port": 6379,
                "base_stack": "redis",
                "event_handlers": [{
                    "event_pattern": "redis_command",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "redis_simple_string",
                            "value": "OK"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for RedisProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::redis::RedisServer;
            let send_first = ctx
                .startup_params
                .as_ref()
                .map(|p| p.get_optional_bool("send_first"))
                .transpose()?
                .flatten()
                .unwrap_or(false);

            RedisServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
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
            "redis_simple_string" => self.execute_redis_simple_string(action),
            "redis_bulk_string" => self.execute_redis_bulk_string(action),
            "redis_array" => self.execute_redis_array(action),
            "redis_integer" => self.execute_redis_integer(action),
            "redis_error" => self.execute_redis_error(action),
            "redis_null" => self.execute_redis_null(action),
            "close_this_connection" => Ok(ActionResult::CloseConnection),
            // Not offered to the model (`close_this_connection` is its verb), but the
            // dashboard's "disconnect this peer" injects `close_connection` through the peer
            // command task, which half-closes the write side on this result.
            "close_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown Redis action: {}", action_type)),
        }
    }
}

impl RedisProtocol {
    /// Every reply verb returns `ActionResult::Output` with the RESP2 bytes already
    /// encoded, so the same executor serves both the connection's read loop and the
    /// dashboard's "message this peer" injection (`server::peer_support` writes
    /// `Output` bytes; a `Custom` result would be reported as executed without
    /// touching the wire — the gap this refactor closed).
    fn execute_redis_simple_string(&self, action: serde_json::Value) -> Result<ActionResult> {
        let value = action
            .get("value")
            .and_then(|v| v.as_str())
            .context("Missing 'value' field")?;

        debug!("Redis simple string response: {}", value);
        let _ = self
            .status_tx
            .send(format!("[DEBUG] Redis → Simple string: {}", value));

        Ok(ActionResult::Output(encode_simple_string(value)))
    }

    fn execute_redis_bulk_string(&self, action: serde_json::Value) -> Result<ActionResult> {
        let value = action.get("value");

        let result = if let Some(v) = value {
            if v.is_null() {
                None
            } else if let Some(s) = v.as_str() {
                Some(s.as_bytes().to_vec())
            } else {
                Some(v.to_string().as_bytes().to_vec())
            }
        } else {
            None
        };

        debug!("Redis bulk string response: {:?}", result);
        let _ = self.status_tx.send(format!(
            "[DEBUG] Redis → Bulk string: {} bytes",
            result.as_ref().map(|v| v.len()).unwrap_or(0)
        ));

        Ok(ActionResult::Output(match result {
            Some(bytes) => encode_bulk_string(&bytes),
            None => encode_null(),
        }))
    }

    fn execute_redis_array(&self, action: serde_json::Value) -> Result<ActionResult> {
        let values = action
            .get("values")
            .and_then(|v| v.as_array())
            .context("Missing 'values' array")?;

        debug!("Redis array response: {} elements", values.len());
        let _ = self
            .status_tx
            .send(format!("[DEBUG] Redis → Array: {} elements", values.len()));

        Ok(ActionResult::Output(encode_array(values)))
    }

    fn execute_redis_integer(&self, action: serde_json::Value) -> Result<ActionResult> {
        let value = action
            .get("value")
            .and_then(|v| v.as_i64())
            .context("Missing 'value' field")?;

        debug!("Redis integer response: {}", value);
        let _ = self
            .status_tx
            .send(format!("[DEBUG] Redis → Integer: {}", value));

        Ok(ActionResult::Output(encode_integer(value)))
    }

    fn execute_redis_error(&self, action: serde_json::Value) -> Result<ActionResult> {
        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .context("Missing 'message' field")?;

        // An error the handler *chose* to send, not one netget synthesised because it could
        // not answer. On the wire both are just `-…`, so the log carries the distinction, as
        // `src/server/radius/` does: `decision=model_reject` here,
        // `decision=fail_closed_*` in `mod.rs`.
        debug!(
            "Redis connection {} decision=model_reject: {}",
            self.connection_id,
            crate::utils::truncate_for_log(message, 512)
        );
        let _ = self
            .status_tx
            .send(format!("[DEBUG] Redis ✗ Error: {}", message));

        Ok(ActionResult::Output(encode_error(message)))
    }

    fn execute_redis_null(&self, _action: serde_json::Value) -> Result<ActionResult> {
        debug!("Redis null response");
        let _ = self.status_tx.send("[DEBUG] Redis → Null".to_string());

        Ok(ActionResult::Output(encode_null()))
    }
}

// ============================================================================
// RESP2 encoding — single source of truth
// ============================================================================
//
// Called by the executors above (so the LLM path, script/static handlers and
// dashboard peer injection all produce identical bytes) and by `mod.rs` for the
// replies the loop synthesises itself (frame-cap error, LLM-failure error,
// no-response error).

/// Strip CR and LF from a RESP *simple* payload, mapping each to a space.
///
/// A RESP simple string (`+…`) and simple error (`-…`) are CRLF-terminated with **no length
/// prefix**, so a newline anywhere in the payload ends the frame early and everything after it
/// is parsed as the *next* reply. The connection is then desynchronised for good: every
/// subsequent command reads the wrong answer, and the client has no way to notice.
///
/// The value comes from model output (`redis_simple_string`'s `value`, `redis_error`'s
/// `message`), so it is exactly as trustworthy as any other generated text. `mod.rs` documents
/// this hazard on the LLM-failure path and avoids it by sending a fixed category; the
/// model-facing verbs had no such guard.
///
/// Mapping to a space rather than rejecting the reply is what Redis itself does —
/// `addReplyErrorFormat` runs `sdsmapchars(s, "\r\n", "  ", 2)` before framing.
fn sanitize_simple_payload(s: &str, action: &str) -> String {
    if !s.contains(['\r', '\n']) {
        return s.to_string();
    }
    warn!(
        "Redis: {} payload contained CR/LF, which would have ended the RESP frame early and \
         desynchronised the connection; mapping each to a space (as Redis does)",
        action
    );
    s.replace(['\r', '\n'], " ")
}

/// Encode a simple string response ("+OK\r\n")
pub fn encode_simple_string(s: &str) -> Vec<u8> {
    format!("+{}\r\n", sanitize_simple_payload(s, "simple string")).into_bytes()
}

/// Encode a bulk string response ("$5\r\nhello\r\n")
pub fn encode_bulk_string(bytes: &[u8]) -> Vec<u8> {
    let mut result = format!("${}\r\n", bytes.len()).into_bytes();
    result.extend_from_slice(bytes);
    result.extend_from_slice(b"\r\n");
    result
}

/// Encode a null bulk string ("$-1\r\n")
pub fn encode_null() -> Vec<u8> {
    b"$-1\r\n".to_vec()
}

/// Encode an integer response (":42\r\n")
pub fn encode_integer(i: i64) -> Vec<u8> {
    format!(":{}\r\n", i).into_bytes()
}

/// Encode an error response ("-ERR message\r\n")
pub fn encode_error(msg: &str) -> Vec<u8> {
    format!("-{}\r\n", sanitize_simple_payload(msg, "error")).into_bytes()
}

/// Encode an array response.
///
/// The element mapping is part of the `redis_array` action contract documented to the LLM:
/// strings become bulk strings, integers become RESP integers, booleans become the bulk
/// strings `"1"`/`"0"`, null becomes a nil bulk string, and nested arrays/objects are
/// serialized to JSON and sent as a bulk string (RESP2 has no JSON type).
pub fn encode_array(values: &[serde_json::Value]) -> Vec<u8> {
    let mut result = format!("*{}\r\n", values.len()).into_bytes();

    for value in values {
        match value {
            serde_json::Value::String(s) => {
                result.extend_from_slice(&encode_bulk_string(s.as_bytes()));
            }
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    result.extend_from_slice(&encode_integer(i));
                } else {
                    // Encode as bulk string
                    let s = n.to_string();
                    result.extend_from_slice(&encode_bulk_string(s.as_bytes()));
                }
            }
            serde_json::Value::Bool(b) => {
                let s = if *b { "1" } else { "0" };
                result.extend_from_slice(&encode_bulk_string(s.as_bytes()));
            }
            serde_json::Value::Null => {
                result.extend_from_slice(&encode_null());
            }
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                // Nested arrays/objects - encode as bulk string JSON
                let s = value.to_string();
                result.extend_from_slice(&encode_bulk_string(s.as_bytes()));
            }
        }
    }

    result
}

/// Action definition: Send Redis simple string response
pub fn redis_simple_string_action() -> ActionDefinition {
    ActionDefinition {
        name: "redis_simple_string".to_string(),
        description: "Send a simple string response (e.g., '+OK\\r\\n')".to_string(),
        parameters: vec![Parameter {
            name: "value".to_string(),
            type_hint: "string".to_string(),
            description: "The string value to send (without RESP encoding)".to_string(),
            required: true,
        }],
        example: json!({
            "type": "redis_simple_string",
            "value": "OK"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> Redis +{value}")
                .with_debug("Redis redis_simple_string: value={value}"),
        ),
    }
}

/// Action definition: Send Redis bulk string response
pub fn redis_bulk_string_action() -> ActionDefinition {
    ActionDefinition {
        name: "redis_bulk_string".to_string(),
        description: "Send a bulk string response (e.g., '$5\\r\\nhello\\r\\n'). Use null for nil bulk string".to_string(),
        parameters: vec![Parameter {
            name: "value".to_string(),
            type_hint: "string".to_string(),
            description: "The string value to send, or null for nil bulk string".to_string(),
            required: false,
        }],
        example: json!({
            "type": "redis_bulk_string",
            "value": "hello world"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> Redis ${value_len}B")
                .with_debug("Redis redis_bulk_string: {value_len} bytes"),
        ),
    }
}

/// Action definition: Send Redis array response
pub fn redis_array_action() -> ActionDefinition {
    ActionDefinition {
        name: "redis_array".to_string(),
        description: "Send an array response, for commands that return a list (KEYS, MGET, LRANGE, SCAN...)".to_string(),
        parameters: vec![Parameter {
            name: "values".to_string(),
            type_hint: "array".to_string(),
            description: "Array of values. A string becomes a bulk string, an integer becomes a RESP integer, true/false become the bulk strings \"1\"/\"0\", null becomes a nil element, and a nested array or object is sent as its JSON text in a bulk string".to_string(),
            required: true,
        }],
        example: json!({
            "type": "redis_array",
            "values": ["value1", "value2", "value3"]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> Redis *{values_len} elements")
                .with_debug("Redis redis_array: {values_len} elements"),
        ),
    }
}

/// Action definition: Send Redis integer response
pub fn redis_integer_action() -> ActionDefinition {
    ActionDefinition {
        name: "redis_integer".to_string(),
        description: "Send an integer response (e.g., ':42\\r\\n')".to_string(),
        parameters: vec![Parameter {
            name: "value".to_string(),
            type_hint: "integer".to_string(),
            description: "The integer value to send".to_string(),
            required: true,
        }],
        example: json!({
            "type": "redis_integer",
            "value": 42
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> Redis :{value}")
                .with_debug("Redis redis_integer: value={value}"),
        ),
    }
}

/// Action definition: Send Redis error response
pub fn redis_error_action() -> ActionDefinition {
    ActionDefinition {
        name: "redis_error".to_string(),
        description: "Send an error response (e.g., '-ERR message\\r\\n')".to_string(),
        parameters: vec![Parameter {
            name: "message".to_string(),
            type_hint: "string".to_string(),
            description: "The error message to send".to_string(),
            required: true,
        }],
        example: json!({
            "type": "redis_error",
            "message": "ERR unknown command 'foobar'"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> Redis -{message}")
                .with_debug("Redis redis_error: {message}"),
        ),
    }
}

/// Action definition: Send Redis null response
pub fn redis_null_action() -> ActionDefinition {
    ActionDefinition {
        name: "redis_null".to_string(),
        description: "Send a null response ('$-1\\r\\n')".to_string(),
        parameters: vec![],
        example: json!({
            "type": "redis_null"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> Redis (nil)")
                .with_debug("Redis redis_null"),
        ),
    }
}

/// Action definition: Close current Redis connection
pub fn close_this_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Close the current Redis connection".to_string(),
        parameters: vec![],
        example: json!({"type": "close_this_connection"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("Redis connection closed")
                .with_debug("Redis close_this_connection"),
        ),
    }
}

// ============================================================================
// Redis Action Constants
// ============================================================================

pub static REDIS_SIMPLE_STRING_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| redis_simple_string_action());
pub static REDIS_BULK_STRING_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| redis_bulk_string_action());
pub static REDIS_ARRAY_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| redis_array_action());
pub static REDIS_INTEGER_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| redis_integer_action());
pub static REDIS_ERROR_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| redis_error_action());
pub static REDIS_NULL_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| redis_null_action());
pub static REDIS_CLOSE_CONNECTION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| close_this_connection_action());

// ============================================================================
// Redis Event Type Constants
// ============================================================================

/// Redis command event - triggered when client sends a command
pub static REDIS_COMMAND_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "redis_command",
        "Redis command received from client",
        json!({"type": "placeholder", "event_id": "redis_command"}),
    )
    .with_parameters(vec![Parameter {
        name: "command".to_string(),
        type_hint: "string".to_string(),
        description: "The Redis command string sent by the client".to_string(),
        required: true,
    }])
    .with_actions(vec![
        REDIS_SIMPLE_STRING_ACTION.clone(),
        REDIS_BULK_STRING_ACTION.clone(),
        REDIS_ARRAY_ACTION.clone(),
        REDIS_INTEGER_ACTION.clone(),
        REDIS_ERROR_ACTION.clone(),
        REDIS_NULL_ACTION.clone(),
        REDIS_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("Redis: {command}")
            .with_debug("Redis command: {command}")
            .with_trace("Redis: {json_pretty(.)}"),
    )
});

/// Get Redis event types
pub fn get_redis_event_types() -> Vec<EventType> {
    vec![REDIS_COMMAND_EVENT.clone()]
}
