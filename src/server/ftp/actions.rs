//! FTP protocol actions implementation
//!
//! Implements RFC 959 FTP command responses with LLM control.

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
use tracing::debug;

/// FTP protocol action handler
pub struct FtpProtocol;

impl FtpProtocol {
    pub fn new() -> Self {
        Self
    }

    fn execute_send_ftp_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let code = parse_ftp_code(&action)?;

        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .context("Missing 'message' parameter (the human-readable text after the code)")?;
        reject_line_breaks("message", message)?;

        let response = format!("{} {}\r\n", code, message);

        debug!("FTP sending response: {} {}", code, message);
        Ok(ActionResult::Output(response.as_bytes().to_vec()))
    }

    fn execute_send_ftp_multiline(&self, action: serde_json::Value) -> Result<ActionResult> {
        let code = parse_ftp_code(&action)?;

        let lines = action
            .get("lines")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        if lines.is_empty() {
            // Degenerate case: a multiline response with no lines is just a single-line
            // response. Fall back to 'message' so the client still gets a valid reply
            // rather than a bare code.
            let message = action
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("OK");
            reject_line_breaks("message", message)?;
            let response = format!("{} {}\r\n", code, message);
            return Ok(ActionResult::Output(response.as_bytes().to_vec()));
        }

        for line in &lines {
            reject_line_breaks("lines", line)?;
        }

        // Build multiline response
        let mut response = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i == lines.len() - 1 {
                // Last line uses space separator
                response.push_str(&format!("{} {}\r\n", code, line));
            } else {
                // Intermediate lines use dash separator
                response.push_str(&format!("{}-{}\r\n", code, line));
            }
        }

        debug!("FTP sending multiline response: code {}", code);
        Ok(ActionResult::Output(response.as_bytes().to_vec()))
    }

    fn execute_send_ftp_data(&self, action: serde_json::Value) -> Result<ActionResult> {
        let data = action
            .get("data")
            .and_then(|v| v.as_str())
            .context("Missing 'data' parameter")?;

        // Ensure data ends with exactly one CRLF.
        let formatted = if data.ends_with("\r\n") {
            data.to_string()
        } else if let Some(stripped) = data.strip_suffix('\n') {
            format!("{}\r\n", stripped)
        } else {
            format!("{}\r\n", data)
        };

        debug!("FTP sending data: {} bytes", formatted.len());
        Ok(ActionResult::Output(formatted.as_bytes().to_vec()))
    }

    fn execute_send_ftp_list(&self, action: serde_json::Value) -> Result<ActionResult> {
        let entries = action
            .get("entries")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        for entry in &entries {
            reject_line_breaks("entries", entry)?;
        }

        let entry_count = entries.len();
        let mut response = String::new();
        for entry in entries {
            response.push_str(entry);
            response.push_str("\r\n");
        }

        debug!("FTP sending LIST data: {} entries", entry_count);
        Ok(ActionResult::Output(response.as_bytes().to_vec()))
    }
}

impl Default for FtpProtocol {
    fn default() -> Self {
        Self::new()
    }
}

/// Reject CR or LF inside a piece of reply text the handler supplied.
///
/// FTP is line-oriented: a reply is terminated by CRLF and the client reads the next line as
/// a *new* reply. So a `message`, listing entry or multiline element containing CR or LF does
/// not produce a longer reply - it forges additional ones, and the client's reply-code state
/// machine is then reading text the handler never meant as a code. With `send_ftp_response`
/// that is a full response-splitting primitive: `"ok\r\n230 Logged in"` turns a 331 into a
/// successful login.
///
/// All three action descriptions already promised this ("Must not contain CR or LF", "Do not
/// include line endings", "the code and separators are added for you"). Nothing enforced it,
/// so the promise held only for as long as the model chose to keep it. Erroring is right
/// rather than stripping: a handler that meant several lines wants `send_ftp_multiline`, and
/// silently rewriting its text would hide the mistake.
fn reject_line_breaks(field: &str, value: &str) -> Result<()> {
    if let Some(pos) = value.find(['\r', '\n']) {
        return Err(anyhow::anyhow!(
            "FTP '{field}' must not contain CR or LF (found one at byte {pos}): a reply is \
             terminated by CRLF, so an embedded line break forges a second reply rather than \
             continuing this one. Use send_ftp_multiline for a reply spanning several lines."
        ));
    }
    Ok(())
}

/// Read and validate the `code` field of an FTP reply action.
///
/// RFC 959 replies are always a three-digit code, so anything outside 100-599 is a
/// mistake the client cannot interpret. Returning an error (rather than silently
/// substituting 500) makes the failure visible instead of shipping a wrong reply.
fn parse_ftp_code(action: &serde_json::Value) -> Result<u64> {
    let code = action
        .get("code")
        .and_then(|v| v.as_u64())
        .context("Missing 'code' parameter (a three-digit RFC 959 reply code, e.g. 220)")?;

    if !(100..=599).contains(&code) {
        return Err(anyhow::anyhow!(
            "Invalid FTP reply code {code}: RFC 959 codes are three digits in the range 100-599 \
             (e.g. 220 ready, 230 logged in, 331 need password, 550 not found)."
        ));
    }

    Ok(code)
}

// Implement Protocol trait (common functionality)
impl Protocol for FtpProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        // This server implements the FTP control connection only - there is no PASV/PORT
        // data connection, so there is no passive port range to configure. It also always
        // sends the 220 greeting itself, so it declares no `send_first` either.
        Vec::new()
    }

    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // FTP doesn't need async actions for now
        Vec::new()
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_ftp_response_action(),
            send_ftp_multiline_action(),
            send_ftp_data_action(),
            send_ftp_list_action(),
            wait_for_more_action(),
            close_connection_action(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "FTP"
    }

    fn get_event_types(&self) -> Vec<EventType> {
        get_ftp_event_types()
    }

    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>FTP"
    }

    fn keywords(&self) -> Vec<&'static str> {
        vec!["ftp", "file transfer", "ftp server"]
    }

    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(21))
            .implementation("Manual line-based parsing with tokio")
            .llm_control("All FTP replies on the control connection")
            .e2e_testing("raw TCP client (nc); real FTP clients cannot transfer files")
            .notes(
                "Control connection only: PASV/PORT are not implemented, so LIST/RETR/STOR \
                 cannot complete against a real FTP client. No TLS. No E2E test.",
            )
            .build()
    }

    fn description(&self) -> &'static str {
        "FTP file transfer server"
    }

    fn example_prompt(&self) -> &'static str {
        "Start an FTP server on port 21 that allows anonymous login and lists files"
    }

    fn group_name(&self) -> &'static str {
        "Application"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        // Deterministic: acknowledge every command with a 200 reply, no LLM
        // call. The connect event arrives as command="CONNECTION_ESTABLISHED".
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "ftp_command":
    actions = [{"type": "send_ftp_response", "code": 200, "message": "Command okay"}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        // NOTE: no `send_first` here. The server always emits ftp_command with
        // command="CONNECTION_ESTABLISHED" on connect so the handler can produce the 220
        // greeting; passing send_first would only produce an "unsupported" warning.
        StartupExamples::new(
            // LLM mode: LLM handles all FTP responses intelligently
            json!({
                "type": "open_server",
                "port": 21,
                "base_stack": "ftp",
                "instruction": "FTP server that allows anonymous login and responds to FTP commands"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_server",
                "port": 21,
                "base_stack": "ftp",
                "event_handlers": [{
                    "event_pattern": "ftp_command",
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
                "port": 21,
                "base_stack": "ftp",
                "event_handlers": [{
                    "event_pattern": "ftp_command",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_ftp_response",
                            "code": 500,
                            "message": "Command not recognized"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for FtpProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::ftp::FtpServer;
            #[allow(deprecated)]
            let listen_addr = ctx.socket_addr().unwrap_or(ctx.legacy_listen_addr());
            FtpServer::spawn_with_llm_actions(
                listen_addr,
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
            "send_ftp_response" => self.execute_send_ftp_response(action),
            "send_ftp_multiline" => self.execute_send_ftp_multiline(action),
            "send_ftp_data" => self.execute_send_ftp_data(action),
            "send_ftp_list" => self.execute_send_ftp_list(action),
            "wait_for_more" => Ok(ActionResult::WaitForMore),
            "close_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown FTP action: {}", action_type)),
        }
    }
}

// Action definitions

fn send_ftp_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_ftp_response".to_string(),
        description: "Send a single-line RFC 959 reply on the FTP control connection. The bytes \
            put on the wire are exactly \"<code> <message>\\r\\n\" - do not include the code in \
            'message' and do not add your own line ending. This is the normal way to answer every \
            FTP command."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "code".to_string(),
                type_hint: "number".to_string(),
                description: "Three-digit RFC 959 reply code, 100-599. 220 ready, 221 goodbye, \
                    230 logged in, 250 command ok, 257 pathname created, 331 need password, \
                    500 syntax error, 530 not logged in, 550 file unavailable. Any value outside \
                    100-599 is rejected"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Human-readable text placed after the code on the same line. Must \
                    not contain CR or LF - use send_ftp_multiline for a reply spanning several \
                    lines"
                    .to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "send_ftp_response",
            "code": 220,
            "message": "FTP Server Ready"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> FTP {code} {message}")
                .with_debug("FTP send_ftp_response: {code} {message}"),
        ),
    }
}

fn send_ftp_multiline_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_ftp_multiline".to_string(),
        description: "Send a multi-line RFC 959 reply, used for FEAT, HELP, STAT and similar. \
            Every line but the last is written as \"<code>-<line>\\r\\n\" and the last as \
            \"<code> <line>\\r\\n\", which is what tells the client the reply has ended. Supply \
            only the text of each line; the code and separators are added for you."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "code".to_string(),
                type_hint: "number".to_string(),
                description: "Three-digit RFC 959 reply code, 100-599, repeated on every line of \
                    the reply (e.g. 211 for FEAT, 214 for HELP)"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "lines".to_string(),
                type_hint: "array".to_string(),
                description: "Array of strings, one per line of the reply, in order. Include the \
                    closing line (e.g. \"End\") yourself. If this is omitted or empty the action \
                    falls back to sending a single-line reply using the 'message' field"
                    .to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "send_ftp_multiline",
            "code": 211,
            "lines": ["Features:", "UTF8", "End"]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> FTP {code} multiline")
                .with_debug("FTP send_ftp_multiline: code={code}, lines={lines_len}"),
        ),
    }
}

fn send_ftp_data_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_ftp_data".to_string(),
        description: "Write raw text on the FTP CONTROL connection, terminated with CRLF. \
            WARNING: this server implements no data connection (PASV/PORT are not supported), so \
            this does NOT perform an FTP file transfer - the bytes appear inline in the command \
            channel. A real FTP client will reject or misparse them. Use it only with raw clients \
            such as `nc`, or to send a reply line that send_ftp_response cannot express."
            .to_string(),
        parameters: vec![Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "Text to write on the control connection. Exactly one trailing CRLF is \
                ensured: a bare \"\\n\" is upgraded to \"\\r\\n\" and a missing ending is added"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_ftp_data",
            "data": "Hello from FTP"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> FTP data {output_bytes}B")
                .with_debug("FTP send_ftp_data: {output_bytes}B")
                .with_trace("FTP data: {preview(data,200)}"),
        ),
    }
}

fn send_ftp_list_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_ftp_list".to_string(),
        description: "Write directory listing lines, one per entry, each terminated with CRLF. \
            WARNING: a real FTP client expects a LIST/NLST listing on a separate data connection, \
            and this server has none - the lines go out on the control connection instead, where \
            a real client will not accept them. Useful for raw clients (`nc`) and for showing a \
            plausible listing to an attacker; not for interoperating with `ftp`/`lftp`/`curl`."
            .to_string(),
        parameters: vec![Parameter {
            name: "entries".to_string(),
            type_hint: "array".to_string(),
            description: "Array of strings, one per directory entry, already formatted as \
                `ls -l` output (permissions, links, owner, group, size, date, name). Do not \
                include line endings; CRLF is appended to each entry"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_ftp_list",
            "entries": [
                "-rw-r--r-- 1 ftp ftp 1024 Jan 01 00:00 file.txt",
                "drwxr-xr-x 2 ftp ftp 4096 Jan 01 00:00 subdir"
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> FTP LIST {entries_len} entries")
                .with_debug("FTP send_ftp_list: {entries_len} entries"),
        ),
    }
}

fn wait_for_more_action() -> ActionDefinition {
    ActionDefinition {
        name: "wait_for_more".to_string(),
        description: "Wait for more data before responding".to_string(),
        parameters: vec![],
        example: json!({
            "type": "wait_for_more"
        }),
        log_template: Some(LogTemplate::new().with_debug("FTP waiting for more data")),
    }
}

fn close_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_connection".to_string(),
        description: "Close the FTP connection".to_string(),
        parameters: vec![],
        example: json!({
            "type": "close_connection"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("FTP connection closed")
                .with_debug("FTP close_connection"),
        ),
    }
}

// ============================================================================
// FTP Action Constants
// ============================================================================

pub static SEND_FTP_RESPONSE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(send_ftp_response_action);
pub static SEND_FTP_MULTILINE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(send_ftp_multiline_action);
pub static SEND_FTP_DATA_ACTION: LazyLock<ActionDefinition> = LazyLock::new(send_ftp_data_action);
pub static SEND_FTP_LIST_ACTION: LazyLock<ActionDefinition> = LazyLock::new(send_ftp_list_action);
pub static WAIT_FOR_MORE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(wait_for_more_action);
pub static CLOSE_CONNECTION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(close_connection_action);

// ============================================================================
// FTP Event Type Constants
// ============================================================================

/// FTP command event - triggered on connect and for every command line thereafter
pub static FTP_COMMAND_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ftp_command",
        "A client connected, or sent a command line on the FTP control connection. This is the \
         only FTP event: the connect case is signalled by the literal command \
         'CONNECTION_ESTABLISHED', for which you must reply with a 220 greeting before the client \
         will send anything else.",
        json!({"type": "send_ftp_response", "code": 220, "message": "FTP Server Ready"}),
    )
    .with_parameters(vec![Parameter {
        name: "command".to_string(),
        type_hint: "string".to_string(),
        description: "The command line as sent by the client, with the trailing CRLF stripped \
            (e.g. 'USER anonymous', 'PASS x@y.z', 'SYST', 'PWD', 'LIST', 'RETR file.txt', \
            'QUIT'). The verb is NOT upper-cased or split out for you. One exception: the \
            literal value 'CONNECTION_ESTABLISHED' is not from the client - it means the TCP \
            connection has just been accepted and the server is waiting for your 220 greeting"
            .to_string(),
        required: true,
    }])
    .with_actions(vec![
        SEND_FTP_RESPONSE_ACTION.clone(),
        SEND_FTP_MULTILINE_ACTION.clone(),
        SEND_FTP_DATA_ACTION.clone(),
        SEND_FTP_LIST_ACTION.clone(),
        WAIT_FOR_MORE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("FTP {client_ip}: {command}")
            .with_debug("FTP command from {client_ip}:{client_port}: {command}")
            .with_trace("FTP: {json_pretty(.)}"),
    )
});

/// Get FTP event types
pub fn get_ftp_event_types() -> Vec<EventType> {
    vec![FTP_COMMAND_EVENT.clone()]
}
