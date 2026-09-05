//! MySQL protocol actions implementation

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

/// MySQL protocol action handler
pub struct MysqlProtocol {
    _connection_id: ConnectionId,
    _app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
}

impl MysqlProtocol {
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

// Implement Protocol trait (common functionality)
impl Protocol for MysqlProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        vec![
                crate::llm::actions::ParameterDefinition {
                    name: "send_first".to_string(),
                    type_hint: "boolean".to_string(),
                    description: "Whether the server should send the first message after connection (not typically needed for this protocol)".to_string(),
                    required: false,
                    example: serde_json::json!(false),
                },
            ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // No user-triggered actions. (A `list_mysql_connections` action used to be declared
        // here; its executor returned a hardcoded empty list, so it only ever misled the model.)
        vec![]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            mysql_query_response_action(),
            mysql_error_response_action(),
            mysql_ok_response_action(),
            close_this_connection_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "MySQL"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_mysql_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>MySQL"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["mysql"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            // Beta: exercised against a real, independent client — mysql_async —
            // covering handshake, queries and prepared statements. Not Stable: Stable additionally wants spec
            // compliance and scripting support reviewed, which has not been done here.
            .state(DevelopmentState::Beta)
            .implementation("opensrv-mysql v0.7 protocol library")
            .llm_control("Query responses (result sets, OK packets, ERR packets)")
            .e2e_testing("mysql_async client crate")
            .notes("No authentication, no TLS; all row values are sent as text")
            .build()
    }
    fn description(&self) -> &'static str {
        "MySQL database server"
    }
    fn example_prompt(&self) -> &'static str {
        "Start a MySQL server on port 3306"
    }
    fn group_name(&self) -> &'static str {
        "Database"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // Deterministic answers for a couple of well-known queries, falling back
        // to an empty OK for writes. Real column/row shapes, no LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "mysql_query":
    q = event.get("query", "").strip().rstrip(";").upper()
    if q.startswith("SELECT VERSION()"):
        actions = [{"type": "mysql_query_response",
                    "columns": [{"name": "version", "type": "VARCHAR"}],
                    "rows": [["8.0.36"]]}]
    elif q.startswith("SELECT"):
        actions = [{"type": "mysql_query_response",
                    "columns": [{"name": "id", "type": "INT"},
                                {"name": "name", "type": "VARCHAR"}],
                    "rows": [[1, "alice"], [2, "bob"]]}]
    else:
        actions = [{"type": "mysql_ok_response", "affected_rows": 0}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: reason about the SQL to synthesise coherent rows.
            json!({
                "type": "open_server",
                "port": 3306,
                "base_stack": "mysql",
                "instruction": "Act as a MySQL server for a 'products' database. Answer SELECT queries by generating plausible rows that satisfy the WHERE clause and requested columns; treat INSERT/UPDATE/DELETE as succeeding and report affected_rows. Keep results consistent across queries in the same session."
            }),
            // Script mode: fixed answers for known queries, no LLM call.
            json!({
                "type": "open_server",
                "port": 3306,
                "base_stack": "mysql",
                "event_handlers": [{
                    "event_pattern": "mysql_query",
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
                "port": 3306,
                "base_stack": "mysql",
                "event_handlers": [{
                    "event_pattern": "mysql_query",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "mysql_query_response",
                            "columns": [{"name": "result", "type": "VARCHAR"}],
                            "rows": [["OK"]]
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for MysqlProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::mysql::MysqlServer;
            let send_first = ctx
                .startup_params
                .as_ref()
                .map(|p| p.get_optional_bool("send_first"))
                .transpose()?
                .flatten()
                .unwrap_or(false);

            MysqlServer::spawn_with_llm_actions(
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
            "mysql_query_response" => self.execute_mysql_query_response(action),
            "mysql_error_response" => self.execute_mysql_error_response(action),
            "mysql_ok_response" => self.execute_mysql_ok_response(action),
            "close_this_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown MySQL action: {}", action_type)),
        }
    }
}

impl MysqlProtocol {
    fn execute_mysql_query_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        // Extract columns and rows from the action
        let columns = action
            .get("columns")
            .and_then(|v| v.as_array())
            .context("Missing 'columns' array")?;

        let rows = action
            .get("rows")
            .and_then(|v| v.as_array())
            .context("Missing 'rows' array")?;

        debug!(
            "MySQL query response: {} columns, {} rows",
            columns.len(),
            rows.len()
        );

        let _ = self.status_tx.send(format!(
            "[DEBUG] MySQL → Result set: {} columns, {} rows",
            columns.len(),
            rows.len()
        ));

        // Return a custom action result with the query response data
        Ok(ActionResult::Custom {
            name: "mysql_query_response".to_string(),
            data: json!({
                "columns": columns,
                "rows": rows
            }),
        })
    }

    fn execute_mysql_error_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        // Truncating with `as u16` turned an out-of-range code into an unrelated one; keep the
        // documented default instead.
        let error_code = action
            .get("error_code")
            .and_then(|v| v.as_u64())
            .and_then(|c| u16::try_from(c).ok())
            .unwrap_or(1064);

        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");

        debug!("MySQL error response: {} - {}", error_code, message);

        let _ = self
            .status_tx
            .send(format!("[DEBUG] MySQL ✗ Error {}: {}", error_code, message));

        Ok(ActionResult::Custom {
            name: "mysql_error".to_string(),
            data: json!({
                "error_code": error_code,
                "message": message
            }),
        })
    }

    fn execute_mysql_ok_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let affected_rows = action
            .get("affected_rows")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let last_insert_id = action
            .get("last_insert_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        debug!(
            "MySQL OK response: affected_rows={}, last_insert_id={}",
            affected_rows, last_insert_id
        );

        let _ = self.status_tx.send(format!(
            "[DEBUG] MySQL → OK: {} rows affected",
            affected_rows
        ));

        Ok(ActionResult::Custom {
            name: "mysql_ok".to_string(),
            data: json!({
                "affected_rows": affected_rows,
                "last_insert_id": last_insert_id
            }),
        })
    }
}

/// Action definition: Send MySQL query response
pub fn mysql_query_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "mysql_query_response".to_string(),
        description: "Send a result set in response to a SELECT query".to_string(),
        parameters: vec![
            Parameter {
                name: "columns".to_string(),
                type_hint: "array".to_string(),
                description: "Array of column definitions. Each column needs 'name' and 'type'. \
                              Recognised types: INT, INTEGER, BIGINT, SMALLINT, TINYINT, FLOAT, DOUBLE, \
                              DECIMAL, DATE, TIME, DATETIME, TIMESTAMP, BLOB, BINARY, TEXT, VARCHAR \
                              (anything else is treated as VARCHAR). The type sets the column metadata \
                              only - every value is transmitted in MySQL's text protocol".to_string(),
                required: true,
            },
            Parameter {
                name: "rows".to_string(),
                type_hint: "array".to_string(),
                description: "Array of rows. Each row is an array of values matching the column order".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "mysql_query_response",
            "columns": [{"name": "id", "type": "INT"}, {"name": "name", "type": "VARCHAR"}],
            "rows": [[1, "Alice"], [2, "Bob"]]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MySQL {columns_len} cols, {rows_len} rows")
                .with_debug("MySQL mysql_query_response: {columns_len} columns, {rows_len} rows"),
        ),
    }
}

/// Action definition: Send MySQL error response
pub fn mysql_error_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "mysql_error_response".to_string(),
        description: "Send an error response to the client".to_string(),
        parameters: vec![
            Parameter {
                name: "error_code".to_string(),
                type_hint: "number".to_string(),
                description:
                    "MySQL error number. Sent verbatim when it is one of the recognised codes: \
                     1044, 1045, 1046, 1049, 1050, 1051, 1052, 1054, 1062, 1064, 1065, 1136, \
                     1146, 1149, 1216, 1217, 1364, 1451, 1452, 1690. Any other value is reported \
                     to the client as 1105 (unknown error) with your message unchanged"
                        .to_string(),
                required: true,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Error message to display to the client".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "mysql_error_response",
            "error_code": 1146,
            "message": "Table 'database.table_name' doesn't exist"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MySQL ERR {error_code}: {message}")
                .with_debug("MySQL mysql_error_response: code={error_code}, message={message}"),
        ),
    }
}

/// Action definition: Send MySQL OK response
pub fn mysql_ok_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "mysql_ok_response".to_string(),
        description: "Send an OK response for INSERT, UPDATE, DELETE, or other non-SELECT queries"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "affected_rows".to_string(),
                type_hint: "number".to_string(),
                description: "Number of rows affected by the query".to_string(),
                required: false,
            },
            Parameter {
                name: "last_insert_id".to_string(),
                type_hint: "number".to_string(),
                description: "Last insert ID for INSERT queries with auto_increment".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "mysql_ok_response",
            "affected_rows": 1,
            "last_insert_id": 42
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MySQL OK, {affected_rows} rows affected")
                .with_debug("MySQL mysql_ok_response: affected_rows={affected_rows}, last_insert_id={last_insert_id}"),
        ),
    }
}

/// Action definition: Close current MySQL connection
pub fn close_this_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Close the current MySQL connection".to_string(),
        parameters: vec![],
        example: json!({"type": "close_this_connection"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("MySQL connection closed")
                .with_debug("MySQL close_this_connection"),
        ),
    }
}

// ============================================================================
// MySQL Action Constants
// ============================================================================

/// MySQL query response action constant
pub static MYSQL_QUERY_RESPONSE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| {
    ActionDefinition {
        name: "mysql_query_response".to_string(),
        description: "Send a result set in response to a SELECT query".to_string(),
        parameters: vec![
            Parameter {
                name: "columns".to_string(),
                type_hint: "array".to_string(),
                description: "Array of column definitions. Each column needs 'name' and 'type'. \
                              Recognised types: INT, INTEGER, BIGINT, SMALLINT, TINYINT, FLOAT, DOUBLE, \
                              DECIMAL, DATE, TIME, DATETIME, TIMESTAMP, BLOB, BINARY, TEXT, VARCHAR \
                              (anything else is treated as VARCHAR). The type sets the column metadata \
                              only - every value is transmitted in MySQL's text protocol".to_string(),
                required: true,
            },
            Parameter {
                name: "rows".to_string(),
                type_hint: "array".to_string(),
                description: "Array of rows. Each row is an array of values matching the column order".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "mysql_query_response",
            "columns": [{"name": "id", "type": "INT"}, {"name": "name", "type": "VARCHAR"}],
            "rows": [[1, "Alice"], [2, "Bob"]]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MySQL {columns_len} cols, {rows_len} rows")
                .with_debug("MySQL mysql_query_response: {columns_len} columns, {rows_len} rows"),
        ),
    }
});

/// MySQL error response action constant
pub static MYSQL_ERROR_RESPONSE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| ActionDefinition {
        name: "mysql_error_response".to_string(),
        description: "Send an error response to the client".to_string(),
        parameters: vec![
            Parameter {
                name: "error_code".to_string(),
                type_hint: "number".to_string(),
                description:
                    "MySQL error number. Sent verbatim when it is one of the recognised codes: \
                     1044, 1045, 1046, 1049, 1050, 1051, 1052, 1054, 1062, 1064, 1065, 1136, \
                     1146, 1149, 1216, 1217, 1364, 1451, 1452, 1690. Any other value is reported \
                     to the client as 1105 (unknown error) with your message unchanged"
                        .to_string(),
                required: true,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Error message to display to the client".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "mysql_error_response",
            "error_code": 1146,
            "message": "Table 'database.table_name' doesn't exist"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MySQL ERR {error_code}: {message}")
                .with_debug("MySQL mysql_error_response: code={error_code}, message={message}"),
        ),
    });

/// MySQL OK response action constant
pub static MYSQL_OK_RESPONSE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| {
    ActionDefinition {
        name: "mysql_ok_response".to_string(),
        description: "Send an OK response for INSERT, UPDATE, DELETE, or other non-SELECT queries"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "affected_rows".to_string(),
                type_hint: "number".to_string(),
                description: "Number of rows affected by the query".to_string(),
                required: false,
            },
            Parameter {
                name: "last_insert_id".to_string(),
                type_hint: "number".to_string(),
                description: "Last insert ID for INSERT queries with auto_increment".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "mysql_ok_response",
            "affected_rows": 1,
            "last_insert_id": 42
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MySQL OK, {affected_rows} rows affected")
                .with_debug("MySQL mysql_ok_response: affected_rows={affected_rows}, last_insert_id={last_insert_id}"),
        ),
    }
});

/// MySQL close connection action constant
pub static MYSQL_CLOSE_CONNECTION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Close the current MySQL connection".to_string(),
        parameters: vec![],
        example: json!({"type": "close_this_connection"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("MySQL connection closed")
                .with_debug("MySQL close_this_connection"),
        ),
    });

// ============================================================================
// MySQL Event Type Constants
// ============================================================================

/// MySQL query event - triggered when client sends a query
pub static MYSQL_QUERY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mysql_query",
        "MySQL query received from client",
        json!({"type": "placeholder", "event_id": "mysql_query"}),
    )
    .with_parameters(vec![Parameter {
        name: "query".to_string(),
        type_hint: "string".to_string(),
        description: "The SQL query string sent by the client".to_string(),
        required: true,
    }])
    .with_actions(vec![
        MYSQL_QUERY_RESPONSE_ACTION.clone(),
        MYSQL_ERROR_RESPONSE_ACTION.clone(),
        MYSQL_OK_RESPONSE_ACTION.clone(),
        MYSQL_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("MySQL: {preview(query,80)}")
            .with_debug("MySQL query: {query}")
            .with_trace("MySQL: {json_pretty(.)}"),
    )
});

/// Get MySQL event types
pub fn get_mysql_event_types() -> Vec<EventType> {
    vec![MYSQL_QUERY_EVENT.clone()]
}
