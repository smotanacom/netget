//! LDAP client protocol actions implementation

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

/// LDAP client connected event
pub static LDAP_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ldap_connected",
        "LDAP client successfully connected to server",
        json!({"type": "bind", "dn": "cn=admin,dc=example,dc=com", "password": "secret"}),
    )
    .with_parameters(vec![Parameter {
        name: "remote_addr".to_string(),
        type_hint: "string".to_string(),
        description: "LDAP server address".to_string(),
        required: true,
    }])
});

/// LDAP client bind response event
pub static LDAP_CLIENT_BIND_RESPONSE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ldap_bind_response",
        "LDAP bind (authentication) response received",
        json!({"type": "search", "base_dn": "dc=example,dc=com", "filter": "(objectClass=person)"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "success".to_string(),
            type_hint: "boolean".to_string(),
            description: "Whether bind was successful".to_string(),
            required: true,
        },
        Parameter {
            name: "message".to_string(),
            type_hint: "string".to_string(),
            description: "Response message or error".to_string(),
            required: false,
        },
    ])
});

/// LDAP client search results event
pub static LDAP_CLIENT_SEARCH_RESULTS_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ldap_search_results",
        "LDAP search results received",
        json!({"type": "wait_for_more"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "entries".to_string(),
            type_hint: "array".to_string(),
            description: "Array of LDAP entries with DN and attributes".to_string(),
            required: true,
        },
        Parameter {
            name: "count".to_string(),
            type_hint: "number".to_string(),
            description: "Number of entries returned".to_string(),
            required: true,
        },
    ])
});

/// LDAP client modify response event
pub static LDAP_CLIENT_MODIFY_RESPONSE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ldap_modify_response",
        "LDAP modify operation response received",
        json!({"type": "wait_for_more"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "success".to_string(),
            type_hint: "boolean".to_string(),
            description: "Whether modify was successful".to_string(),
            required: true,
        },
        Parameter {
            name: "message".to_string(),
            type_hint: "string".to_string(),
            description: "Response message or error".to_string(),
            required: false,
        },
    ])
});

/// LDAP client protocol action handler
pub struct LdapClientProtocol;

impl LdapClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for LdapClientProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
                ActionDefinition {
                    name: "bind".to_string(),
                    description: "Authenticate to LDAP server with credentials".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "dn".to_string(),
                            type_hint: "string".to_string(),
                            description: "Distinguished Name (DN) to bind as (e.g., 'cn=admin,dc=example,dc=com')".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "password".to_string(),
                            type_hint: "string".to_string(),
                            description: "Password for authentication".to_string(),
                            required: true,
                        },
                    ],
                    example: json!({
                        "type": "bind",
                        "dn": "cn=admin,dc=example,dc=com",
                        "password": "secret"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "search".to_string(),
                    description: "Search for LDAP entries".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "base_dn".to_string(),
                            type_hint: "string".to_string(),
                            description: "Base DN to start search from (e.g., 'dc=example,dc=com')".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "filter".to_string(),
                            type_hint: "string".to_string(),
                            description: "LDAP search filter (e.g., '(objectClass=person)', '(cn=john)')".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "attributes".to_string(),
                            type_hint: "array".to_string(),
                            description: "Attributes to retrieve (e.g., ['cn', 'mail', 'uid'])".to_string(),
                            required: false,
                        },
                        Parameter {
                            name: "scope".to_string(),
                            type_hint: "string".to_string(),
                            description: "Search scope: 'base', 'one', or 'subtree' (default: 'subtree')".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "search",
                        "base_dn": "dc=example,dc=com",
                        "filter": "(objectClass=person)",
                        "attributes": ["cn", "mail", "uid"],
                        "scope": "subtree"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "add".to_string(),
                    description: "Add a new LDAP entry".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "dn".to_string(),
                            type_hint: "string".to_string(),
                            description: "Distinguished Name of the new entry".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "attributes".to_string(),
                            type_hint: "object".to_string(),
                            description: "Object with attribute names as keys and arrays of values".to_string(),
                            required: true,
                        },
                    ],
                    example: json!({
                        "type": "add",
                        "dn": "cn=newuser,dc=example,dc=com",
                        "attributes": {
                            "objectClass": ["person", "inetOrgPerson"],
                            "cn": ["newuser"],
                            "sn": ["User"],
                            "mail": ["newuser@example.com"]
                        }
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "modify".to_string(),
                    description: "Modify an existing LDAP entry".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "dn".to_string(),
                            type_hint: "string".to_string(),
                            description: "Distinguished Name of the entry to modify".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "operation".to_string(),
                            type_hint: "string".to_string(),
                            description: "Modification operation: 'add', 'delete', or 'replace'".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "attribute".to_string(),
                            type_hint: "string".to_string(),
                            description: "Attribute name to modify".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "values".to_string(),
                            type_hint: "array".to_string(),
                            description: "Array of values for the attribute".to_string(),
                            required: true,
                        },
                    ],
                    example: json!({
                        "type": "modify",
                        "dn": "cn=user,dc=example,dc=com",
                        "operation": "replace",
                        "attribute": "mail",
                        "values": ["newemail@example.com"]
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "delete".to_string(),
                    description: "Delete an LDAP entry".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "dn".to_string(),
                            type_hint: "string".to_string(),
                            description: "Distinguished Name of the entry to delete".to_string(),
                            required: true,
                        },
                    ],
                    example: json!({
                        "type": "delete",
                        "dn": "cn=olduser,dc=example,dc=com"
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "disconnect".to_string(),
                    description: "Disconnect from the LDAP server".to_string(),
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
            name: "wait_for_more".to_string(),
            description: "Wait for more data from LDAP server".to_string(),
            parameters: vec![],
            example: json!({
                "type": "wait_for_more"
            }),
            log_template: None,
        }]
    }
    fn protocol_name(&self) -> &'static str {
        "LDAP"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "ldap_connected",
                "Triggered when LDAP client connects to server",
                json!({"type": "wait_for_more"}),
            ),
            EventType::new(
                "ldap_bind_response",
                "Triggered when LDAP bind response is received",
                json!({"type": "wait_for_more"}),
            ),
            EventType::new(
                "ldap_search_results",
                "Triggered when LDAP search results are received",
                json!({"type": "wait_for_more"}),
            ),
            EventType::new(
                "ldap_modify_response",
                "Triggered when LDAP modify response is received",
                json!({"type": "wait_for_more"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>LDAP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["ldap", "ldap client", "connect to ldap", "directory"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("ldap3 crate with full directory operations")
            .llm_control("Full control over bind, search, add, modify, delete operations")
            .e2e_testing(
                "tests/client/ldap/command_channel_test.rs runs. The three tests in \
                 tests/client/ldap/e2e_test.rs need a Docker OpenLDAP container and are all \
                 #[ignore]d, so nothing in the default suite drives this client against a \
                 real directory.",
            )
            .notes(
                "ldap3's synchronous LdapConn owns the socket, so every verb runs on \
                 spawn_blocking and NetGet never sees the wire bytes. Bind credentials come \
                 from the model or the operator; no key material or system account is read.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "LDAP client for directory services operations"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to LDAP at localhost:389, bind as cn=admin,dc=example,dc=com and search for all users"
    }
    fn group_name(&self) -> &'static str {
        "Directory"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls LDAP operations
            json!({
                "type": "open_client",
                "remote_addr": "localhost:389",
                "base_stack": "ldap",
                "instruction": "Bind as cn=admin,dc=example,dc=com and search for all users"
            }),
            // Script mode: Code-based directory operations
            json!({
                "type": "open_client",
                "remote_addr": "localhost:389",
                "base_stack": "ldap",
                "event_handlers": [{
                    "event_pattern": "ldap_search_results",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<ldap_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed bind and search
            json!({
                "type": "open_client",
                "remote_addr": "localhost:389",
                "base_stack": "ldap",
                "event_handlers": [
                    {
                        "event_pattern": "ldap_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "bind",
                                "dn": "cn=admin,dc=example,dc=com",
                                "password": "secret"
                            }]
                        }
                    },
                    {
                        "event_pattern": "ldap_bind_response",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "search",
                                "base_dn": "dc=example,dc=com",
                                "filter": "(objectClass=person)"
                            }]
                        }
                    },
                    {
                        "event_pattern": "ldap_search_results",
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
impl Client for LdapClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::ldap::LdapClient;
            LdapClient::connect_with_llm_actions(
                ctx.remote_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.client_id,
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
            "bind" => {
                let dn = action
                    .get("dn")
                    .and_then(|v| v.as_str())
                    .context("Missing 'dn' field")?
                    .to_string();
                let password = action
                    .get("password")
                    .and_then(|v| v.as_str())
                    .context("Missing 'password' field")?
                    .to_string();

                Ok(ClientActionResult::Custom {
                    name: "ldap_bind".to_string(),
                    data: json!({
                        "dn": dn,
                        "password": password,
                    }),
                })
            }
            "search" => {
                let base_dn = action
                    .get("base_dn")
                    .and_then(|v| v.as_str())
                    .context("Missing 'base_dn' field")?
                    .to_string();
                let filter = action
                    .get("filter")
                    .and_then(|v| v.as_str())
                    .context("Missing 'filter' field")?
                    .to_string();
                let attributes = action
                    .get("attributes")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                let scope = action
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or("subtree")
                    .to_string();

                Ok(ClientActionResult::Custom {
                    name: "ldap_search".to_string(),
                    data: json!({
                        "base_dn": base_dn,
                        "filter": filter,
                        "attributes": attributes,
                        "scope": scope,
                    }),
                })
            }
            "add" => {
                let dn = action
                    .get("dn")
                    .and_then(|v| v.as_str())
                    .context("Missing 'dn' field")?
                    .to_string();
                let attributes = action
                    .get("attributes")
                    .context("Missing 'attributes' field")?
                    .clone();

                Ok(ClientActionResult::Custom {
                    name: "ldap_add".to_string(),
                    data: json!({
                        "dn": dn,
                        "attributes": attributes,
                    }),
                })
            }
            "modify" => {
                let dn = action
                    .get("dn")
                    .and_then(|v| v.as_str())
                    .context("Missing 'dn' field")?
                    .to_string();
                let operation = action
                    .get("operation")
                    .and_then(|v| v.as_str())
                    .context("Missing 'operation' field")?
                    .to_string();
                let attribute = action
                    .get("attribute")
                    .and_then(|v| v.as_str())
                    .context("Missing 'attribute' field")?
                    .to_string();
                let values = action
                    .get("values")
                    .and_then(|v| v.as_array())
                    .context("Missing 'values' field")?
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>();

                Ok(ClientActionResult::Custom {
                    name: "ldap_modify".to_string(),
                    data: json!({
                        "dn": dn,
                        "operation": operation,
                        "attribute": attribute,
                        "values": values,
                    }),
                })
            }
            "delete" => {
                let dn = action
                    .get("dn")
                    .and_then(|v| v.as_str())
                    .context("Missing 'dn' field")?
                    .to_string();

                Ok(ClientActionResult::Custom {
                    name: "ldap_delete".to_string(),
                    data: json!({
                        "dn": dn,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown LDAP client action: {}",
                action_type
            )),
        }
    }
}
