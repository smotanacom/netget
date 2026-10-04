use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, ParameterDefinition,
};
use crate::protocol::{ConnectContext, EventType};
use crate::server::graphql::actions::{action, parameter};
use crate::server::graphql::engine;
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;

#[derive(Default)]
pub struct GraphqlClientProtocol;
impl GraphqlClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

fn query_action() -> ActionDefinition {
    action(
        "graphql_query",
        "Run a GraphQL query or mutation. Rust checks the syntax, sends it per GraphQL over HTTP (POST JSON, or GET for queries when use_get is true) and checks that the answer is a well-formed GraphQL response.",
        vec![
            parameter("query", "string", "The GraphQL document, e.g. query($id: ID!) { book(id: $id) { title } }", true),
            parameter("variables", "object", "Values for the document's variables, keyed by name without the $", false),
            parameter("operation_name", "string", "Which operation to run when the document holds several", false),
            parameter("use_get", "boolean", "Send a query as GET with URL parameters instead of POST (mutations are always POST)", false),
        ],
        json!({"type":"graphql_query","query":"{ hello }"}),
    )
}
fn disconnect_action() -> ActionDefinition {
    action(
        "disconnect",
        "Stop this GraphQL client",
        vec![],
        json!({"type":"disconnect"}),
    )
}
fn actions() -> Vec<ActionDefinition> {
    vec![query_action(), disconnect_action()]
}

pub static CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "graphql_connected",
        "The client is ready; when introspection is on and the server allows it, the root fields it offers are listed",
        query_action().example.clone(),
    )
    .with_parameters(vec![
        parameter("url", "string", "GraphQL endpoint URL in use", true),
        parameter("root_fields", "object", "Root field signatures by operation type: {query: [\"book(id: ID!): Book\"], mutation: [...], subscription: [...]}", false),
        parameter("introspection_error", "string", "Why introspection gave no schema (disabled, refused or not GraphQL)", false),
    ])
    .with_actions(actions())
});
pub static RESPONSE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "graphql_response",
        "The server's answer to the last operation",
        disconnect_action().example.clone(),
    )
    .with_parameters(vec![
        parameter("operation_type", "string", "query or mutation", true),
        parameter(
            "operation_name",
            "string",
            "Name of the operation sent, when it had one",
            false,
        ),
        parameter("status", "number", "HTTP status of the answer", true),
        parameter(
            "media_type",
            "string",
            "application/graphql-response+json or application/json",
            false,
        ),
        parameter(
            "data",
            "object",
            "The response data (null when the operation failed entirely)",
            false,
        ),
        parameter(
            "errors",
            "array",
            "GraphQL errors [{message, path, locations, extensions}], absent when none",
            false,
        ),
        parameter(
            "error",
            "string",
            "Set when the answer was not a GraphQL response at all (e.g. HTTP 404)",
            false,
        ),
    ])
    .with_actions(actions())
});

impl Protocol for GraphqlClientProtocol {
    fn protocol_name(&self) -> &'static str {
        "GraphQL"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP>GraphQL"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["graphql", "gql", "graphql client"]
    }
    fn description(&self) -> &'static str {
        "GraphQL over HTTP client: introspects the root fields, runs queries and mutations, checks responses"
    }
    fn get_async_actions(&self, _: &crate::state::app_state::AppState) -> Vec<ActionDefinition> {
        actions()
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![CONNECTED_EVENT.clone(), RESPONSE_EVENT.clone()]
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "endpoint".into(),
                type_hint: "string".into(),
                description: "URL path of the GraphQL endpoint on remote_addr".into(),
                required: false,
                example: json!("/api/graphql"),
                default: Some(json!(crate::server::graphql::DEFAULT_ENDPOINT)),
            },
            ParameterDefinition {
                name: "introspect".into(),
                type_hint: "boolean".into(),
                description: "Ask for the schema's root fields on connect so the handler knows what it can query".into(),
                required: false,
                example: json!(false),
                default: Some(json!(super::DEFAULT_INTROSPECT)),
            },
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::*;
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::None)
            .implementation("Shared http_fetch client (reqwest natively, no redirects); GraphQL over HTTP POST/GET with Accept application/graphql-response+json then application/json; apollo-compiler syntax check; response shape checked")
            .llm_control("Which queries and mutations to run, with which variables, and what to do with the answers")
            .e2e_testing("tests/client/graphql: strawberry-graphql 0.330.2 (independent) answers introspection, POST and GET queries, variables, a union, a mutation, a field error and a validation error")
            .notes("Plain HTTP; no subscriptions, batching, uploads or persisted queries. 1 MiB answers, 30 s per request.")
            .max_inbound_bytes(engine::MAX_BODY_BYTES)
            .build()
    }
    fn example_prompt(&self) -> &'static str {
        "Ask the GraphQL API at 127.0.0.1:4000 for its first five books"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        let llm = json!({"type":"open_client","protocol":"graphql","remote_addr":"127.0.0.1:4000","instruction":"List the books and their authors"});
        let mut static_example = llm.clone();
        static_example["event_handlers"] = json!([
            {"event_pattern":"graphql_connected","handler":{"type":"static","actions":[query_action().example]}},
            {"event_pattern":"graphql_response","handler":{"type":"static","actions":[]}}
        ]);
        let mut scripted = static_example.clone();
        scripted["event_handlers"][1]["handler"] = json!({"type":"script","language":"python","code":"import json\nprint(json.dumps({'actions':[]}))"});
        crate::llm::actions::StartupExamples::new(llm, scripted, static_example)
    }
    fn group_name(&self) -> &'static str {
        "AI & API"
    }
}

impl Client for GraphqlClientProtocol {
    fn connect(
        &self,
        ctx: ConnectContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::connect(ctx))
    }
    fn execute_action(&self, v: Value) -> Result<ClientActionResult> {
        match v["type"].as_str() {
            Some("graphql_query") => {
                let query = v["query"].as_str().unwrap_or_default();
                let (op, _) = engine::parse_operation(query, v["operation_name"].as_str())?;
                ensure!(
                    op != apollo_compiler::executable::OperationType::Subscription,
                    "subscriptions are not supported over GraphQL over HTTP"
                );
                if let Some(vars) = v.get("variables").filter(|x| !x.is_null()) {
                    ensure!(vars.is_object(), "variables must be an object");
                }
                ensure!(
                    engine::budget_ok(&v),
                    "operation exceeds the GraphQL bounds"
                );
            }
            Some("disconnect") => return Ok(ClientActionResult::Disconnect),
            _ => bail!("Unknown GraphQL client action"),
        }
        Ok(ClientActionResult::Custom {
            name: v["type"].as_str().unwrap_or_default().into(),
            data: v,
        })
    }
}
