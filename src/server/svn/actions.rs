//! SVN protocol actions implementation

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

pub struct SvnProtocol;

impl SvnProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SvnProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        Vec::new() // SVN has no async actions
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_greeting_action(),
            send_success_action(),
            send_failure_action(),
            send_list_action(),
            send_response_action(),
            close_connection_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "SVN"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_svn_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>SVN"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["svn", "subversion"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            // svn:// is port 3690, which is above 1024 and needs no privilege. This used to
            // declare PrivilegedPort(3690); server_startup.rs only refuses to spawn when the
            // port is < 1024, so that check could never fire and merely read as protection
            // that did not exist.
            .privilege_requirement(PrivilegeRequirement::None)
            .implementation("Hand-rolled subset of the svn:// wire protocol, line-framed")
            .llm_control("Greeting, command responses (get-latest-rev, get-dir, stat, log, ...)")
            .e2e_testing("svn command-line client")
            .notes(
                "Line-framed subset only: tuples are read one line at a time, so a \
                 length-prefixed string containing a newline (any file content, any multi-line \
                 log message) desynchronises the parser. No svndiff, no editor/commit commands, \
                 no authentication beyond announcing ANONYMOUS. Usable as a honeypot or for \
                 protocol experiments, not by a real `svn checkout`.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "SVN (Subversion) version control server"
    }
    fn example_prompt(&self) -> &'static str {
        "SVN server on port 3690 - respond to repository commands with fake data"
    }
    fn group_name(&self) -> &'static str {
        "Infrastructure"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        // Deterministic: send the protocol greeting on connect and acknowledge
        // every command with success, no LLM call. One script handles both.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
et = data["event_type_id"]
if et == "svn_greeting":
    actions = [{"type": "send_svn_greeting", "min_version": 2,
                "max_version": 2, "mechanisms": ["ANONYMOUS"]}]
elif et == "svn_command":
    actions = [{"type": "send_svn_success"}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode
            json!({
                "type": "open_server",
                "port": 3690,
                "base_stack": "svn",
                "instruction": "SVN server. Respond to commands with standard repository layout (trunk, branches, tags). Latest revision is 42."
            }),
            // Script mode
            json!({
                "type": "open_server",
                "port": 3690,
                "base_stack": "svn",
                "event_handlers": [
                    {
                        "event_pattern": "svn_greeting",
                        "handler": {
                            "type": "script",
                            "language": "python",
                            "code": script
                        }
                    },
                    {
                        "event_pattern": "svn_command",
                        "handler": {
                            "type": "script",
                            "language": "python",
                            "code": script
                        }
                    }
                ]
            }),
            // Static mode
            json!({
                "type": "open_server",
                "port": 3690,
                "base_stack": "svn",
                "event_handlers": [{
                    "event_pattern": "svn_greeting",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_svn_greeting",
                            "min_version": 2,
                            "max_version": 2,
                            "mechanisms": ["ANONYMOUS"]
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for SvnProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::svn::SvnServer;
            SvnServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
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
            "send_svn_greeting" => self.execute_send_greeting(action),
            "send_svn_success" => self.execute_send_success(action),
            "send_svn_failure" => self.execute_send_failure(action),
            "send_svn_list" => self.execute_send_list(action),
            "send_svn_response" => self.execute_send_response(action),
            "close_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown SVN action: {}", action_type)),
        }
    }
}

impl SvnProtocol {
    fn execute_send_greeting(&self, action: serde_json::Value) -> Result<ActionResult> {
        let min_version = action
            .get("min_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(2) as u32;

        let max_version = action
            .get("max_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(2) as u32;

        let mechanisms = action
            .get("mechanisms")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["ANONYMOUS"]);

        // SVN protocol greeting format (simplified)
        // ( success ( 2 2 ( ) ( edit-pipeline svndiff1 absent-entries ) ) )
        let mut response = format!("( success ( {} {} ( ", min_version, max_version);

        // Mechanisms list
        for (i, mech) in mechanisms.iter().enumerate() {
            if i > 0 {
                response.push(' ');
            }
            response.push_str(mech);
        }
        response.push_str(" ) ( edit-pipeline svndiff1 ) ) )\n");

        Ok(ActionResult::Output(response.into_bytes()))
    }

    fn execute_send_success(&self, action: serde_json::Value) -> Result<ActionResult> {
        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("success");

        let data = action.get("data");

        let mut response = String::from("( success ( ");

        if let Some(data_val) = data {
            // If data is provided, include it in the response
            if let Some(data_str) = data_val.as_str() {
                response.push_str(&svn_item(data_str));
            } else if let Some(data_array) = data_val.as_array() {
                for (i, item) in data_array.iter().enumerate() {
                    if i > 0 {
                        response.push(' ');
                    }
                    if let Some(s) = item.as_str() {
                        response.push_str(&svn_item(s));
                    } else {
                        response.push_str(&item.to_string());
                    }
                }
            }
        } else {
            // Default success response
            response.push_str(&svn_item(message));
        }

        response.push_str(" ) )\n");

        Ok(ActionResult::Output(response.into_bytes()))
    }

    fn execute_send_failure(&self, action: serde_json::Value) -> Result<ActionResult> {
        // Range-check before narrowing. `as u32` wraps, and the wrap lands on the one value
        // this frame must never carry: apr-err 0 is `APR_SUCCESS`, so a failure response
        // built from `error_code: 4294967296` tells the client the operation did not fail.
        let error_code = action
            .get("error_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(210000);
        if error_code == 0 || error_code > u32::MAX as u64 {
            return Err(anyhow::anyhow!(
                "error_code {error_code} is not an apr-err this failure response can carry \
                 (1-4294967295). 0 is APR_SUCCESS and would report the operation as having \
                 succeeded; svn's own codes start at 20000 (SVN_ERR_BAD_CONTAINING_POOL), \
                 with 210000 (SVN_ERR_RA_SVN_CMD_ERR) the generic ra_svn failure."
            ));
        }
        let error_code = error_code as u32;

        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Operation failed");

        // ( failure ( ( apr-err:number message:string file:string line:number ) ... ) )
        // The message is a counted string, not a quoted one - svn has no quoting.
        let response = format!(
            "( failure ( ( {} {} {} 0 ) ) )\n",
            error_code,
            svn_string(message),
            svn_string("")
        );

        Ok(ActionResult::Output(response.into_bytes()))
    }

    fn execute_send_list(&self, action: serde_json::Value) -> Result<ActionResult> {
        let items = action
            .get("items")
            .and_then(|v| v.as_array())
            .context("Missing 'items' array")?;

        // get-dir returns ( success ( rev:number props:list ( entry... ) ) ) where each
        // entry is ( name:string kind:word size:number has-props:bool created-rev:number
        // created-date:list last-author:list ).
        //
        // The previous builder emitted an opening "( " for the first entry and a closing
        // " ) " only for entries after the first, so every response was unbalanced, names
        // were "double quoted" (svn has no quoting) and the revision was written as the
        // non-token `rev:N`. Nothing that reads svn could parse any of it.
        let mut response = String::from("( success ( 0 ( ) ( ");

        for item in items {
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let kind = match item.get("kind").and_then(|v| v.as_str()) {
                Some("dir") => "dir",
                Some("none") => "none",
                Some("unknown") => "unknown",
                _ => "file",
            };
            let size = item.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            let revision = item.get("revision").and_then(|v| v.as_u64()).unwrap_or(0);

            response.push_str(&format!(
                "( {} {} {} false {} ( ) ( ) ) ",
                svn_string(name),
                kind,
                size,
                revision
            ));
        }

        response.push_str(") ) )\n");

        Ok(ActionResult::Output(response.into_bytes()))
    }

    fn execute_send_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let response = action
            .get("response")
            .and_then(|v| v.as_str())
            .context("Missing 'response' parameter")?;

        // Ensure response ends with newline for SVN protocol
        let mut data = response.to_string();
        if !data.ends_with('\n') {
            data.push('\n');
        }

        Ok(ActionResult::Output(data.into_bytes()))
    }
}

/// Encode a value as an svn counted string: `<byte-length>:<bytes>`.
///
/// svn has no quoting mechanism at all - a string is always written as its byte length, a
/// colon, then the raw bytes. Emitting `"foo"` (as this protocol used to) puts three literal
/// characters plus two quote characters on the wire and no svn parser accepts it.
fn svn_string(value: &str) -> String {
    format!("{}:{}", value.len(), value)
}

/// Encode one datum for a `success` tuple.
///
/// A value that is entirely ASCII digits is emitted as a bare number (revisions, sizes and
/// counts are numbers in the grammar); anything else is emitted as a counted string. A caller
/// that needs exact control over the tuple should use `send_svn_response` instead.
fn svn_item(value: &str) -> String {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        value.to_string()
    } else {
        svn_string(value)
    }
}

fn send_greeting_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_svn_greeting".to_string(),
        description: "Send SVN protocol greeting with version and capabilities".to_string(),
        parameters: vec![
            Parameter {
                name: "min_version".to_string(),
                type_hint: "number".to_string(),
                description: "Minimum protocol version (default: 2)".to_string(),
                required: false,
            },
            Parameter {
                name: "max_version".to_string(),
                type_hint: "number".to_string(),
                description: "Maximum protocol version (default: 2)".to_string(),
                required: false,
            },
            Parameter {
                name: "mechanisms".to_string(),
                type_hint: "array".to_string(),
                description: "Authentication mechanisms (default: [\"ANONYMOUS\"])".to_string(),
                required: false,
            },
        ],
        // No `realm` parameter: the greeting tuple has no slot for one. In ra_svn the realm
        // travels in the auth-request that follows the client's mechanism choice, and this
        // server does not implement that exchange. It used to be declared here and read into
        // a `_realm` local, so a model that supplied it changed nothing on the wire.
        example: json!({
            "type": "send_svn_greeting",
            "min_version": 2,
            "max_version": 2,
            "mechanisms": ["ANONYMOUS"]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SVN greeting v{min_version}-{max_version}")
                .with_debug(
                    "SVN greeting: version={min_version}-{max_version}, mechanisms={mechanisms}",
                ),
        ),
    }
}

fn send_success_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_svn_success".to_string(),
        description: "Send SVN success response".to_string(),
        parameters: vec![
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Success message (default: \"success\")".to_string(),
                required: false,
            },
            Parameter {
                name: "data".to_string(),
                type_hint: "string or array".to_string(),
                description: "Value(s) to return inside the success tuple. A value made only of digits is sent as an svn number (use this for revisions); any other value is sent as an svn counted string, so \"hello\" goes on the wire as 5:hello. Use send_svn_response if you need to write the tuple yourself".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_svn_success",
            "message": "success",
            "data": "123"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SVN success")
                .with_debug("SVN success: message={message}"),
        ),
    }
}

fn send_failure_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_svn_failure".to_string(),
        description: "Send SVN error/failure response".to_string(),
        parameters: vec![
            Parameter {
                name: "error_code".to_string(),
                type_hint: "number".to_string(),
                description: "SVN error code (default: 210000)".to_string(),
                required: false,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Error message".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_svn_failure",
            "error_code": 210000,
            "message": "Repository not found"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SVN error {error_code}: {message}")
                .with_debug("SVN failure: code={error_code}, message={message}"),
        ),
    }
}

fn send_list_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_svn_list".to_string(),
        description: "Send an SVN directory listing (the entry list of a get-dir response)"
            .to_string(),
        parameters: vec![Parameter {
            name: "items".to_string(),
            type_hint: "array".to_string(),
            description: "Array of entries. Each entry: name (string), kind (\"file\" or \"dir\"), size (number, optional), revision (number, optional, the created-rev)".to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_svn_list",
            "items": [
                {"name": "trunk", "kind": "dir", "revision": 1},
                {"name": "branches", "kind": "dir", "revision": 1},
                {"name": "tags", "kind": "dir", "revision": 1}
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SVN list: {items_len} items")
                .with_debug("SVN list: {items_len} items returned"),
        ),
    }
}

fn send_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_svn_response".to_string(),
        description: "Send custom SVN protocol response".to_string(),
        parameters: vec![Parameter {
            name: "response".to_string(),
            type_hint: "string".to_string(),
            description: "SVN protocol response text".to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_svn_response",
            "response": "( success ( 42 ) )"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SVN response")
                .with_debug("SVN custom response: {response_len}B"),
        ),
    }
}

fn close_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_connection".to_string(),
        description: "Close the SVN connection".to_string(),
        parameters: vec![],
        example: json!({"type": "close_connection"}),
        log_template: Some(LogTemplate::new().with_info("-> SVN connection closed")),
    }
}

/// SVN greeting event - triggered when client first connects
pub static SVN_GREETING_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "svn_greeting",
        "SVN client connected, send protocol greeting",
        json!({
            "type": "send_svn_greeting",
            "min_version": 2,
            "max_version": 2
        }),
    )
    .with_parameters(vec![Parameter {
        name: "client_ip".to_string(),
        type_hint: "string".to_string(),
        description: "Address of the connecting client".to_string(),
        required: false,
    }])
    .with_actions(vec![send_greeting_action(), close_connection_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("{client_ip} SVN connected")
            .with_debug("SVN client connected from {client_ip}"),
    )
});

/// SVN command event - triggered when client sends a command
pub static SVN_COMMAND_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "svn_command",
        "SVN client sent a protocol command",
        // Rendered verbatim into the protocol documentation the model reads
        // (src/llm/actions/tools.rs, src/mcp_stdio/docs.rs), so it must be an action the
        // executor accepts - the previous {"type": "placeholder"} was not one.
        json!({
            "type": "send_svn_success",
            "data": "42"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "command_line".to_string(),
            type_hint: "string".to_string(),
            description: "The full command line received".to_string(),
            required: true,
        },
        Parameter {
            name: "command".to_string(),
            type_hint: "string".to_string(),
            description: "The parsed command name".to_string(),
            required: true,
        },
        Parameter {
            name: "args".to_string(),
            type_hint: "array".to_string(),
            description: "Command arguments".to_string(),
            required: false,
        },
        Parameter {
            name: "client_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Address of the connecting client".to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        send_success_action(),
        send_failure_action(),
        send_list_action(),
        send_response_action(),
        close_connection_action(),
    ])
    .with_alternative_example(json!({
        "type": "send_svn_failure",
        "error_code": 210005,
        "message": "Path not found"
    }))
    .with_log_template(
        LogTemplate::new()
            // No {duration_ms}: nothing measures one for this event, so the placeholder
            // rendered as an empty string and the line read "… SVN get-dir (ms)".
            .with_info("{client_ip} SVN {command}")
            .with_debug("SVN command from {client_ip}: {command}")
            .with_trace("SVN command: {command_line}"),
    )
});

pub fn get_svn_event_types() -> Vec<EventType> {
    vec![SVN_GREETING_EVENT.clone(), SVN_COMMAND_EVENT.clone()]
}
