//! SSH client protocol actions implementation

use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::{ConnectContext, EventType};
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

/// SSH client connected event
pub static SSH_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_connected",
        "SSH client successfully authenticated to server",
        json!({
            "type": "execute_command",
            "command": "pwd"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "remote_addr".to_string(),
            type_hint: "string".to_string(),
            description: "Remote SSH server address".to_string(),
            required: true,
        },
        Parameter {
            name: "username".to_string(),
            type_hint: "string".to_string(),
            description: "Username used for authentication".to_string(),
            required: true,
        },
    ])
});

/// SSH command output received event
pub static SSH_CLIENT_OUTPUT_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_output_received",
        "Command output received from SSH server",
        json!({
            "type": "execute_command",
            "command": "pwd"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "command".to_string(),
            type_hint: "string".to_string(),
            description: "The command this output belongs to".to_string(),
            required: true,
        },
        Parameter {
            name: "output".to_string(),
            type_hint: "string".to_string(),
            description: "The command's stdout as a UTF-8 string".to_string(),
            required: true,
        },
        Parameter {
            name: "stderr".to_string(),
            type_hint: "string".to_string(),
            description: "The command's stderr, when it wrote any".to_string(),
            required: false,
        },
        Parameter {
            name: "exit_code".to_string(),
            type_hint: "number".to_string(),
            description: "The command's exit status, when the server sent one".to_string(),
            required: false,
        },
    ])
});

/// SSH client protocol action handler
pub struct SshClientProtocol;

impl Default for SshClientProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl SshClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SshClientProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "execute_command".to_string(),
                description: "Execute a command on the remote SSH server".to_string(),
                parameters: vec![Parameter {
                    name: "command".to_string(),
                    type_hint: "string".to_string(),
                    description: "The shell command to execute".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "execute_command",
                    "command": "ls -la"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from the SSH server".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "disconnect"
                }),
                log_template: None,
            },
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "execute_command".to_string(),
                description: "Execute another command in response to output".to_string(),
                parameters: vec![Parameter {
                    name: "command".to_string(),
                    type_hint: "string".to_string(),
                    description: "The shell command to execute".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "execute_command",
                    "command": "pwd"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "wait_for_more".to_string(),
                description: "Wait for more output before responding".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "wait_for_more"
                }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "SSH"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "ssh_connected",
                "Triggered when SSH client authenticates successfully",
                json!({"type": "placeholder", "event_id": "ssh_connected"}),
            ),
            EventType::new(
                "ssh_output_received",
                "Triggered when SSH command output is received",
                json!({"type": "placeholder", "event_id": "ssh_output_received"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>SSH"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["ssh", "ssh client", "connect to ssh", "secure shell"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Beta)
            .implementation(
                "russh 0.45; password or public-key authentication (private_key_path). One \
                 session channel per command; stdout, stderr and exit status go back to the \
                 model, and its answer is executed in turn, bounded at four follow-ups",
            )
            .llm_control("Execute commands and read output")
            .e2e_testing(
                "tests/client/ssh/real_server_test.rs, 12 LLM calls, against OpenSSH's sshd run \
                 unprivileged from a temp-dir config with ssh-keygen keys, public-key auth as \
                 the current user. The model's first command writes a file and exits 3 with \
                 stderr; its second, built from that stdout, stderr and exit status, appends \
                 to the file; it then disconnects. The file must hold both lines exactly and \
                 sshd's log must show the publickey login and the disconnect. A key sshd does \
                 not know is refused, and a model that commands forever runs exactly five \
                 commands. Not #[ignore]d; a missing sshd or ssh-keygen fails the test.",
            )
            .notes(
                "The host key is accepted unconditionally (no known_hosts check, no pinning), \
                 so this client is not safe against an active network attacker. Password \
                 authentication is not exercised against a real server (an unprivileged sshd \
                 cannot check one). No SSH agent, keyboard-interactive, certificates, PTY, \
                 SFTP or forwarding. The private key is read from the operator's path at \
                 connect and never reaches the model.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "SSH client for connecting to SSH servers and executing commands"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to SSH at localhost:22 with user 'admin' and execute 'ls -la'"
    }
    fn group_name(&self) -> &'static str {
        "Network Infrastructure"
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "username".to_string(),
                type_hint: "string".to_string(),
                description: "SSH username for authentication".to_string(),
                required: true,
                example: json!("testuser"),
                default: None,
            },
            ParameterDefinition {
                name: "password".to_string(),
                type_hint: "string".to_string(),
                description: "SSH password for authentication (if using password auth)".to_string(),
                required: false,
                example: json!("testpass"),
                default: None,
            },
            ParameterDefinition {
                name: "auth_method".to_string(),
                type_hint: "string".to_string(),
                description: "Authentication method: 'password' or 'publickey' (default: \
                              publickey when private_key_path is given, otherwise password)"
                    .to_string(),
                required: false,
                example: json!("publickey"),
                default: None,
            },
            ParameterDefinition {
                name: "private_key_path".to_string(),
                type_hint: "string".to_string(),
                description: "Path to an OpenSSH-format private key file for publickey \
                              authentication. Read once at connect; its contents never reach \
                              the model"
                    .to_string(),
                required: false,
                example: json!("/home/user/.ssh/id_ed25519"),
                default: None,
            },
            ParameterDefinition {
                name: "private_key_passphrase".to_string(),
                type_hint: "string".to_string(),
                description: "Passphrase for an encrypted private_key_path".to_string(),
                required: false,
                example: json!("key passphrase"),
                default: None,
            },
        ]
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls SSH commands
            json!({
                "type": "open_client",
                "remote_addr": "localhost:22",
                "base_stack": "ssh",
                "startup_params": {
                    "username": "user",
                    "password": "pass"
                },
                "instruction": "Execute 'uname -a' and 'df -h' to check system info"
            }),
            // Script mode: Code-based command execution
            json!({
                "type": "open_client",
                "remote_addr": "localhost:22",
                "base_stack": "ssh",
                "startup_params": {
                    "username": "user",
                    "password": "pass"
                },
                "event_handlers": [{
                    "event_pattern": "ssh_output_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<ssh_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed command sequence
            json!({
                "type": "open_client",
                "remote_addr": "localhost:22",
                "base_stack": "ssh",
                "startup_params": {
                    "username": "user",
                    "password": "pass"
                },
                "event_handlers": [
                    {
                        "event_pattern": "ssh_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "execute_command",
                                "command": "hostname"
                            }]
                        }
                    },
                    {
                        "event_pattern": "ssh_output_received",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "disconnect"
                            }]
                        }
                    }
                ]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for SshClientProtocol {
    fn connect(
        &self,
        ctx: ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::ssh::SshClient;

            SshClient::connect_with_llm_actions(
                ctx.remote_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.client_id,
                ctx.startup_params,
            )
            .await
        })
    }
    fn execute_action(&self, action: serde_json::Value) -> Result<ClientActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "execute_command" => {
                let command = action
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing 'command' field")?;

                Ok(ClientActionResult::Custom {
                    name: "execute_command".to_string(),
                    data: json!({ "command": command }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown SSH client action: {}",
                action_type
            )),
        }
    }
}
