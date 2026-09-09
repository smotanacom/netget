//! SSH protocol actions implementation

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

/// SSH protocol action handler.
///
/// Stateless. Live connection state belongs to the per-connection `SshHandler` in
/// `mod.rs`; the registry builds one of these for prompt generation and every handler
/// builds its own, so anything stored here would be invisible to the other copies.
pub struct SshProtocol;

impl Default for SshProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl SshProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SshProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        // No `send_first`: the server always raises ssh_banner when a shell opens, which is
        // the SSH equivalent of speaking first, and it does so regardless of any flag.
        Vec::new()
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // None. close_ssh_connection and list_ssh_connections used to be advertised here, but
        // both only produced an ActionResult::Custom that nothing consumed, and the connection
        // map they read was never populated - they always did nothing.
        Vec::new()
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_ssh_data_action(),
            wait_for_more_action(),
            close_this_connection_action(),
            sftp_handle_action(),
            sftp_directory_listing_action(),
            sftp_file_content_action(),
            sftp_file_attributes_action(),
            sftp_error_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "SSH"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_ssh_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>SSH"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["ssh"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            // Experimental, not Beta. Beta means "works against real clients", evidenced by a
            // test that a third-party implementation completed a real exchange. No such test
            // exists here: `tests/server/ssh` drives the server with a bare `TcpStream` and
            // asserts on the version-exchange banner, and its own comment records that the
            // `ssh2` client "has timing/compatibility issues with russh server" — i.e. the one
            // third-party client that was tried does not complete a session.
            //
            // russh is not the missing evidence either. It is the library this server is
            // *built on*, so driving it with russh would be the circular case
            // `tests/server/websocket/e2e_test.rs` describes, not an independent peer.
            //
            // Promote to Beta when a real SSH client (openssh's `ssh`/`sftp`, or `ssh2`)
            // completes auth and a channel exchange in a test that is neither `#[ignore]`d nor
            // skipped when the binary is absent.
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(22))
            .implementation("russh v0.45, russh-sftp v2.1; ephemeral Ed25519 host key")
            .llm_control("Auth decisions, shell banner and output, SFTP reads and listings")
            .e2e_testing(
                "No third-party SSH client completes a session in the automated suite. \
                 tests/server/ssh drives the socket directly (banner, version exchange, \
                 concurrent connects, script-vs-LLM routing) and tests/server/ssh/llm_failure_test \
                 covers the fail-closed paths. Interactive checks with openssh ssh/sftp were \
                 done by hand and are not reproducible in CI.",
            )
            .notes(
                "FAILS CLOSED: an LLM error, a handler that returns no ssh_auth_decision, and \
                 a malformed decision all deny, logged as decision=fail_closed_* and never as \
                 decision=model_reject. Shell and SFTP only: no port forwarding, no X11, no \
                 keyboard-interactive. SFTP is read-only (write/remove/mkdir/rmdir/rename are \
                 not implemented) and serves no real filesystem — every listing, stat and byte \
                 comes from the model. Host key is regenerated on every start, so clients warn \
                 about a changed key.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "Secure shell server for remote access"
    }
    fn example_prompt(&self) -> &'static str {
        "Pretent to be a shell via SSH on port 2222"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // Deterministic: answer every shell command with a canned line, no LLM
        // call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "ssh_shell_command":
    actions = [{"type": "ssh_shell_response", "response": "command not found\n"}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: LLM handles all SSH responses intelligently
            json!({
                "type": "open_server",
                "port": 2222,
                "base_stack": "ssh",
                "instruction": "SSH server providing secure shell access"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_server",
                "port": 2222,
                "base_stack": "ssh",
                "event_handlers": [{
                    "event_pattern": "ssh_shell_command",
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
                "port": 2222,
                "base_stack": "ssh",
                "event_handlers": [{
                    "event_pattern": "ssh_auth",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "ssh_auth_decision",
                            "allowed": true
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for SshProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::ssh::SshServer;
            #[allow(deprecated)]
            let listen_addr = ctx.socket_addr().unwrap_or(ctx.legacy_listen_addr());

            SshServer::spawn_with_llm_actions(
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
            "send_ssh_data" => self.execute_send_ssh_data(action),
            "wait_for_more" => Ok(ActionResult::WaitForMore),
            "close_this_connection" => Ok(ActionResult::CloseConnection),
            "ssh_auth_decision" => self.execute_ssh_auth_decision(action),
            "ssh_send_banner" => self.execute_ssh_send_banner(action),
            "ssh_shell_response" => self.execute_ssh_shell_response(action),
            // SFTP replies. These carry structured data back to sftp_handler.rs, which reads
            // them out of the execution result rather than writing bytes itself.
            "sftp_handle"
            | "sftp_directory_listing"
            | "sftp_file_content"
            | "sftp_file_attributes"
            | "sftp_error" => Ok(ActionResult::Custom {
                name: action_type.to_string(),
                data: action,
            }),
            _ => Err(anyhow::anyhow!("Unknown SSH action: {}", action_type)),
        }
    }
}

impl SshProtocol {
    fn execute_send_ssh_data(&self, action: serde_json::Value) -> Result<ActionResult> {
        let data = action
            .get("data")
            .and_then(|v| v.as_str())
            .context("Missing 'data' parameter")?;

        Ok(ActionResult::Output(data.as_bytes().to_vec()))
    }

    fn execute_ssh_auth_decision(&self, action: serde_json::Value) -> Result<ActionResult> {
        // Must be a real JSON boolean. The old code accepted any JSON value here, and the
        // consumer then did `as_bool()`, so "allowed": "true" silently became a denial.
        let allowed = action
            .get("allowed")
            .context("Missing 'allowed' parameter (a JSON boolean: true to accept the login, false to reject it)")?
            .as_bool()
            .context("'allowed' must be a JSON boolean, true or false - not a string or number")?;

        debug!("SSH auth decision action: allowed={}", allowed);

        // Store the decision in the action result metadata
        Ok(ActionResult::Custom {
            name: "ssh_auth_decision".to_string(),
            data: json!({"allowed": allowed}),
        })
    }

    fn execute_ssh_send_banner(&self, action: serde_json::Value) -> Result<ActionResult> {
        let banner = action
            .get("banner")
            .and_then(|v| v.as_str())
            .context("Missing 'banner' parameter")?;

        debug!("SSH sending banner: {}", banner);
        Ok(ActionResult::Output(banner.as_bytes().to_vec()))
    }

    fn execute_ssh_shell_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let response = action
            .get("response")
            .and_then(|v| v.as_str())
            .context("Missing 'response' parameter")?;

        debug!("SSH shell response: {}", response);
        Ok(ActionResult::Output(response.as_bytes().to_vec()))
    }
}

fn send_ssh_data_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_ssh_data".to_string(),
        description: "Write text to the client's terminal on the current shell channel. This is \
            a lower-level alias for ssh_shell_response and behaves identically, including the \
            \\n -> \\r\\n conversion; prefer ssh_shell_response when answering a command. This \
            is NOT a way to send raw SSH protocol bytes - the transport is encrypted by the \
            server and you never see or write wire-level SSH packets."
            .to_string(),
        parameters: vec![Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "Text for the terminal. Bare \\n is converted to \\r\\n before sending, \
                so you may write either"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_ssh_data",
            "data": "connection reset by peer\n"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SSH {output_bytes}B")
                .with_debug("SSH send_ssh_data: {output_bytes}B"),
        ),
    }
}

fn wait_for_more_action() -> ActionDefinition {
    ActionDefinition {
        name: "wait_for_more".to_string(),
        description: "Send nothing and wait for the client's next input line. The shell reads \
            one line at a time, so this simply means 'no output for this command'. Note the \
            server still writes its own \"$ \" prompt afterwards."
            .to_string(),
        parameters: vec![],
        example: json!({
            "type": "wait_for_more"
        }),
        log_template: Some(LogTemplate::new().with_debug("SSH waiting for more data")),
    }
}

fn close_this_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "End the shell session on the channel that raised this event: sends exit \
            status 0, then EOF, then closes the channel. Use it for 'exit', 'logout' or Ctrl-D. \
            Emit any farewell text with ssh_shell_response in the same response, before this \
            action."
            .to_string(),
        parameters: vec![],
        example: json!({
            "type": "close_this_connection"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("SSH connection closed")
                .with_debug("SSH close_this_connection"),
        ),
    }
}

// ============================================================================
// SFTP reply actions
//
// Each SFTP request expects exactly ONE of these in the response. They are read back by
// `sftp_handler.rs`, which turns them into the corresponding SSH_FXP_* packet.
// ============================================================================

fn sftp_handle_action() -> ActionDefinition {
    ActionDefinition {
        name: "sftp_handle".to_string(),
        description: "Answer an SFTP 'open' or 'opendir' request by granting a handle for the \
            path. The handle is an opaque string you choose; the client passes it back on every \
            later read/readdir/close for that file or directory, and the server remembers which \
            path it stood for. Reply with sftp_error instead if the path should not exist."
            .to_string(),
        parameters: vec![Parameter {
            name: "handle".to_string(),
            type_hint: "string".to_string(),
            description: "Opaque identifier for this open file or directory. Must be unique \
                among currently-open handles on this session. If omitted, the requested path is \
                used as the handle"
                .to_string(),
            required: false,
        }],
        example: json!({
            "type": "sftp_handle",
            "handle": "dir-1"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SFTP handle {handle}")
                .with_debug("SFTP sftp_handle: {handle}"),
        ),
    }
}

fn sftp_directory_listing_action() -> ActionDefinition {
    ActionDefinition {
        name: "sftp_directory_listing".to_string(),
        description: "Answer an SFTP 'readdir' request with the full contents of the directory. \
            Send every entry in one response - the server reports end-of-directory on the next \
            readdir for the same handle, so a second call will not be made. An empty 'entries' \
            array is a valid empty directory."
            .to_string(),
        parameters: vec![Parameter {
            name: "entries".to_string(),
            type_hint: "array".to_string(),
            description: "Array of objects, one per directory entry. 'name' (string, the bare \
                file name with no path) is required; an entry without it is dropped. 'is_dir' \
                (boolean, default false) and 'size' (number of bytes, default 0) are optional \
                but should be set - clients display them, and 'size' should match what \
                sftp_file_attributes reports for the same file. Permissions are derived from \
                'is_dir' (0755 for directories, 0644 for files)"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "sftp_directory_listing",
            "entries": [
                {"name": "readme.txt", "is_dir": false, "size": 24},
                {"name": "logs", "is_dir": true, "size": 4096}
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SFTP listing {entries_len} entries")
                .with_debug("SFTP sftp_directory_listing: {entries_len} entries"),
        ),
    }
}

fn sftp_file_content_action() -> ActionDefinition {
    ActionDefinition {
        name: "sftp_file_content".to_string(),
        description: "Answer an SFTP 'read' request with the contents of the file. IMPORTANT: \
            return the WHOLE file every time, not the slice the request asked for - the server \
            applies the request's offset and length for you. Keep the content consistent with \
            the 'size' you reported from sftp_file_attributes for the same path, otherwise the \
            client's download will be truncated or will not terminate."
            .to_string(),
        parameters: vec![Parameter {
            name: "content".to_string(),
            type_hint: "string".to_string(),
            description: "The complete file contents as text. Binary files cannot be served: \
                the string's UTF-8 bytes are what the client receives"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "sftp_file_content",
            "content": "Hello from NetGet SFTP!\n"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SFTP content {content_len}B")
                .with_debug("SFTP sftp_file_content: {content_len}B"),
        ),
    }
}

fn sftp_file_attributes_action() -> ActionDefinition {
    ActionDefinition {
        name: "sftp_file_attributes".to_string(),
        description: "Answer an SFTP 'lstat' or 'fstat' request with the metadata of a path. \
            Clients call this before downloading to learn the file size, so 'size' must match \
            the length of what sftp_file_content will return for the same path. Reply with \
            sftp_error instead if the path should not exist."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "size".to_string(),
                type_hint: "number".to_string(),
                description: "File size in bytes. Must equal the byte length of the 'content' \
                    you would return for this path"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "is_dir".to_string(),
                type_hint: "boolean".to_string(),
                description: "True if this path is a directory. Sets the directory bit in the \
                    reported mode, which is what makes `ls` and `cd` treat it as one"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "permissions".to_string(),
                type_hint: "number".to_string(),
                description: "Optional Unix mode as a decimal number (e.g. 420 for 0644, 493 \
                    for 0755). Defaults to 0644, with the directory bit added when is_dir is true"
                    .to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "sftp_file_attributes",
            "size": 24,
            "is_dir": false
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SFTP attrs size={size} dir={is_dir}")
                .with_debug("SFTP sftp_file_attributes: size={size}, is_dir={is_dir}"),
        ),
    }
}

fn sftp_error_action() -> ActionDefinition {
    ActionDefinition {
        name: "sftp_error".to_string(),
        description: "Fail the SFTP request with an error status. Use this for paths that should \
            not exist, permission denials, and operations you do not want to support. Returning \
            no action at all also fails the request, but with a less specific status - prefer \
            this so the client shows the right message."
            .to_string(),
        parameters: vec![Parameter {
            name: "code".to_string(),
            type_hint: "string".to_string(),
            description: "One of: \"no_such_file\" (the default, for a path that does not \
                exist), \"permission_denied\", \"failure\" (generic), \"op_unsupported\", or \
                \"eof\". Any other value is treated as \"failure\""
                .to_string(),
            required: false,
        }],
        example: json!({
            "type": "sftp_error",
            "code": "no_such_file"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SFTP error {code}")
                .with_debug("SFTP sftp_error: {code}"),
        ),
    }
}

// ============================================================================
// SSH Action Constants
// ============================================================================

/// SSH send banner action constant
pub static SSH_SEND_BANNER_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| ActionDefinition {
        name: "ssh_send_banner".to_string(),
        description: "Write the greeting shown the moment an SSH shell opens, before the user \
            has typed anything - a welcome line, MOTD or fake 'Last login' banner. Answers the \
            ssh_banner event. If the server should open silently, return no action at all \
            (or show_message, which is not sent to the client)."
            .to_string(),
        parameters: vec![Parameter {
            name: "banner".to_string(),
            type_hint: "string".to_string(),
            description: "The text to display. Use \\n for line breaks; it is converted to the \
                \\r\\n that terminals require. Do not append a \"$ \" prompt - the server writes \
                one after each command on its own"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "ssh_send_banner",
            "banner": "Welcome to NetGet SSH Server!\nType 'help' for available commands.\n"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SSH banner")
                .with_debug("SSH ssh_send_banner")
                .with_trace("SSH banner: {preview(banner,100)}"),
        ),
    });

/// SSH authentication decision action constant
pub static SSH_AUTH_DECISION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| ActionDefinition {
        name: "ssh_auth_decision".to_string(),
        description: "Accept or reject the login attempt in the ssh_auth event. This is the ONLY \
            valid answer to ssh_auth - if you return anything else, or nothing, the login is \
            rejected. Decide from the event's 'username', 'auth_type' and (for password logins) \
            'password' together with your instruction. There is no way to send a message with \
            the rejection; use ssh_send_banner after a successful login instead."
            .to_string(),
        parameters: vec![Parameter {
            name: "allowed".to_string(),
            type_hint: "boolean".to_string(),
            description: "JSON boolean, not a string: true accepts the login, false rejects it \
                and lets the client retry with another method"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "ssh_auth_decision",
            "allowed": true
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("SSH auth: {allowed}")
                .with_debug("SSH ssh_auth_decision: allowed={allowed}"),
        ),
    });

/// SSH shell response action constant
pub static SSH_SHELL_RESPONSE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| ActionDefinition {
        name: "ssh_shell_response".to_string(),
        description: "Write the output of a shell command to the client's terminal. Answers the \
            ssh_shell_command event. Produce what the real command would print (ls, pwd, cat, \
            uname, whoami, ...), or a shell-style error such as \"bash: foo: command not \
            found\". There is no real filesystem behind this - invent one and use memory \
            (set_memory/append_memory) to keep the current directory and any files consistent \
            across commands. To log the user out, add close_this_connection after this action."
            .to_string(),
        parameters: vec![Parameter {
            name: "response".to_string(),
            type_hint: "string".to_string(),
            description: "The command's output. Use \\n for line breaks; it is converted to the \
                \\r\\n terminals require, so plain Unix output works. Do not add a \"$ \" \
                prompt - the server writes one after your output. An empty string prints nothing"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "ssh_shell_response",
            "response": "/home/user\n"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SSH response {output_bytes}B")
                .with_debug("SSH ssh_shell_response: {output_bytes}B")
                .with_trace("SSH response: {preview(response,200)}"),
        ),
    });

/// SSH close connection action constant
pub static SSH_CLOSE_CONNECTION_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Close the SSH connection. Use this when the user types 'exit', 'logout', \
            or explicitly requests to close/disconnect. The connection will be terminated gracefully.".to_string(),
        parameters: vec![],
        example: json!({
            "type": "close_this_connection"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("SSH connection closed")
                .with_debug("SSH close_this_connection"),
        ),
    }
});

// ============================================================================
// SSH Event Type Constants
// ============================================================================
// These are static definitions that can be referenced throughout the codebase

/// SSH authentication event - triggered when a client attempts to authenticate
pub static SSH_AUTH_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_auth",
        "A client is trying to authenticate. You must answer with ssh_auth_decision: anything \
         else, including an error, is treated as a denial. The client may retry with a different \
         method or username, so expect this event more than once per connection.",
        json!({
            "type": "ssh_auth_decision",
            "allowed": true
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "username".to_string(),
            type_hint: "string".to_string(),
            description: "Username the client is attempting to log in as".to_string(),
            required: true,
        },
        Parameter {
            name: "auth_type".to_string(),
            type_hint: "string".to_string(),
            description: "Authentication method, exactly \"password\" or \"publickey\". Match on \
                this value in a script or static handler"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "password".to_string(),
            type_hint: "string".to_string(),
            description: "The password the client sent, in the clear. Present only when \
                auth_type is \"password\"; absent for \"publickey\". The key itself is verified \
                by the SSH transport and is not exposed here, so for publickey you are deciding \
                on the username alone"
                .to_string(),
            required: false,
        },
    ])
    .with_action(SSH_AUTH_DECISION_ACTION.clone())
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH auth: {username} ({auth_type}) from {client_ip}")
            .with_debug("SSH auth: user={username}, type={auth_type}")
            .with_trace("SSH auth: {json_pretty(.)}"),
    )
});

/// SSH banner event - triggered when a shell session opens
pub static SSH_BANNER_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_banner",
        "A shell session just opened, after successful authentication and before the user has \
         typed anything. Answer with ssh_send_banner to greet them, or return nothing for a \
         silent login.",
        json!({
            "type": "ssh_send_banner",
            "banner": "Welcome to NetGet SSH Server!\n"
        }),
    )
    // No parameters - banner is shown before any data is available
    .with_action(SSH_SEND_BANNER_ACTION.clone())
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH session opened from {client_ip}")
            .with_debug("SSH banner request from {client_ip}")
            .with_trace("SSH banner: {json_pretty(.)}"),
    )
});

/// SSH shell command event - triggered when user enters a command
pub static SSH_SHELL_COMMAND_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_shell_command",
        "The user pressed Enter (or a control key) in an SSH shell, or ran a one-shot command \
         with `ssh host <command>`. Answer with ssh_shell_response. The server echoes typed \
         characters and writes a \"$ \" prompt after your output on its own, so do not include a \
         prompt unless you want a second one.",
        json!({
            "type": "ssh_shell_response",
            "response": "/home/user\n"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "command".to_string(),
            type_hint: "string".to_string(),
            description: "The line the user typed, including the trailing newline, or the whole \
                command line for `ssh host <command>`. Control characters such as Ctrl-C (0x03) \
                appear here literally; the flags below are the reliable way to detect them"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "first_input".to_string(),
            type_hint: "boolean".to_string(),
            description: "True on the first Enter of this shell session. Useful for printing a \
                message-of-the-day or the initial prompt"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "empty_input".to_string(),
            type_hint: "boolean".to_string(),
            description: "True when the user pressed Enter on an empty line. Return an empty \
                response - the server prints the prompt itself"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "control".to_string(),
            type_hint: "array".to_string(),
            description: "Names of the control keys present in this input, as strings: \
                \"ctrl_c\" (interrupt), \"ctrl_d\" (end of file - usually means log out), \
                \"ctrl_z\" (suspend). Empty for ordinary commands"
                .to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        SSH_SHELL_RESPONSE_ACTION.clone(),
        SSH_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH cmd: {command}")
            .with_debug("SSH shell command: '{command}'")
            .with_trace("SSH command: {json_pretty(.)}"),
    )
});

/// SFTP operation event - triggered when SFTP client performs a filesystem operation
pub static SFTP_OPERATION_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sftp_operation",
        "An SFTP client asked for a filesystem operation. You are the filesystem: nothing is \
         read from or written to disk, so invent a consistent virtual tree and keep it in memory \
         across operations. Answer with exactly one reply action, chosen by 'operation': \
         opendir/open -> sftp_handle, readdir -> sftp_directory_listing, read -> \
         sftp_file_content, lstat/fstat -> sftp_file_attributes. Any operation may instead be \
         refused with sftp_error.",
        json!({
            "type": "sftp_directory_listing",
            "entries": [{"name": "readme.txt", "is_dir": false, "size": 24}]
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "operation".to_string(),
            type_hint: "string".to_string(),
            description: "Which request this is: \"opendir\", \"readdir\", \"open\", \"read\" or \
                \"lstat\". A client's fstat is resolved to the handle's path and arrives as \
                \"lstat\"; close and realpath are answered by the server and never reach you"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "path".to_string(),
            type_hint: "string".to_string(),
            description: "Absolute path being operated on. For handle-based operations \
                (readdir, read, fstat) this is the path the handle was opened with, resolved for \
                you"
            .to_string(),
            required: true,
        },
        Parameter {
            name: "handle".to_string(),
            type_hint: "string".to_string(),
            description: "The open handle this request refers to. Present for readdir, read and \
                fstat; absent for opendir, open and lstat"
                .to_string(),
            required: false,
        },
        Parameter {
            name: "offset".to_string(),
            type_hint: "number".to_string(),
            description: "Byte offset the client wants to read from ('read' only). Informational \
                - return the whole file in sftp_file_content and the server slices it"
                .to_string(),
            required: false,
        },
        Parameter {
            name: "length".to_string(),
            type_hint: "number".to_string(),
            description: "Number of bytes the client wants ('read' only). Informational, as for \
                'offset'"
                .to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        sftp_handle_action(),
        sftp_directory_listing_action(),
        sftp_file_content_action(),
        sftp_file_attributes_action(),
        sftp_error_action(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SFTP {operation} {path}")
            .with_debug("SFTP op={operation}, path={path}, handle={handle}")
            .with_trace("SFTP operation: {json_pretty(.)}"),
    )
});

/// Get SSH event types
pub fn get_ssh_event_types() -> Vec<EventType> {
    vec![
        SSH_AUTH_EVENT.clone(),
        SSH_BANNER_EVENT.clone(),
        SSH_SHELL_COMMAND_EVENT.clone(),
        SFTP_OPERATION_EVENT.clone(),
    ]
}
