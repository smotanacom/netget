//! SAML client protocol actions implementation

use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

/// SAML client connected event
pub static SAML_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "saml_connected",
        "SAML client initialized and ready to authenticate",
        json!({
            "type": "parse_assertion",
            "response_xml": "<samlp:Response...>"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "idp_url".to_string(),
        type_hint: "string".to_string(),
        description: "Identity Provider URL".to_string(),
        required: true,
    }])
});

/// SAML authentication response received event
pub static SAML_CLIENT_RESPONSE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "saml_response_received",
        "SAML authentication response received from IdP",
        json!({
            "type": "parse_assertion",
            "response_xml": "<samlp:Response...>"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "success".to_string(),
            type_hint: "boolean".to_string(),
            // Not "whether authentication was successful". NetGet checks no signature,
            // issuer, audience or expiry, so this says only what the XML claims.
            description: "True only when the response's top-level <StatusCode> is exactly \
                          urn:oasis:names:tc:SAML:2.0:status:Success. NOTHING IS VERIFIED: no \
                          signature, issuer, audience or expiry is checked, so a forged \
                          response reports true just as readily as a genuine one."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "status_code".to_string(),
            type_hint: "string".to_string(),
            description: "The response's top-level SAML status URI, verbatim - a nested \
                          second-level code never appears here."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "assertion".to_string(),
            type_hint: "object".to_string(),
            description: "{subject, status_code} when success is true, otherwise absent. \
                          `subject` is the first <NameID> in the document."
                .to_string(),
            required: false,
        },
        Parameter {
            name: "attributes".to_string(),
            type_hint: "object".to_string(),
            description: "User attributes from IdP".to_string(),
            required: false,
        },
    ])
});

/// SAML client protocol action handler
pub struct SamlClientProtocol;

impl SamlClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SamlClientProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "entity_id".to_string(),
                description: "Service Provider entity ID".to_string(),
                type_hint: "string".to_string(),
                required: false,
                example: json!("https://example.com/saml/sp"),
            },
            ParameterDefinition {
                name: "acs_url".to_string(),
                description: "Assertion Consumer Service URL".to_string(),
                type_hint: "string".to_string(),
                required: false,
                example: json!("https://example.com/saml/acs"),
            },
            ParameterDefinition {
                name: "binding".to_string(),
                description: "SAML binding type (redirect or post)".to_string(),
                type_hint: "string".to_string(),
                required: false,
                example: json!("redirect"),
            },
        ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "initiate_sso".to_string(),
                description: "Initiate SAML Single Sign-On with IdP".to_string(),
                parameters: vec![
                    Parameter {
                        name: "relay_state".to_string(),
                        type_hint: "string".to_string(),
                        description: "Optional relay state to preserve across authentication"
                            .to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "force_authn".to_string(),
                        type_hint: "boolean".to_string(),
                        description: "Force re-authentication at IdP".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "initiate_sso",
                    "relay_state": "/protected/resource",
                    "force_authn": false
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "validate_assertion".to_string(),
                description: "Read a base64-encoded SAMLResponse from the IdP and report its status, \
                              subject and attributes. Despite the name this VALIDATES NOTHING: \
                              no signature, issuer, audience, expiry or replay check is \
                              performed, so a forged response is read exactly like a genuine \
                              one. Judge the reported fields yourself."
                    .to_string(),
                parameters: vec![Parameter {
                    name: "saml_response".to_string(),
                    type_hint: "string".to_string(),
                    description: "Base64-encoded SAML response".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "validate_assertion",
                    "saml_response": "PHNhbWxwOlJlc3BvbnNlLi4uPg=="
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from SAML IdP".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "disconnect"
                }),
                log_template: None,
            },
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![ActionDefinition {
            name: "parse_assertion".to_string(),
            description: "Read a raw (not base64-encoded) SAMLResponse XML document and report its \
                          status, subject and attributes. Nothing is verified - see \
                          validate_assertion."
                .to_string(),
            parameters: vec![Parameter {
                name: "response_xml".to_string(),
                type_hint: "string".to_string(),
                description: "SAML response XML".to_string(),
                required: true,
            }],
            example: json!({
                "type": "parse_assertion",
                "response_xml": "<samlp:Response...>"
            }),
            log_template: Some(
                LogTemplate::new()
                    .with_info("-> SAML parse assertion")
                    .with_debug("SAML parse_assertion"),
            ),
        }]
    }
    fn protocol_name(&self) -> &'static str {
        "SAML"
    }
    /// The events this client raises — the real ones.
    ///
    /// This used to build two fresh `EventType`s here whose example action was
    /// `{"type": "placeholder", "event_id": "saml_connected"}` and which declared no
    /// parameters at all, while `mod.rs` raised the `LazyLock` statics above. Nothing failed:
    /// `get_protocol_docs` and the script-template prompt read *this* list, so anyone reading
    /// the documentation was shown a placeholder action that does not exist and told nothing
    /// about `success`, `status_code`, `assertion` or `attributes`. Return the statics.
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            SAML_CLIENT_CONNECTED_EVENT.clone(),
            SAML_CLIENT_RESPONSE_RECEIVED_EVENT.clone(),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP>SAML"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "saml",
            "saml client",
            "connect to saml",
            "sso",
            "single sign-on",
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation(
                "Hand-written SAML 2.0 SP: builds an AuthnRequest and encodes it for the \
                 HTTP-Redirect or HTTP-POST binding, and reads a SAMLResponse with quick-xml. \
                 NetGet sends nothing itself - the browser carries both messages.",
            )
            // "assertion validation" is what this used to claim and it is the one thing that
            // does not happen: no signature, issuer, audience, expiry or replay check exists,
            // and there is no key here to check a signature against.
            .llm_control(
                "When to start SSO, with what RelayState and ForceAuthn, and what to make of \
                 the status, subject and attributes read out of a response. Nothing is verified.",
            )
            .e2e_testing(
                "Mocked model plus in-test SAMLResponse documents \
                 (tests/client/saml/response_parsing_test.rs); no third-party SAML IdP.",
            )
            .notes(
                "Accepts forged and unsigned assertions: nothing checks <ds:Signature>, Issuer, \
                 AudienceRestriction, NotOnOrAfter, InResponseTo or assertion-ID replay. The \
                 `success` it reports means only that the response's top-level StatusCode says \
                 Success. Simulator and honeypot only, never an access-control boundary.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "SAML 2.0 Service Provider client (reads responses; no signature is verified)"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to SAML IdP at https://idp.example.com/saml and authenticate user"
    }
    fn group_name(&self) -> &'static str {
        "Authentication"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM handles SAML SSO flow
            json!({
                "type": "open_client",
                "remote_addr": "https://idp.example.com/saml/sso",
                "base_stack": "saml",
                "instruction": "Initiate SAML SSO authentication with the Identity Provider",
                "startup_params": {
                    "entity_id": "https://myapp.example.com/saml/sp",
                    "acs_url": "https://myapp.example.com/saml/acs"
                }
            }),
            // Script mode: Code-based SAML handling
            json!({
                "type": "open_client",
                "remote_addr": "https://idp.example.com/saml/sso",
                "base_stack": "saml",
                "startup_params": {
                    "entity_id": "https://myapp.example.com/saml/sp"
                },
                "event_handlers": [{
                    "event_pattern": "saml_connected",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<saml_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed SAML action
            json!({
                "type": "open_client",
                "remote_addr": "https://idp.example.com/saml/sso",
                "base_stack": "saml",
                "startup_params": {
                    "entity_id": "https://myapp.example.com/saml/sp"
                },
                "event_handlers": [{
                    "event_pattern": "saml_connected",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "initiate_sso",
                            "relay_state": "/dashboard",
                            "force_authn": false
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for SamlClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::saml::SamlClient;
            SamlClient::connect_with_llm_actions(
                ctx.remote_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.client_id,
                // `entity_id`, `acs_url` and `binding` are declared startup parameters, and
                // this call used to drop `ctx.startup_params` on the floor — so all three
                // were advertised knobs that did nothing, and `connect_with_llm_actions`
                // seeded its own defaults under a comment saying they "can be overridden by
                // startup params". `pop3:use_tls` set the precedent: the parameter list is
                // per-protocol, so passing the params through is the whole fix.
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
            "initiate_sso" => {
                let relay_state = action
                    .get("relay_state")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let force_authn = action
                    .get("force_authn")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                Ok(ClientActionResult::Custom {
                    name: "saml_initiate_sso".to_string(),
                    data: json!({
                        "relay_state": relay_state,
                        "force_authn": force_authn,
                    }),
                })
            }
            "validate_assertion" => {
                let saml_response = action
                    .get("saml_response")
                    .and_then(|v| v.as_str())
                    .context("Missing 'saml_response' field")?
                    .to_string();

                Ok(ClientActionResult::Custom {
                    name: "saml_validate_assertion".to_string(),
                    data: json!({
                        "saml_response": saml_response,
                    }),
                })
            }
            "parse_assertion" => {
                let response_xml = action
                    .get("response_xml")
                    .and_then(|v| v.as_str())
                    .context("Missing 'response_xml' field")?
                    .to_string();

                Ok(ClientActionResult::Custom {
                    name: "saml_parse_assertion".to_string(),
                    data: json!({
                        "response_xml": response_xml,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            _ => Err(anyhow::anyhow!(
                "Unknown SAML client action: {}",
                action_type
            )),
        }
    }
}
