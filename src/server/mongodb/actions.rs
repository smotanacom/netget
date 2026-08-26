//! MongoDB protocol actions implementation

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
use tracing::debug;

/// MongoDB protocol action handler
pub struct MongodbProtocol {
    _connection_id: ConnectionId,
    _app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
}

impl MongodbProtocol {
    pub fn new(
        connection_id: ConnectionId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            _connection_id: connection_id,
            _app_state: app_state,
            status_tx,
        }
    }
}

// Event type definitions
pub static MONGODB_COMMAND_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mongodb_command",
        "MongoDB command received from client",
        json!({"type": "placeholder", "event_id": "mongodb_command"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "command".to_string(),
            type_hint: "string".to_string(),
            description: "Command name (find, insert, update, delete, etc.)".to_string(),
            required: true,
        },
        Parameter {
            name: "database".to_string(),
            type_hint: "string".to_string(),
            description: "Target database name".to_string(),
            required: true,
        },
        Parameter {
            name: "collection".to_string(),
            type_hint: "string".to_string(),
            description: "Target collection name (the value of the command field, e.g. \"users\" \
                          for {find: \"users\"}). Null for commands that do not name a collection"
                .to_string(),
            required: false,
        },
        Parameter {
            name: "filter".to_string(),
            type_hint: "object".to_string(),
            description: "Query filter (for find/update/delete)".to_string(),
            required: false,
        },
        Parameter {
            name: "document".to_string(),
            type_hint: "object".to_string(),
            description: "Document to insert or update".to_string(),
            required: false,
        },
    ])
    // Without this the model is offered no protocol actions at all for the only event this
    // protocol fires: `call_llm` builds the available-action list from
    // `event.event_type.actions` (src/llm/action_helper.rs:454), not from `get_sync_actions()`.
    // Every `find_response` the model produced was rejected as an unknown action.
    .with_actions(vec![
        find_response_action(),
        insert_response_action(),
        update_response_action(),
        delete_response_action(),
        error_response_action(),
        close_this_connection_action(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("{client_ip} MongoDB {command} {database}.{collection} ({duration_ms}ms)")
            .with_debug("MongoDB {command} on {database}.{collection} from {client_ip}")
            .with_trace("MongoDB command: {json_pretty(.)}"),
    )
});

pub static MONGODB_DISCONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mongodb_disconnected",
        "A MongoDB client disconnected. Purely informational - the socket is already closed, so \
         nothing can be sent in reply. Use it to record what happened in memory or the log.",
        json!({"type": "show_message", "message": "MongoDB client disconnected"}),
    )
    .with_parameters(vec![Parameter {
        name: "reason".to_string(),
        type_hint: "string".to_string(),
        description: "Why the connection ended: client_disconnect, close_this_connection, \
                      invalid_message_length, unsupported_opcode or malformed_op_msg"
            .to_string(),
        required: false,
    }])
    .with_no_actions()
    .with_log_template(
        LogTemplate::new()
            .with_info("{client_ip} MongoDB disconnected: {reason}")
            .with_debug("MongoDB disconnect from {client_ip}: {reason}"),
    )
});

// Implement Protocol trait (common functionality)
impl Protocol for MongodbProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        vec![crate::llm::actions::ParameterDefinition {
            name: "send_first".to_string(),
            type_hint: "boolean".to_string(),
            description:
                "Whether the server should send the first message after connection (not needed)"
                    .to_string(),
            required: false,
            example: serde_json::json!(false),
        }]
    }

    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // No user-triggered actions. (A `list_mongodb_connections` action used to be declared
        // here; its executor returned `NoAction`, so it did literally nothing.)
        vec![]
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            find_response_action(),
            insert_response_action(),
            update_response_action(),
            delete_response_action(),
            error_response_action(),
            close_this_connection_action(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "MongoDB"
    }

    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            MONGODB_COMMAND_EVENT.clone(),
            MONGODB_DISCONNECTED_EVENT.clone(),
        ]
    }

    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>MongoDB"
    }

    fn keywords(&self) -> Vec<&'static str> {
        vec!["mongodb", "mongo"]
    }

    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            // Beta: exercised against a real, independent client — the official mongodb Rust driver —
            // covering handshake plus CRUD commands. Not Stable: Stable additionally wants spec
            // compliance and scripting support reviewed, which has not been done here.
            .state(DevelopmentState::Beta)
            .implementation("bson v3.0 with manual OP_MSG parsing (section kind 0 only)")
            .llm_control("Query responses (documents, counts, errors)")
            .e2e_testing("mongodb official client crate")
            .notes(
                "No authentication, no compression, no storage. Only OP_MSG (2013) is \
                 implemented - a legacy OP_QUERY handshake is rejected. The driver's `hello` \
                 handshake is answered by the LLM like any other command, so an instruction \
                 that does not cover it will fail to connect",
            )
            .build()
    }

    fn description(&self) -> &'static str {
        "MongoDB database server"
    }

    fn example_prompt(&self) -> &'static str {
        "Start a MongoDB server on port 27017"
    }

    fn group_name(&self) -> &'static str {
        "Database"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // Deterministic: answer every command with an empty find result, no
        // LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
if data["event_type_id"] == "mongodb_command":
    actions = [{"type": "find_response", "documents": []}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: LLM handles all MongoDB responses intelligently
            json!({
                "type": "open_server",
                "port": 27017,
                "base_stack": "mongodb",
                "instruction": "MongoDB database server handling document queries"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_server",
                "port": 27017,
                "base_stack": "mongodb",
                "event_handlers": [{
                    "event_pattern": "mongodb_command",
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
                "port": 27017,
                "base_stack": "mongodb",
                "event_handlers": [{
                    "event_pattern": "mongodb_command",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "find_response",
                            "documents": []
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for MongodbProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::mongodb::MongodbServer;
            let send_first = ctx
                .startup_params
                .as_ref()
                .map(|p| p.get_optional_bool("send_first"))
                .transpose()?
                .flatten()
                .unwrap_or(false);

            MongodbServer::spawn_with_llm_actions(
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
            "find_response" => self.execute_find_response(action),
            "insert_response" => self.execute_insert_response(action),
            "update_response" => self.execute_update_response(action),
            "delete_response" => self.execute_delete_response(action),
            "error_response" => self.execute_error_response(action),
            "close_this_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown MongoDB action: {}", action_type)),
        }
    }
}

impl MongodbProtocol {
    fn execute_find_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let documents = action
            .get("documents")
            .and_then(|v| v.as_array())
            .context("Missing 'documents' array")?
            .clone();

        debug!("MongoDB find_response with {} documents", documents.len());
        let _ = self.status_tx.send(format!(
            "[MongoDB] Sending find response with {} documents",
            documents.len()
        ));

        Ok(ActionResult::Custom {
            name: "mongodb_response".to_string(),
            data: json!({
                "type": "find_response",
                "documents": documents
            }),
        })
    }

    fn execute_insert_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        // Declared `required: true`, so it is an error to omit it — not a licence to invent
        // one. Defaulting to 1 reported a successful insert of a document that nothing said
        // was inserted, so a malformed answer reached the driver as an acknowledged write.
        let inserted_count = action
            .get("inserted_count")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "mongodb_insert_response requires `inserted_count` (a non-negative integer);                      it is how many documents were actually stored and cannot be assumed"
                )
            })?;

        debug!("MongoDB insert_response: {} documents", inserted_count);
        let _ = self.status_tx.send(format!(
            "[MongoDB] Insert acknowledged: {} documents",
            inserted_count
        ));

        Ok(ActionResult::Custom {
            name: "mongodb_response".to_string(),
            data: json!({
                "type": "insert_response",
                "inserted_count": inserted_count
            }),
        })
    }

    fn execute_update_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let matched_count = action
            .get("matched_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let modified_count = action
            .get("modified_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        debug!(
            "MongoDB update_response: matched={}, modified={}",
            matched_count, modified_count
        );
        let _ = self.status_tx.send(format!(
            "[MongoDB] Update acknowledged: {} matched, {} modified",
            matched_count, modified_count
        ));

        Ok(ActionResult::Custom {
            name: "mongodb_response".to_string(),
            data: json!({
                "type": "update_response",
                "matched_count": matched_count,
                "modified_count": modified_count
            }),
        })
    }

    fn execute_delete_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let deleted_count = action
            .get("deleted_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        debug!("MongoDB delete_response: {} documents", deleted_count);
        let _ = self.status_tx.send(format!(
            "[MongoDB] Delete acknowledged: {} documents",
            deleted_count
        ));

        Ok(ActionResult::Custom {
            name: "mongodb_response".to_string(),
            data: json!({
                "type": "delete_response",
                "deleted_count": deleted_count
            }),
        })
    }

    fn execute_error_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let code = action.get("code").and_then(|v| v.as_u64()).unwrap_or(0) as i32;
        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();

        debug!("MongoDB error_response: code={}, message={}", code, message);
        let _ = self
            .status_tx
            .send(format!("[MongoDB] Error: {} - {}", code, message));

        Ok(ActionResult::Custom {
            name: "mongodb_response".to_string(),
            data: json!({
                "type": "error_response",
                "code": code,
                "message": message
            }),
        })
    }
}

// Action definitions
fn find_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "find_response".to_string(),
        description: "Send query results to the client".to_string(),
        parameters: vec![Parameter {
            name: "documents".to_string(),
            type_hint: "array".to_string(),
            description: "Array of BSON documents to return".to_string(),
            required: true,
        }],
        example: json!({
            "type": "find_response",
            "documents": [
                {"_id": {"$oid": "507f1f77bcf86cd799439011"}, "name": "Alice", "age": 30},
                {"_id": {"$oid": "507f191e810c19729de860ea"}, "name": "Bob", "age": 25}
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> {documents_len} documents")
                .with_debug("MongoDB find: {documents_len} documents returned"),
        ),
    }
}

fn insert_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "insert_response".to_string(),
        description: "Acknowledge insert operation".to_string(),
        parameters: vec![Parameter {
            name: "inserted_count".to_string(),
            type_hint: "integer".to_string(),
            description: "Number of documents inserted".to_string(),
            required: true,
        }],
        example: json!({
            "type": "insert_response",
            "inserted_count": 1
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> inserted {inserted_count}")
                .with_debug("MongoDB insert: {inserted_count} documents"),
        ),
    }
}

fn update_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "update_response".to_string(),
        description: "Acknowledge update operation".to_string(),
        parameters: vec![
            Parameter {
                name: "matched_count".to_string(),
                type_hint: "integer".to_string(),
                description: "Number of documents matched by filter".to_string(),
                required: true,
            },
            Parameter {
                name: "modified_count".to_string(),
                type_hint: "integer".to_string(),
                description: "Number of documents modified".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "update_response",
            "matched_count": 1,
            "modified_count": 1
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> matched {matched_count}, modified {modified_count}")
                .with_debug("MongoDB update: matched={matched_count}, modified={modified_count}"),
        ),
    }
}

fn delete_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "delete_response".to_string(),
        description: "Acknowledge delete operation".to_string(),
        parameters: vec![Parameter {
            name: "deleted_count".to_string(),
            type_hint: "integer".to_string(),
            description: "Number of documents deleted".to_string(),
            required: true,
        }],
        example: json!({
            "type": "delete_response",
            "deleted_count": 2
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> deleted {deleted_count}")
                .with_debug("MongoDB delete: {deleted_count} documents"),
        ),
    }
}

fn error_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "error_response".to_string(),
        description: "Send error response to client".to_string(),
        parameters: vec![
            Parameter {
                name: "code".to_string(),
                type_hint: "integer".to_string(),
                description: "MongoDB error code".to_string(),
                required: true,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Error message".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "error_response",
            "code": 26,
            "message": "Namespace not found"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> error {code}: {message}")
                .with_debug("MongoDB error: code={code}, message={message}"),
        ),
    }
}

fn close_this_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Close the current MongoDB connection".to_string(),
        parameters: vec![],
        example: json!({
            "type": "close_this_connection"
        }),
        log_template: Some(LogTemplate::new().with_info("-> connection closed")),
    }
}
