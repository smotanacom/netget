//! SSH Agent server protocol actions

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::Result;
use serde_json::json;
use std::sync::LazyLock;

// ============================================================================
// Action constants
//
// These are defined here rather than inline in `get_sync_actions` so that each event type can
// attach the actions that actually answer it. An event that lists none leaves the model with
// only the common actions (`set_memory`, `show_message`, ...), so every agent action it returns
// is rejected as unknown and the request fails after two retries - which is what happened to
// all eight of these events. See `tests/event_action_declarations_test.rs`.
// ============================================================================

pub static SEND_IDENTITIES_LIST_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| {
    ActionDefinition {
        name: "send_identities_list".to_string(),
        description: "Answer ssh_agent_request_identities with the keys this agent \
            holds. This is the ONLY valid answer to that event - send_success does not \
            satisfy it. An empty 'identities' array is valid and means the agent holds \
            no keys, which is what `ssh-add -l` reports as \"no identities\"."
            .to_string(),
        parameters: vec![Parameter {
            name: "identities".to_string(),
            type_hint: "array".to_string(),
            description: "Array of objects, one per key. Each needs \
                'public_key_blob_hex' (the SSH public key blob, hex-encoded: the wire \
                encoding of the key, which for ssh-ed25519 is the string \"ssh-ed25519\" \
                followed by the 32-byte public key, each length-prefixed) and 'comment' \
                (the label `ssh-add -l` prints, e.g. \"deploy-key\"). A blob that is not \
                valid hex, or decodes to zero bytes, fails the whole request rather than \
                sending a broken identity"
                .to_string(),
            required: true,
        }],
        example: json!({"type": "send_identities_list", "identities": [{"public_key_blob_hex": "0000000b7373682d6564323535313900000020e5a1b3", "comment": "deploy-key"}]}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SSH Agent {identities_len} identities")
                .with_debug("SSH Agent send_identities_list: count={identities_len}"),
        ),
    }
});

pub static SEND_SIGN_RESPONSE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| {
    ActionDefinition {
        name: "send_sign_response".to_string(),
        description: "Answer ssh_agent_sign_request with a signature over the data the \
            client sent. NOTE: no private key exists here - you are fabricating the \
            signature bytes, so a real client will fail to verify them against the \
            public key it holds. Useful for honeypots and for exercising the protocol; \
            reply with send_failure to refuse the signature instead."
            .to_string(),
        parameters: vec![Parameter {
            name: "signature_hex".to_string(),
            type_hint: "string".to_string(),
            description: "Hex-encoded SSH signature blob: the signature algorithm name \
                and the signature itself, each length-prefixed, as in the event's \
                'public_key_blob_hex' encoding. Invalid hex, or hex decoding to zero \
                bytes, fails the request instead of sending an empty signature"
                .to_string(),
            required: true,
        }],
        example: json!({"type": "send_sign_response", "signature_hex": "0000000b7373682d65643235353139000000400a1b2c3d"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SSH Agent signature")
                .with_debug("SSH Agent send_sign_response"),
        ),
    }
});

pub static SEND_SUCCESS_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| ActionDefinition {
    name: "send_success".to_string(),
    description: "Send SSH_AGENT_SUCCESS: the operation was accepted. This is the \
        expected answer to ssh_agent_add_identity, ssh_agent_remove_identity, \
        ssh_agent_remove_all_identities, ssh_agent_lock and ssh_agent_unlock. It is \
        NOT a valid answer to a request for identities or a signature."
        .to_string(),
    parameters: vec![],
    example: json!({"type": "send_success"}),
    log_template: Some(
        LogTemplate::new()
            .with_info("-> SSH Agent SUCCESS")
            .with_debug("SSH Agent send_success"),
    ),
});

pub static SEND_FAILURE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| ActionDefinition {
    name: "send_failure".to_string(),
    description: "Send SSH_AGENT_FAILURE: refuse the operation. Use it to deny a \
        signature, reject a key, or refuse an unlock whose passphrase does not \
        match. Every event accepts this. If you return no action at all the server \
        sends nothing and the client blocks, so refuse explicitly."
        .to_string(),
    parameters: vec![],
    example: json!({"type": "send_failure"}),
    log_template: Some(
        LogTemplate::new()
            .with_info("-> SSH Agent FAILURE")
            .with_debug("SSH Agent send_failure"),
    ),
});

pub static CLOSE_CONNECTION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| ActionDefinition {
        name: "close_connection".to_string(),
        description: "Close the agent connection that raised this event, without \
        replying to the request. The client sees the socket drop. Use send_failure \
        instead when you want to refuse an operation but keep the session."
            .to_string(),
        parameters: vec![],
        example: json!({"type": "close_connection"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SSH Agent close connection")
                .with_debug("SSH Agent close_connection"),
        ),
    });

pub static WAIT_FOR_MORE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| ActionDefinition {
    name: "wait_for_more".to_string(),
    description: "Send no reply and wait for the next message. Rarely correct: the \
        agent protocol is strict request/response and a client blocks until it gets \
        an answer, so prefer send_success or send_failure."
        .to_string(),
    parameters: vec![],
    example: json!({"type": "wait_for_more"}),
    log_template: Some(
        LogTemplate::new()
            .with_info("-> SSH Agent wait")
            .with_debug("SSH Agent wait_for_more"),
    ),
});

// Event type constants
pub static SSH_AGENT_CONNECTION_OPENED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_connection_opened",
        "A client connected to the agent socket and has not sent a request yet. Nothing is \
         expected of you here - it exists so you can set up state before the first request. \
         Returning no action is normal; do NOT send send_success, which would put an \
         unrequested reply on the wire and desynchronise the client. Close the connection if \
         you want to refuse the client outright.",
        json!({
            "type": "close_connection"
        }),
    )
    .with_parameter(Parameter {
        name: "connection_id".to_string(),
        type_hint: "string".to_string(),
        description: "Unique connection identifier".to_string(),
        required: true,
    })
    // Only close_connection: replying to a request the client has not made desynchronises it,
    // which the description above spells out. Refusing the connection outright is legitimate.
    .with_actions(vec![CLOSE_CONNECTION_ACTION.clone()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent connection opened")
            .with_debug("SSH Agent conn={connection_id}")
            .with_trace("SSH Agent connection: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_REQUEST_IDENTITIES_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_request_identities",
        "The client asked which keys this agent holds (`ssh-add -l`, or ssh choosing a key for a \
         login). Answer with send_identities_list - an empty list is valid.",
        json!({
            "type": "send_identities_list",
            "identities": []
        }),
    )
    .with_actions(vec![
        SEND_IDENTITIES_LIST_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent REQUEST_IDENTITIES")
            .with_debug("SSH Agent REQUEST_IDENTITIES")
            .with_trace("SSH Agent identities request: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_SIGN_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_sign_request",
        "The client asked the agent to sign a challenge with one of its keys, which is how an \
         SSH login using agent authentication proves key possession. Answer with \
         send_sign_response to sign, or send_failure to refuse. No private key exists here, so \
         any signature you return is fabricated and will not verify.",
        json!({
            "type": "send_sign_response",
            "signature_hex": "0000000b7373682d656432353531390000004000..."
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "key_type".to_string(),
            type_hint: "string".to_string(),
            description: "Algorithm name decoded from the key blob, e.g. \"ssh-ed25519\" or \
                \"ssh-rsa\". Use this to identify which key is being asked for rather than \
                comparing hex blobs. Empty if the blob could not be decoded"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "public_key_blob_hex".to_string(),
            type_hint: "string".to_string(),
            description: "The public key the client wants to sign with, as the hex-encoded SSH \
                wire blob. Compare it against the blobs you returned from send_identities_list \
                to tell which of your keys is meant"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "data_hex".to_string(),
            type_hint: "string".to_string(),
            description: "The challenge to be signed, hex-encoded. It is an SSH session \
                identifier and login request, not human-readable text"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "flags".to_string(),
            type_hint: "integer".to_string(),
            description: "Signature flags from the client: 0 for the key's default algorithm, \
                2 requests rsa-sha2-256 and 4 rsa-sha2-512 for RSA keys"
                .to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        SEND_SIGN_RESPONSE_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent SIGN_REQUEST flags={flags}")
            .with_debug("SSH Agent SIGN_REQUEST flags={flags}")
            .with_trace("SSH Agent sign request: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_ADD_IDENTITY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_add_identity",
        "The client is loading a private key into the agent (`ssh-add`). The private key material \
         is parsed off the wire and discarded - it is never given to you and never stored. \
         Answer send_success to accept the key or send_failure to refuse it; remember accepted \
         keys in memory if you want them to appear in later identity listings.",
        json!({
            "type": "send_success"
        })
    )
    .with_parameters(vec![
        Parameter {
            name: "key_type".to_string(),
            type_hint: "string".to_string(),
            description: "Algorithm name the client sent, e.g. \"ssh-ed25519\" or \"ssh-rsa\""
                .to_string(),
            required: true,
        },
        Parameter {
            name: "public_key_blob_hex".to_string(),
            type_hint: "string".to_string(),
            description: "Hex-encoded public part of the key. NOTE: this is parsed assuming the \
                Ed25519 layout, so for RSA and other multi-field key types this value and \
                'comment' may be wrong or the message may fail to parse entirely"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "comment".to_string(),
            type_hint: "string".to_string(),
            description: "The label the client attached to the key, usually the path or an \
                email address. Subject to the same Ed25519-layout caveat as \
                'public_key_blob_hex'"
                .to_string(),
            required: true,
        },
        Parameter {
            name: "constrained".to_string(),
            type_hint: "boolean".to_string(),
            description: "True if the client sent the key with constraints such as a lifetime \
                (`ssh-add -t`) or confirmation requirement (`ssh-add -c`). The constraints \
                themselves are not parsed and nothing enforces them - honour them yourself if \
                you care"
                .to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        SEND_SUCCESS_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent ADD_IDENTITY {key_type} ({comment})")
            .with_debug("SSH Agent ADD_IDENTITY key_type={key_type} comment={comment} constrained={constrained}")
            .with_trace("SSH Agent add identity: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_REMOVE_IDENTITY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_remove_identity",
        "The client asked the agent to forget one key (`ssh-add -d`). Answer send_success if you \
         drop it from memory, or send_failure if you do not hold it.",
        json!({
            "type": "send_success"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "public_key_blob_hex".to_string(),
        type_hint: "string".to_string(),
        description: "Hex-encoded SSH wire blob of the key to forget. Match it against the \
                blobs you return from send_identities_list"
            .to_string(),
        required: true,
    }])
    .with_actions(vec![
        SEND_SUCCESS_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent REMOVE_IDENTITY")
            .with_debug("SSH Agent REMOVE_IDENTITY")
            .with_trace("SSH Agent remove identity: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_REMOVE_ALL_IDENTITIES_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_remove_all_identities",
        "The client asked the agent to forget every key (`ssh-add -D`). Answer send_success after \
         clearing them from memory.",
        json!({
            "type": "send_success"
        }),
    )
    .with_actions(vec![
        SEND_SUCCESS_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent REMOVE_ALL_IDENTITIES")
            .with_debug("SSH Agent REMOVE_ALL_IDENTITIES")
            .with_trace("SSH Agent remove all: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_LOCK_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_lock",
        "The client asked to lock the agent with a passphrase (`ssh-add -x`). While locked, a \
         real agent hides its identities and refuses to sign until unlocked. Nothing here \
         enforces that: record the passphrase and the locked state in memory and honour it \
         yourself on later events. Answer send_success to accept.",
        json!({
            "type": "send_success"
        }),
    )
    .with_parameter(Parameter {
        name: "passphrase".to_string(),
        type_hint: "string".to_string(),
        description: "The passphrase the client sent, in the clear".to_string(),
        required: true,
    })
    .with_actions(vec![
        SEND_SUCCESS_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent LOCK")
            .with_debug("SSH Agent LOCK")
            .with_trace("SSH Agent lock: {json_pretty(.)}"),
    )
});

pub static SSH_AGENT_UNLOCK_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ssh_agent_unlock",
        "The client asked to unlock the agent (`ssh-add -X`). Compare 'passphrase' with the one \
         from the lock request you remembered: answer send_success if it matches and \
         send_failure if it does not.",
        json!({
            "type": "send_success"
        }),
    )
    .with_parameter(Parameter {
        name: "passphrase".to_string(),
        type_hint: "string".to_string(),
        description: "The passphrase the client sent, in the clear".to_string(),
        required: true,
    })
    .with_actions(vec![
        SEND_SUCCESS_ACTION.clone(),
        SEND_FAILURE_ACTION.clone(),
        CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SSH Agent UNLOCK")
            .with_debug("SSH Agent UNLOCK")
            .with_trace("SSH Agent unlock: {json_pretty(.)}"),
    )
});

/// SSH Agent server protocol implementation
pub struct SshAgentProtocol;

impl SshAgentProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SshAgentProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![ParameterDefinition {
            name: "socket_path".to_string(),
            type_hint: "string".to_string(),
            description: "Path to Unix domain socket (default: ./netget-ssh-agent.sock)"
                .to_string(),
            required: false,
            example: json!("./netget-ssh-agent.sock"),
        }]
    }

    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // None.
        //
        // `modify_instruction` produced an ActionResult::Custom that execute_action_result
        // ignored, so it never changed anything; the common `update_instruction` action does
        // the job properly. `close_connection` declared a `connection_id` parameter that the
        // executor discarded - it always closed the connection that raised the event, never
        // the one named - so it has moved to the sync actions below, without that parameter.
        Vec::new()
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            SEND_IDENTITIES_LIST_ACTION.clone(),
            SEND_SIGN_RESPONSE_ACTION.clone(),
            SEND_SUCCESS_ACTION.clone(),
            SEND_FAILURE_ACTION.clone(),
            CLOSE_CONNECTION_ACTION.clone(),
            WAIT_FOR_MORE_ACTION.clone(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "SSH Agent"
    }

    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            (*SSH_AGENT_CONNECTION_OPENED_EVENT).clone(),
            (*SSH_AGENT_REQUEST_IDENTITIES_EVENT).clone(),
            (*SSH_AGENT_SIGN_REQUEST_EVENT).clone(),
            (*SSH_AGENT_ADD_IDENTITY_EVENT).clone(),
            (*SSH_AGENT_REMOVE_IDENTITY_EVENT).clone(),
            (*SSH_AGENT_REMOVE_ALL_IDENTITIES_EVENT).clone(),
            (*SSH_AGENT_LOCK_EVENT).clone(),
            (*SSH_AGENT_UNLOCK_EVENT).clone(),
        ]
    }

    fn stack_name(&self) -> &'static str {
        "UNIX Socket > SSH Agent"
    }

    fn keywords(&self) -> Vec<&'static str> {
        vec!["ssh-agent", "agent", "key-agent", "ssh keys"]
    }

    fn metadata(&self) -> ProtocolMetadataV2 {
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            // Unix domain socket, so no privileged port is involved.
            .implementation("Custom SSH Agent wire parser over a Unix domain socket")
            .llm_control("Identity listings, signing decisions, key lifecycle, lock/unlock")
            .e2e_testing(
                "tests/server/ssh_agent drives the real binary over its Unix socket with a \
                 hand-written agent client and a mocked model, covering REQUEST_IDENTITIES, \
                 SIGN_REQUEST, ADD_IDENTITY and a pipelined multi-operation session. That \
                 client is written from the wire format inside the test, so it is an \
                 independent reading of the protocol, not an independent implementation - \
                 the same class of evidence as dhcp's in-test RFC 2131 decoder. No \
                 third-party agent client drives it, and none usefully could: see notes.",
            )
            .notes(
                "FAILS CLOSED: an LLM error, an answer with no usable action, and an action \
                 whose fields will not decode all send SSH_AGENT_FAILURE, logged as \
                 decision=fail_closed_*. Saying nothing is not an option - the client blocks \
                 on the read - and it is never SSH_AGENT_SUCCESS. Virtual agent: no private \
                 keys exist, so signatures are fabricated and will not verify, which is why \
                 a real `ssh` cannot complete a login through this agent and why its rating \
                 cannot rest on one. ADD_IDENTITY parsing assumes the Ed25519 key layout. \
                 Lock/unlock and key constraints are reported but never enforced.",
            )
            .build()
    }

    fn description(&self) -> &'static str {
        "SSH Agent protocol server for managing SSH keys and signing operations"
    }

    fn example_prompt(&self) -> &'static str {
        "Start SSH Agent on ./netget-ssh-agent.sock; provide 2 Ed25519 keys (admin-key, deploy-key); sign any requests automatically"
    }

    fn group_name(&self) -> &'static str {
        "Security"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // Deterministic: report an empty key list for every identities request,
        // no LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "ssh_agent_request_identities":
    actions = [{"type": "send_identities_list", "identities": []}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: LLM handles all SSH Agent responses intelligently
            json!({
                "type": "open_server",
                "port": 0,
                "base_stack": "ssh-agent",
                "instruction": "SSH Agent managing keys and signing operations"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_server",
                "port": 0,
                "base_stack": "ssh-agent",
                "event_handlers": [{
                    "event_pattern": "ssh_agent_request_identities",
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
                "port": 0,
                "base_stack": "ssh-agent",
                "event_handlers": [{
                    "event_pattern": "ssh_agent_request_identities",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_identities_list",
                            "identities": []
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for SshAgentProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(async move {
            // For Unix sockets, we need a path not a SocketAddr
            // Extract socket_path from startup_params or use default
            let socket_path = ctx
                .startup_params
                .as_ref()
                .map(|p| p.get_optional_string("socket_path"))
                .transpose()?
                .flatten()
                .unwrap_or_else(|| "./netget-ssh-agent.sock".to_string());

            let socket_path_buf = std::path::PathBuf::from(socket_path);

            use crate::server::ssh_agent::SshAgentServer;
            let _actual_path = SshAgentServer::spawn_with_llm_actions(
                socket_path_buf,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.server_id,
            )
            .await?;

            // Return a dummy SocketAddr since Unix sockets don't have IP addresses
            Ok("127.0.0.1:0".parse().unwrap())
        })
    }

    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        let action_type = action["type"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'type' field in action"))?;

        match action_type {
            "send_identities_list" => {
                let identities = action["identities"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'identities' field"))?;

                Ok(ActionResult::Custom {
                    name: "send_identities_list".to_string(),
                    data: json!({ "identities": identities }),
                })
            }
            "send_sign_response" => {
                let signature_hex = action["signature_hex"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing 'signature_hex' field"))?;

                Ok(ActionResult::Custom {
                    name: "send_sign_response".to_string(),
                    data: json!({ "signature_hex": signature_hex }),
                })
            }
            "send_success" => Ok(ActionResult::Custom {
                name: "send_success".to_string(),
                data: json!({}),
            }),
            "send_failure" => Ok(ActionResult::Custom {
                name: "send_failure".to_string(),
                data: json!({}),
            }),
            "wait_for_more" => Ok(ActionResult::WaitForMore),
            // No `modify_instruction` arm. It is advertised by nothing (see
            // get_async_actions), and the Custom result it used to build had no branch in
            // execute_action_result, so it was accepted and did nothing - the shape a
            // round-trip check passes and a user never gets any use from. Rejecting the name
            // now routes it through the fail-closed rule as decision=fail_closed_action_error.
            "close_connection" => {
                // CloseConnection is a unit variant
                Ok(ActionResult::CloseConnection)
            }
            _ => anyhow::bail!("Unknown action type: {}", action_type),
        }
    }
}
