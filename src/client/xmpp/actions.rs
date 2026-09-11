//! XMPP client protocol actions implementation

use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, Parameter,
};
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

/// XMPP client connected event
pub static XMPP_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "xmpp_connected",
        "XMPP client successfully connected and authenticated",
        json!({
            "type": "send_message",
            "to": "friend@example.com",
            "body": "Hello!"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "jid".to_string(),
        type_hint: "string".to_string(),
        description: "The JID (Jabber ID) of the connected client".to_string(),
        required: true,
    }])
});

/// XMPP client message received event
pub static XMPP_CLIENT_MESSAGE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "xmpp_message_received",
        "XMPP message received from another user",
        json!({
            "type": "send_message",
            "to": "friend@example.com",
            "body": "Thanks for your message!"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "from".to_string(),
            type_hint: "string".to_string(),
            description: "JID of the message sender".to_string(),
            required: true,
        },
        Parameter {
            name: "to".to_string(),
            type_hint: "string".to_string(),
            description: "JID of the message recipient".to_string(),
            required: true,
        },
        Parameter {
            name: "body".to_string(),
            type_hint: "string".to_string(),
            description: "Message body text".to_string(),
            required: true,
        },
        Parameter {
            name: "message_type".to_string(),
            type_hint: "string".to_string(),
            description: "Type of message (Chat, Groupchat, etc.)".to_string(),
            required: true,
        },
    ])
});

/// XMPP client presence received event
pub static XMPP_CLIENT_PRESENCE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "xmpp_presence_received",
        "XMPP presence update received from a contact",
        json!({
            "type": "wait_for_more"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "from".to_string(),
            type_hint: "string".to_string(),
            description: "JID of the contact".to_string(),
            required: true,
        },
        Parameter {
            name: "presence_type".to_string(),
            type_hint: "string".to_string(),
            description: "Type of presence (Available, Unavailable, etc.)".to_string(),
            required: true,
        },
        Parameter {
            name: "show".to_string(),
            type_hint: "string".to_string(),
            description: "Availability indicator (away, chat, dnd, xa)".to_string(),
            required: false,
        },
        Parameter {
            name: "status".to_string(),
            type_hint: "string".to_string(),
            description: "Status message".to_string(),
            required: false,
        },
    ])
});

/// XMPP client protocol action handler
pub struct XmppClientProtocol;

impl XmppClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for XmppClientProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        vec![
            crate::llm::actions::ParameterDefinition {
                name: "jid".to_string(),
                type_hint: "string".to_string(),
                description: "JID (Jabber ID) to connect as, e.g. 'alice@example.com'. Supply \
                              this together with 'password'. Without both, the only remaining \
                              way to give credentials is to pack them into remote_addr as \
                              'user@domain@password'."
                    .to_string(),
                required: false,
                example: serde_json::json!("alice@example.com"),
            },
            crate::llm::actions::ParameterDefinition {
                name: "password".to_string(),
                type_hint: "string".to_string(),
                description: "Password for SASL authentication. Supply this together with \
                              'jid'."
                    .to_string(),
                required: false,
                example: serde_json::json!("secret"),
            },
        ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "send_message".to_string(),
                description: "Send a message to a JID".to_string(),
                parameters: vec![
                    Parameter {
                        name: "to".to_string(),
                        type_hint: "string".to_string(),
                        description: "JID of the recipient".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "body".to_string(),
                        type_hint: "string".to_string(),
                        description: "Message body text".to_string(),
                        required: true,
                    },
                ],
                example: json!({
                    "type": "send_message",
                    "to": "friend@example.com",
                    "body": "Hello from NetGet!"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "send_presence".to_string(),
                description: "Send presence update".to_string(),
                parameters: vec![
                    Parameter {
                        name: "show".to_string(),
                        type_hint: "string".to_string(),
                        description: "Availability (away, chat, dnd, xa)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "status".to_string(),
                        type_hint: "string".to_string(),
                        description: "Status message".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "send_presence",
                    "show": "away",
                    "status": "Out for lunch"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from the XMPP server".to_string(),
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
                name: "send_message".to_string(),
                description: "Send a message in response to a received message or event"
                    .to_string(),
                parameters: vec![
                    Parameter {
                        name: "to".to_string(),
                        type_hint: "string".to_string(),
                        description: "JID of the recipient".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "body".to_string(),
                        type_hint: "string".to_string(),
                        description: "Message body text".to_string(),
                        required: true,
                    },
                ],
                example: json!({
                    "type": "send_message",
                    "to": "friend@example.com",
                    "body": "Thanks for your message!"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "wait_for_more".to_string(),
                description: "Wait for more events before responding".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "wait_for_more"
                }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "XMPP"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        // The three statics, not fresh copies. This used to build a second, parameterless set
        // with the same ids whose example action was literally `{"type": "placeholder"}` - an
        // action no executor has - so everything reading `get_event_types()` (the model's
        // docs, the dashboard's routing editor) was shown a suggestion that cannot run and told
        // the events carry no fields, while the events actually raised carry up to four.
        vec![
            XMPP_CLIENT_CONNECTED_EVENT.clone(),
            XMPP_CLIENT_MESSAGE_RECEIVED_EVENT.clone(),
            XMPP_CLIENT_PRESENCE_RECEIVED_EVENT.clone(),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>TLS>XMPP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "xmpp",
            "xmpp client",
            "jabber",
            "connect to xmpp",
            "connect to jabber",
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation(
                "tokio-xmpp 5.0 over StartTLS, aimed at `remote_addr` explicitly \
                 (`DnsConfig::Addr`/`NoSrv`). It does not do an SRV lookup on the JID's \
                 domain: a client that cannot reach the host the operator named must fail \
                 rather than offer the password somewhere else. `connect()` waits for \
                 `Event::Online` before reporting success. IQ stanzas are received and \
                 ignored; there is no roster, no MUC and no TLS configuration.",
            )
            .llm_control("Send messages, presence updates, respond to incoming stanzas")
            .e2e_testing(
                "tests/client/xmpp/command_channel_test.rs (not ignored) covers command-channel \
                 registration, action execution, the access-log entry and disconnect. It does \
                 **not** cover a stanza reaching a peer: no XMPP server this suite can start \
                 completes tokio-xmpp's STARTTLS/SASL negotiation, and NetGet's own XMPP server \
                 implements neither. The three tests in e2e_test.rs that would cover it are \
                 `#[ignore]`d and need a real prosody/ejabberd, so they are not evidence of \
                 anything - which is why this stays Experimental.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "XMPP/Jabber client for instant messaging"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to XMPP at xmpp.example.com:5222 as alice@example.com and respond to \
         incoming messages"
    }
    fn group_name(&self) -> &'static str {
        "Application"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls XMPP messaging.
            //
            // `remote_addr` is the host to connect to and `jid`/`password` are startup
            // parameters. Every example here used to give `remote_addr` alone, which cannot
            // work - there were no credentials in it and the parameters were never read - so
            // each one failed at connect with "Invalid XMPP address format".
            json!({
                "type": "open_client",
                "remote_addr": "xmpp.example.com:5222",
                "base_stack": "xmpp",
                "startup_params": {
                    "jid": "alice@example.com",
                    "password": "secret"
                },
                "instruction": "Send presence and auto-reply to all incoming messages"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_client",
                "remote_addr": "xmpp.example.com:5222",
                "base_stack": "xmpp",
                "startup_params": {
                    "jid": "alice@example.com",
                    "password": "secret"
                },
                "event_handlers": [{
                    "event_pattern": "xmpp_message_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<xmpp_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed XMPP presence on connect
            json!({
                "type": "open_client",
                "remote_addr": "xmpp.example.com:5222",
                "base_stack": "xmpp",
                "startup_params": {
                    "jid": "alice@example.com",
                    "password": "secret"
                },
                "event_handlers": [
                    {
                        "event_pattern": "xmpp_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "send_presence",
                                "show": "chat",
                                "status": "Available"
                            }]
                        }
                    },
                    {
                        "event_pattern": "xmpp_message_received",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "wait_for_more"
                            }]
                        }
                    }
                ]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for XmppClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::xmpp::XmppClientConnection;
            XmppClientConnection::connect_with_llm_actions(
                ctx.remote_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.client_id,
                // Never passed until now, which is why the declared `jid` and `password`
                // parameters were read by nothing and every documented startup example failed
                // at connect.
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
            "send_message" => {
                let to = action
                    .get("to")
                    .and_then(|v| v.as_str())
                    .context("Missing 'to' field")?;

                let body = action
                    .get("body")
                    .and_then(|v| v.as_str())
                    .context("Missing 'body' field")?;

                Ok(ClientActionResult::Custom {
                    name: "send_message".to_string(),
                    data: json!({
                        "to": to,
                        "body": body,
                    }),
                })
            }
            "send_presence" => {
                let show = action.get("show").and_then(|v| v.as_str());
                let status = action.get("status").and_then(|v| v.as_str());

                Ok(ClientActionResult::Custom {
                    name: "send_presence".to_string(),
                    data: json!({
                        "show": show,
                        "status": status,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown XMPP client action: {}",
                action_type
            )),
        }
    }
}
