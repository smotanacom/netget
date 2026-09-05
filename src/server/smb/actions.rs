//! SMB protocol actions implementation

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

/// SMB protocol action handler
pub struct SmbProtocol;

impl Default for SmbProtocol {
    fn default() -> Self {
        Self
    }
}

impl SmbProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SmbProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![disconnect_client_action()]
    }
    /// SMB raises exactly one event, and it must be declared here as well as emitted.
    ///
    /// This was missing, so the trait default applied and returned an empty vec: SMB_OPERATION_EVENT
    /// was built and dispatched at runtime, but invisible to anything that walks the registry.
    /// `tests/event_action_declarations_test.rs` audits every registered protocol's events and
    /// therefore audited none of SMB's, and `tests/mock_event_ids_test.rs` reported all nineteen
    /// of this protocol's own mock rules as naming an unknown event.
    fn get_event_types(&self) -> Vec<crate::protocol::EventType> {
        vec![SMB_OPERATION_EVENT.clone()]
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        // Every action listed here has an executor branch in src/server/smb/mod.rs and is
        // attached to SMB_OPERATION_EVENT below, so the model can both see it and have it
        // take effect.
        //
        // `smb_delete_file` and `smb_delete_directory` used to be listed and were removed:
        // SMB2 has no DELETE command. A client deletes by opening the file and issuing
        // SET_INFO with FileDispositionInformation (MS-SMB2 2.2.39 / 2.2.21), and this
        // server does not implement SET_INFO at all - the command falls through to the
        // "Unknown SMB2 command" arm. Advertising a delete action the server can never be
        // asked to perform only gave the model a response that did nothing.
        vec![
            smb_auth_success_action(),
            smb_auth_deny_action(),
            smb_list_directory_action(),
            smb_read_file_action(),
            smb_write_file_action(),
            smb_get_file_info_action(),
            smb_create_file_action(),
            smb_create_directory_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "SMB"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>SMB"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["smb", "cifs"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("Manual SMB2 protocol (0x0210 dialect)")
            .llm_control(
                "Authentication (allow/deny), directory listings, file metadata, file content on \
                 read, file-vs-directory on create, and write authorisation. File payloads carry \
                 an explicit `encoding` field (utf8/base64/hex) in both directions, so binary \
                 content survives a read and a written binary payload is shown to the model \
                 losslessly.",
            )
            .e2e_testing(
                "Raw SMB2 packets over TCP against a mocked LLM \
                 (tests/server/smb/e2e_test.rs). Verified at wire level: NEGOTIATE, \
                 SESSION_SETUP allow and deny, a full NEGOTIATE -> SESSION_SETUP -> \
                 TREE_CONNECT -> CREATE -> READ flow in which a non-UTF-8 byte string sent \
                 as base64 comes back byte-for-byte in the READ response body, the \
                 FILE_ATTRIBUTE_DIRECTORY bit set by smb_create_directory, and WRITE \
                 answering STATUS_ACCESS_DENIED when the model does not return \
                 smb_write_file. NOT verified against smbclient or Windows Explorer - no \
                 real SMB client has ever been run against this server.",
            )
            .notes(
                "SMB 2.1 only, guest auth only, no signing/encryption, no SET_INFO (so no \
                 delete/rename), timestamps are zero, and tree/session IDs in responses are \
                 hardcoded rather than echoed. Adjacent operations share no state beyond the \
                 file-handle table: the model is the filesystem.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "SMB/CIFS file server"
    }
    fn example_prompt(&self) -> &'static str {
        "Start an SMB/CIFS file server on port 8445"
    }
    fn group_name(&self) -> &'static str {
        "Web & File"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        // Deterministic: accept every session on this share, no LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "smb_operation":
    actions = [{"type": "smb_auth_success",
                "username": event.get("username", "guest")}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode
            json!({
                "type": "open_server",
                "port": 445,
                "base_stack": "smb",
                "instruction": "SMB file server. Accept all guest connections. Provide /documents directory with sample files. Return file content on reads."
            }),
            // Script mode
            json!({
                "type": "open_server",
                "port": 445,
                "base_stack": "smb",
                "event_handlers": [{
                    "event_pattern": "smb_operation",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }]
            }),
            // Static mode
            json!({
                "type": "open_server",
                "port": 445,
                "base_stack": "smb",
                "event_handlers": [{
                    "event_pattern": "smb_operation",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "smb_auth_success",
                            "username": "guest"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for SmbProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::smb::SmbServer;
            SmbServer::spawn_with_llm_actions(
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

        // Return Custom result with the action data for SMB server to handle
        Ok(ActionResult::Custom {
            name: action_type.to_string(),
            data: action,
        })
    }
}

// Event type for SMB operations
pub static SMB_OPERATION_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "smb_operation",
        "An SMB2 client asked for a filesystem operation. You are the filesystem: nothing is \
         read from or written to disk, so invent a consistent virtual tree and keep it in memory \
         across operations. Answer with the action matching 'operation': session_setup -> \
         smb_auth_success or smb_auth_deny, create -> smb_create_file or smb_create_directory \
         (which one decides whether the client is told the handle is a directory), read -> \
         smb_read_file, write -> smb_write_file (the write is REFUSED with STATUS_ACCESS_DENIED \
         unless you return it), query_info -> smb_get_file_info, query_directory -> \
         smb_list_directory.",
        json!({
            "type": "smb_read_file",
            "path": "/documents/file.txt",
            "content": "Sample file content",
            "encoding": "utf8"
        }),
    )
    // Every action here has an executor branch in src/server/smb/mod.rs. `call_llm` builds the
    // model's tool list from this list, not from get_sync_actions(), so anything missing here is
    // invisible to the model and anything here without an executor branch is a no-op it can emit.
    // Keep the two in sync.
    .with_actions(vec![
        smb_auth_success_action(),
        smb_auth_deny_action(),
        smb_create_file_action(),
        smb_create_directory_action(),
        smb_read_file_action(),
        smb_write_file_action(),
        smb_get_file_info_action(),
        smb_list_directory_action(),
    ])
    .with_parameters(vec![
        Parameter {
            name: "operation".to_string(),
            type_hint: "string".to_string(),
            description: "Which request this is: \"session_setup\" (authentication), \"create\" \
                          (open), \"read\", \"write\", \"query_info\" (stat) or \
                          \"query_directory\" (list)"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "path".to_string(),
            type_hint: "string".to_string(),
            description: "The file or directory path being accessed".to_string(),
            required: false,
        },
        Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "write only: the bytes the client wrote, rendered according to the \
                          'encoding' field of this event. Absent for every other operation."
                .to_string(),
            required: false,
        },
        Parameter {
            name: "encoding".to_string(),
            type_hint: "string".to_string(),
            description: "write only: how to read 'data'. \"utf8\" means 'data' is the written \
                          bytes as literal text; \"base64\" means 'data' is the written bytes \
                          base64-encoded, used whenever they are not all printable ASCII. To \
                          hand the same bytes back on a later read, pass this 'data' and this \
                          'encoding' straight into smb_read_file's 'content' and 'encoding'."
                .to_string(),
            required: false,
        },
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SMB {operation} {path}")
            .with_debug("SMB {operation}: {path}")
            .with_trace("SMB: {json_pretty(.)}"),
    )
});

// ============================================================================
// Payload encoding
//
// SMB carries file *contents*, which are routinely not text. Both directions
// therefore carry an explicit `encoding` field next to the payload string, and
// there is deliberately no sniffing: "SGVsbG8=" is simultaneously valid text and
// valid base64, and only the sender knows which it means. This is the same shape
// as `send_tcp_data`'s `encoding` field (d70bb5b5); the defect fixed here was
// that `smb_read_file.content` was documented as "base64 encoded for binary"
// while the executor did `.as_bytes()`, so a model that followed the
// documentation put literal base64 ASCII into the file.
// ============================================================================

/// Turn an outbound payload string into the exact bytes the server writes into an
/// SMB2 response, honouring the action's optional `encoding` field.
///
/// - absent or `"utf8"`: the string's UTF-8 bytes, verbatim (default, backwards compatible)
/// - `"base64"`: standard base64, so `"SGVsbG8="` yields the 5 bytes `Hello`
/// - `"hex"`: two hex digits per byte, so `"48656c6c6f"` yields the same 5 bytes
pub fn decode_smb_payload(payload: &str, encoding: Option<&str>) -> Result<Vec<u8>> {
    use base64::Engine as _;

    match encoding.unwrap_or("utf8") {
        "utf8" | "text" => Ok(payload.as_bytes().to_vec()),
        "base64" => {
            // Models frequently wrap long base64 across lines; tolerate whitespace.
            let cleaned: String = payload
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(&cleaned)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Invalid base64 in payload ({payload:?}): {e}. To send this string as \
                         literal text instead, omit 'encoding' or set it to \"utf8\"."
                    )
                })
        }
        "hex" => {
            let cleaned: String = payload
                .chars()
                .filter(|c| !c.is_ascii_whitespace() && *c != ':')
                .collect();
            let cleaned = cleaned.strip_prefix("0x").unwrap_or(&cleaned);
            if cleaned.len() % 2 != 0 {
                return Err(anyhow::anyhow!(
                    "Invalid hex in payload: expected an even number of hex digits, got {} \
                     ({payload:?}). Each byte is two hex digits, e.g. \"48656c6c6f\" = \"Hello\".",
                    cleaned.len()
                ));
            }
            hex::decode(cleaned).map_err(|e| {
                anyhow::anyhow!(
                    "Invalid hex in payload ({payload:?}): {e}. Use only 0-9/a-f, two digits per \
                     byte. To send this string as literal text instead, omit 'encoding' or set \
                     it to \"utf8\"."
                )
            })
        }
        other => Err(anyhow::anyhow!(
            "Invalid 'encoding' value {other:?}. Valid values are \"utf8\" (default, the \
             string's characters as-is), \"base64\" and \"hex\"."
        )),
    }
}

/// Render bytes received from the client for the model, together with the `encoding`
/// name that says how to read them back.
///
/// Printable ASCII is passed through as text so ordinary text files stay readable in
/// prompts and logs; anything else is base64-encoded rather than lossily converted.
/// The pair is symmetric with [`decode_smb_payload`]: feeding the returned string and
/// encoding back through it reproduces the original bytes exactly.
pub fn encode_smb_payload(bytes: &[u8]) -> (String, &'static str) {
    use base64::Engine as _;

    if bytes
        .iter()
        .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
    {
        (String::from_utf8_lossy(bytes).to_string(), "utf8")
    } else {
        (
            base64::engine::general_purpose::STANDARD.encode(bytes),
            "base64",
        )
    }
}

/// Shared `encoding` parameter for every action carrying an outbound payload string.
fn encoding_parameter(payload_field: &str) -> Parameter {
    Parameter {
        name: "encoding".to_string(),
        type_hint: "string".to_string(),
        description: format!(
            "How to turn '{payload_field}' into the bytes the client receives. \"utf8\" (the \
             default when omitted) uses the characters of '{payload_field}' unchanged - use it \
             for text files. \"base64\" decodes '{payload_field}' as standard base64 and \
             \"hex\" as hex digits - use one of those for binary files, e.g. \
             {{\"{payload_field}\": \"SGVsbG8=\", \"encoding\": \"base64\"}} delivers the 5 \
             bytes 'Hello', whereas the same value without \"encoding\" delivers the 8 \
             characters S-G-V-s-b-G-8-=. There is no auto-detection. No other values are \
             accepted"
        ),
        required: false,
    }
}

// Action definitions

fn disconnect_client_action() -> ActionDefinition {
    ActionDefinition {
        name: "disconnect_client".to_string(),
        description: "Disconnect an SMB client".to_string(),
        parameters: vec![Parameter {
            name: "client".to_string(),
            type_hint: "string".to_string(),
            description: "Client address to disconnect".to_string(),
            required: true,
        }],
        example: json!({
            "type": "disconnect_client",
            "client": "192.168.1.100:54321"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("SMB disconnect {client}")
                .with_debug("SMB disconnect_client: {client}"),
        ),
    }
}

fn smb_list_directory_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_list_directory".to_string(),
        description: "List files in a directory".to_string(),
        parameters: vec![
            Parameter {
                name: "path".to_string(),
                type_hint: "string".to_string(),
                description: "Directory path to list".to_string(),
                required: true,
            },
            Parameter {
                name: "files".to_string(),
                type_hint: "array".to_string(),
                description: "Array of file objects with name, size, is_directory, modified_time"
                    .to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "smb_list_directory",
            "path": "/documents",
            "files": [
                {
                    "name": "report.pdf",
                    "size": 524288,
                    "is_directory": false,
                    "modified_time": "2025-01-15T10:30:00Z"
                }
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB DIR {path} ({files_len} files)")
                .with_debug("SMB smb_list_directory: path={path}, {files_len} files"),
        ),
    }
}

fn smb_read_file_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_read_file".to_string(),
        description: "Answer a 'read' operation with the file's contents. The bytes in 'content' \
                      (interpreted according to 'encoding') become the body of the SMB2 READ \
                      response."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "path".to_string(),
                type_hint: "string".to_string(),
                description: "File path to read".to_string(),
                required: true,
            },
            Parameter {
                name: "content".to_string(),
                type_hint: "string".to_string(),
                description: "File content. Interpreted according to 'encoding': by default the \
                              characters of this string are delivered as-is (UTF-8)."
                    .to_string(),
                required: true,
            },
            encoding_parameter("content"),
        ],
        example: json!({
            "type": "smb_read_file",
            "path": "/documents/file.txt",
            "content": "Hello, World!",
            "encoding": "utf8"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB READ {path}")
                .with_debug("SMB smb_read_file: path={path}"),
        ),
    }
}

fn smb_write_file_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_write_file".to_string(),
        description: "Accept a 'write' operation. The client has already sent the bytes - they \
                      are in the event's 'data' field - so this action does not carry them back; \
                      it authorises the write and the server answers STATUS_SUCCESS. If you do \
                      NOT return this action for a 'write' operation the write is refused with \
                      STATUS_ACCESS_DENIED, so silence is a denial, not an approval."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "path".to_string(),
                type_hint: "string".to_string(),
                description: "File path being written (echo the event's 'path')".to_string(),
                required: true,
            },
            Parameter {
                name: "bytes_written".to_string(),
                type_hint: "number".to_string(),
                description: "How many bytes to report as written. Omit to report all the bytes \
                              the client sent, which is what a normal filesystem does. A smaller \
                              number tells the client the write was partial."
                    .to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "smb_write_file",
            "path": "/documents/file.txt"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB WRITE OK {path}")
                .with_debug("SMB smb_write_file: path={path}"),
        ),
    }
}

fn smb_get_file_info_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_get_file_info".to_string(),
        description: "Get file metadata".to_string(),
        parameters: vec![
            Parameter {
                name: "path".to_string(),
                type_hint: "string".to_string(),
                description: "File path".to_string(),
                required: true,
            },
            Parameter {
                name: "size".to_string(),
                type_hint: "number".to_string(),
                description: "File size in bytes".to_string(),
                required: true,
            },
            Parameter {
                name: "is_directory".to_string(),
                type_hint: "boolean".to_string(),
                description: "Whether path is a directory".to_string(),
                required: true,
            },
            Parameter {
                name: "modified_time".to_string(),
                type_hint: "string".to_string(),
                description: "Last modified time (ISO 8601)".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "smb_get_file_info",
            "path": "/documents/file.txt",
            "size": 1024,
            "is_directory": false,
            "modified_time": "2025-01-15T10:30:00Z"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB INFO {path}")
                .with_debug("SMB smb_get_file_info: path={path}, size={size}"),
        ),
    }
}

fn smb_create_file_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_create_file".to_string(),
        description: "Answer a 'create' operation: the path is (or becomes) a regular file. The \
                      handle the client receives is marked FILE_ATTRIBUTE_NORMAL, so the client \
                      will follow up with read/write rather than query_directory. This is also \
                      the default when you return neither create action."
            .to_string(),
        parameters: vec![Parameter {
            name: "path".to_string(),
            type_hint: "string".to_string(),
            description: "File path being opened or created".to_string(),
            required: true,
        }],
        example: json!({
            "type": "smb_create_file",
            "path": "/documents/newfile.txt"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB CREATE {path}")
                .with_debug("SMB smb_create_file: path={path}"),
        ),
    }
}

// `smb_delete_file` and `smb_delete_directory` used to be defined here. SMB2 has no DELETE
// command - deletion is SET_INFO/FileDispositionInformation on an open handle - and this
// server does not implement SET_INFO, so neither action could ever have been requested or
// routed. They were removed rather than left advertised.

fn smb_create_directory_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_create_directory".to_string(),
        description: "Answer a 'create' operation: the path is (or becomes) a directory. The \
                      handle the client receives carries FILE_ATTRIBUTE_DIRECTORY, which is what \
                      makes the client issue query_directory against it instead of read."
            .to_string(),
        parameters: vec![Parameter {
            name: "path".to_string(),
            type_hint: "string".to_string(),
            description: "Directory path being opened or created".to_string(),
            required: true,
        }],
        example: json!({
            "type": "smb_create_directory",
            "path": "/documents/newdir"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB MKDIR {path}")
                .with_debug("SMB smb_create_directory: path={path}"),
        ),
    }
}

fn smb_auth_success_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_auth_success".to_string(),
        description: "Allow SMB authentication for the user (respond to session_setup event)"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "username".to_string(),
                type_hint: "string".to_string(),
                description: "Username that was authenticated".to_string(),
                required: true,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Optional message explaining why auth was allowed".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "smb_auth_success",
            "username": "alice",
            "message": "User alice authenticated successfully"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB AUTH OK {username}")
                .with_debug("SMB smb_auth_success: user={username}"),
        ),
    }
}

fn smb_auth_deny_action() -> ActionDefinition {
    ActionDefinition {
        name: "smb_auth_deny".to_string(),
        description: "Deny SMB authentication for the user (respond to session_setup event)"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "username".to_string(),
                type_hint: "string".to_string(),
                description: "Username that was denied".to_string(),
                required: true,
            },
            Parameter {
                name: "reason".to_string(),
                type_hint: "string".to_string(),
                description: "Reason for denying authentication".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "smb_auth_deny",
            "username": "hacker",
            "reason": "User not authorized"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SMB AUTH DENIED {username}")
                .with_debug("SMB smb_auth_deny: user={username}, reason={reason}"),
        ),
    }
}
