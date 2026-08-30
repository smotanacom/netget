//! OpenID Connect client protocol actions implementation

use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

/// OpenID Connect client discovered configuration event
pub static OIDC_CLIENT_DISCOVERED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "oidc_discovered",
        "OpenID Connect provider configuration discovered",
        json!({
            "type": "fetch_userinfo"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "issuer".to_string(),
            type_hint: "string".to_string(),
            description: "OpenID Connect provider issuer URL".to_string(),
            required: true,
        },
        Parameter {
            name: "authorization_endpoint".to_string(),
            type_hint: "string".to_string(),
            description: "Authorization endpoint URL".to_string(),
            required: true,
        },
        Parameter {
            name: "token_endpoint".to_string(),
            type_hint: "string".to_string(),
            description: "Token endpoint URL".to_string(),
            required: true,
        },
        Parameter {
            name: "userinfo_endpoint".to_string(),
            type_hint: "string".to_string(),
            description: "UserInfo endpoint URL".to_string(),
            required: false,
        },
        Parameter {
            name: "supported_scopes".to_string(),
            type_hint: "array".to_string(),
            description: "Supported OAuth scopes".to_string(),
            required: false,
        },
    ])
});

/// OpenID Connect client token received event
pub static OIDC_CLIENT_TOKEN_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "oidc_token_received",
        "OAuth/OIDC tokens received from provider",
        json!({
            "type": "fetch_userinfo"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "access_token".to_string(),
            type_hint: "string".to_string(),
            description: "OAuth access token".to_string(),
            required: true,
        },
        Parameter {
            name: "id_token".to_string(),
            type_hint: "string".to_string(),
            description: "OpenID Connect ID token (JWT)".to_string(),
            required: false,
        },
        Parameter {
            name: "refresh_token".to_string(),
            type_hint: "string".to_string(),
            description: "OAuth refresh token".to_string(),
            required: false,
        },
        Parameter {
            name: "expires_in".to_string(),
            type_hint: "number".to_string(),
            description: "Token expiration time in seconds".to_string(),
            required: false,
        },
        Parameter {
            name: "token_type".to_string(),
            type_hint: "string".to_string(),
            description: "Token type (usually 'Bearer')".to_string(),
            required: true,
        },
    ])
});

/// OpenID Connect client userinfo received event
pub static OIDC_CLIENT_USERINFO_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "oidc_userinfo_received",
        "UserInfo data received from OpenID Connect provider",
        json!({
            "type": "refresh_token"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "sub".to_string(),
            type_hint: "string".to_string(),
            description: "Subject identifier (user ID)".to_string(),
            required: true,
        },
        Parameter {
            name: "claims".to_string(),
            type_hint: "object".to_string(),
            description: "User claims (name, email, etc.)".to_string(),
            required: true,
        },
    ])
});

/// OpenID Connect client protocol action handler
pub struct OpenIdConnectClientProtocol;

impl OpenIdConnectClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for OpenIdConnectClientProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
                ParameterDefinition {
                    name: "client_id".to_string(),
                    description: "OAuth2/OIDC client ID".to_string(),
                    type_hint: "string".to_string(),
                    required: true,
                    example: json!("my-application-id"),
                },
                ParameterDefinition {
                    name: "client_secret".to_string(),
                    description: "OAuth2/OIDC client secret (if using confidential client)".to_string(),
                    type_hint: "string".to_string(),
                    required: false,
                    example: json!("secret-key-12345"),
                },
                ParameterDefinition {
                    name: "redirect_uri".to_string(),
                    description: "OAuth2 redirect URI for authorization code flow".to_string(),
                    type_hint: "string".to_string(),
                    required: false,
                    example: json!("http://localhost:8080/callback"),
                },
                ParameterDefinition {
                    name: "scopes".to_string(),
                    description: "OAuth2 scopes to request (space-separated)".to_string(),
                    type_hint: "string".to_string(),
                    required: false,
                    example: json!("openid profile email"),
                },
                ParameterDefinition {
                    name: "flow".to_string(),
                    description: "OAuth2/OIDC flow type (device_code, password, client_credentials, authorization_code)".to_string(),
                    type_hint: "string".to_string(),
                    required: false,
                    example: json!("device_code"),
                },
            ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
                ActionDefinition {
                    name: "discover_configuration".to_string(),
                    description: "Discover OpenID Connect provider configuration from .well-known/openid-configuration".to_string(),
                    parameters: vec![],
                    example: json!({
                        "type": "discover_configuration"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "start_device_flow".to_string(),
                    description: "Start OAuth2 device code flow for CLI authentication".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "scopes".to_string(),
                            type_hint: "string".to_string(),
                            description: "Space-separated OAuth scopes".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "start_device_flow",
                        "scopes": "openid profile email"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "start_authorization_code_flow".to_string(),
                    description: "Start OAuth2 authorization code flow with local HTTP callback server".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "scopes".to_string(),
                            type_hint: "string".to_string(),
                            description: "Space-separated OAuth scopes".to_string(),
                            required: false,
                        },
                        Parameter {
                            name: "port".to_string(),
                            type_hint: "number".to_string(),
                            description: "Local callback server port (default: 8080)".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "start_authorization_code_flow",
                        "scopes": "openid profile email",
                        "port": 8080
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "exchange_password".to_string(),
                    description: "Exchange username/password for tokens (Resource Owner Password Credentials flow)".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "username".to_string(),
                            type_hint: "string".to_string(),
                            description: "Username".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "password".to_string(),
                            type_hint: "string".to_string(),
                            description: "Password".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "scopes".to_string(),
                            type_hint: "string".to_string(),
                            description: "Space-separated OAuth scopes".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "exchange_password",
                        "username": "user@example.com",
                        "password": "secret123",
                        "scopes": "openid profile"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "exchange_client_credentials".to_string(),
                    description: "Exchange client credentials for access token (machine-to-machine)".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "scopes".to_string(),
                            type_hint: "string".to_string(),
                            description: "Space-separated OAuth scopes".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "exchange_client_credentials",
                        "scopes": "api.read api.write"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "refresh_token".to_string(),
                    description: "Refresh access token using refresh token".to_string(),
                    parameters: vec![],
                    example: json!({
                        "type": "refresh_token"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "fetch_userinfo".to_string(),
                    description: "Fetch user information from UserInfo endpoint using access token".to_string(),
                    parameters: vec![],
                    example: json!({
                        "type": "fetch_userinfo"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "disconnect".to_string(),
                    description: "Disconnect from the OpenID Connect provider".to_string(),
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
                name: "fetch_userinfo".to_string(),
                description: "Fetch user information after receiving tokens".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "fetch_userinfo"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "refresh_token".to_string(),
                description: "Refresh access token in response to expiration".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "refresh_token"
                }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "OpenIDConnect"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "oidc_discovered",
                "Triggered when OIDC provider configuration is discovered",
                json!({"type": "placeholder", "event_id": "oidc_discovered"}),
            ),
            EventType::new(
                "oidc_token_received",
                "Triggered when OAuth/OIDC tokens are received",
                json!({"type": "placeholder", "event_id": "oidc_token_received"}),
            ),
            EventType::new(
                "oidc_userinfo_received",
                "Triggered when UserInfo data is received",
                json!({"type": "placeholder", "event_id": "oidc_userinfo_received"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP>OIDC"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["openidconnect", "oidc", "openid connect", "oauth2 client"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
                .state(DevelopmentState::Experimental)
                .implementation("openidconnect crate with full OAuth2/OIDC flows")
                .llm_control("Full control over authentication flows (device code, password, client credentials)")
                .e2e_testing("Local OIDC provider or public test providers")
                .build()
    }
    fn description(&self) -> &'static str {
        "OpenID Connect client for OAuth2/OIDC authentication"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to OpenID Connect provider at https://accounts.google.com and authenticate"
    }
    fn group_name(&self) -> &'static str {
        "Authentication"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls OIDC authentication flow
            json!({
                "type": "open_client",
                "remote_addr": "https://accounts.google.com",
                "base_stack": "openidconnect",
                "instruction": "Discover configuration and start device code authentication",
                "startup_params": {
                    "client_id": "my-application-id",
                    "scopes": "openid profile email"
                }
            }),
            // Script mode: Code-based OIDC token handling
            json!({
                "type": "open_client",
                "remote_addr": "https://accounts.google.com",
                "base_stack": "openidconnect",
                "startup_params": {
                    "client_id": "my-application-id"
                },
                "event_handlers": [{
                    "event_pattern": "oidc_token_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<oidc_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed OIDC discovery and userinfo flow
            json!({
                "type": "open_client",
                "remote_addr": "https://accounts.google.com",
                "base_stack": "openidconnect",
                "startup_params": {
                    "client_id": "my-application-id",
                    "scopes": "openid profile email"
                },
                "event_handlers": [
                    {
                        "event_pattern": "oidc_discovered",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "start_device_flow",
                                "scopes": "openid profile email"
                            }]
                        }
                    },
                    {
                        "event_pattern": "oidc_token_received",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "fetch_userinfo"
                            }]
                        }
                    },
                    {
                        "event_pattern": "oidc_userinfo_received",
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
impl Client for OpenIdConnectClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::openidconnect::OpenIdConnectClient;
            OpenIdConnectClient::connect_with_llm_actions(
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
            "discover_configuration" => Ok(ClientActionResult::Custom {
                name: "oidc_discover".to_string(),
                data: json!({}),
            }),
            "start_device_flow" => {
                let scopes = action
                    .get("scopes")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                Ok(ClientActionResult::Custom {
                    name: "oidc_device_flow".to_string(),
                    data: json!({
                        "scopes": scopes,
                    }),
                })
            }
            "start_authorization_code_flow" => {
                let scopes = action
                    .get("scopes")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let port = action.get("port").and_then(|v| v.as_u64()).unwrap_or(8080) as u16;

                Ok(ClientActionResult::Custom {
                    name: "oidc_authorization_code".to_string(),
                    data: json!({
                        "scopes": scopes,
                        "port": port,
                    }),
                })
            }
            "exchange_password" => {
                let username = action
                    .get("username")
                    .and_then(|v| v.as_str())
                    .context("Missing 'username' field")?
                    .to_string();

                let password = action
                    .get("password")
                    .and_then(|v| v.as_str())
                    .context("Missing 'password' field")?
                    .to_string();

                let scopes = action
                    .get("scopes")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                Ok(ClientActionResult::Custom {
                    name: "oidc_password_flow".to_string(),
                    data: json!({
                        "username": username,
                        "password": password,
                        "scopes": scopes,
                    }),
                })
            }
            "exchange_client_credentials" => {
                let scopes = action
                    .get("scopes")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                Ok(ClientActionResult::Custom {
                    name: "oidc_client_credentials".to_string(),
                    data: json!({
                        "scopes": scopes,
                    }),
                })
            }
            "refresh_token" => Ok(ClientActionResult::Custom {
                name: "oidc_refresh_token".to_string(),
                data: json!({}),
            }),
            "fetch_userinfo" => Ok(ClientActionResult::Custom {
                name: "oidc_fetch_userinfo".to_string(),
                data: json!({}),
            }),
            "disconnect" => Ok(ClientActionResult::Disconnect),
            _ => Err(anyhow::anyhow!(
                "Unknown OpenID Connect client action: {}",
                action_type
            )),
        }
    }
}
