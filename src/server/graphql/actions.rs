use super::engine;
use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::{EventType, SpawnContext};
use crate::state::app_state::AppState;
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;

#[derive(Default)]
pub struct GraphqlProtocol;
impl GraphqlProtocol {
    pub fn new() -> Self {
        Self
    }
}

pub fn parameter(name: &str, kind: &str, description: &str, required: bool) -> Parameter {
    Parameter {
        name: name.into(),
        type_hint: kind.into(),
        description: description.into(),
        required,
    }
}

pub fn action(
    name: &str,
    description: &str,
    parameters: Vec<Parameter>,
    example: Value,
) -> ActionDefinition {
    ActionDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
        example,
        log_template: Some(LogTemplate::new().with_info(format!("-> GraphQL {name}"))),
    }
}

fn result_action() -> ActionDefinition {
    action(
        "graphql_result",
        "Answer the operation with data shaped like the event's `shape`: an object keyed by the response keys (aliases included), leaves as JSON values of their GraphQL type, objects as nested objects, lists as arrays; give `__typename` for union/interface values. Rust executes the query over this data, so missing nullable fields become null and a wrong type becomes a field error with its path.",
        vec![
            parameter("data", "object", "Response data keyed by response key, e.g. {\"book\": {\"title\": \"Dune\"}}", true),
            parameter("errors", "array", "Optional field errors to report alongside the data: [{message, path: [\"book\", \"author\"], extensions}]", false),
        ],
        json!({"type":"graphql_result","data":{"hello":"Hello, world"}}),
    )
}

fn error_action() -> ActionDefinition {
    action(
        "graphql_error",
        "Refuse the whole operation (e.g. not authorized): the response carries data null and this one error",
        vec![
            parameter("message", "string", "Error message the client sees (1 to 4096 bytes)", true),
            parameter("extensions", "object", "Optional machine-readable details, e.g. {\"code\": \"FORBIDDEN\"}", false),
        ],
        json!({"type":"graphql_error","message":"not authorized","extensions":{"code":"FORBIDDEN"}}),
    )
}

pub static OPERATION_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "graphql_operation",
        "A query or mutation that parsed and validated against the schema (introspection is answered by Rust). Supply the data for its root fields.",
        result_action().example.clone(),
    )
    .with_parameters(vec![
        parameter("operation_type", "string", "query or mutation", true),
        parameter("operation_name", "string", "The operation's name, when it has one", false),
        parameter("query", "string", "The GraphQL document as sent", true),
        parameter("variables", "object", "Variables after coercion to their declared types", true),
        parameter("root_fields", "array", "Root fields asked for: [{response_key, field, arguments}] with variables substituted", true),
        parameter("shape", "object", "Skeleton of the data to return: response keys mapped to GraphQL types or nested skeletons", true),
        parameter("method", "string", "HTTP method (GET or POST)", true),
    ])
    .with_actions(vec![result_action(), error_action()])
});

fn startup(
    name: &str,
    kind: &str,
    description: &str,
    example: Value,
    default: Option<Value>,
) -> ParameterDefinition {
    ParameterDefinition {
        name: name.into(),
        type_hint: kind.into(),
        description: description.into(),
        required: false,
        example,
        default,
    }
}

impl Protocol for GraphqlProtocol {
    fn protocol_name(&self) -> &'static str {
        "GraphQL"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP>GraphQL"
    }
    fn description(&self) -> &'static str {
        "GraphQL over HTTP server: schema-validated queries and mutations, introspection, spec execution over handler data"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["graphql", "gql", "graphql server", "graphql api"]
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![result_action(), error_action()]
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![OPERATION_EVENT.clone()]
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            startup(
                "schema",
                "string",
                "GraphQL SDL the server validates against and introspects (up to 256 KiB); needs a Query type",
                json!("type Query { book(id: ID!): Book } type Book { id: ID! title: String! }"),
                Some(json!(super::DEFAULT_SCHEMA)),
            ),
            startup(
                "endpoint",
                "string",
                "URL path that serves GraphQL",
                json!("/api/graphql"),
                Some(json!(super::DEFAULT_ENDPOINT)),
            ),
            startup(
                "introspection",
                "boolean",
                "Answer __schema and __type queries (off: they become field errors)",
                json!(false),
                Some(json!(super::DEFAULT_INTROSPECTION)),
            ),
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::*;
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::None)
            .implementation("hyper HTTP/1.1 per the GraphQL-over-HTTP draft (POST JSON and GET, application/graphql-response+json and application/json); apollo-compiler 1.33 parses, validates, introspects and executes")
            .llm_control("The data (and field errors) behind every query and mutation, or a refusal")
            .e2e_testing("tests/server/graphql: gql 4.4.0 (independent; builds its schema from our introspection and validates locally) runs queries, aliases, fragments on a union, variables, a mutation and field errors; raw HTTP status/media-type rules")
            .notes("No subscriptions, batching, persisted queries, file uploads or @defer/@stream. No resolvers in Rust: the handler supplies the data, Rust executes the query over it. 1 MiB bodies, 64 KiB queries, parser depth 64.")
            .request_only("GraphQL over HTTP answers each request; nothing is pushed")
            .answers_on_failure()
            .max_inbound_bytes(engine::MAX_BODY_BYTES)
            .build()
    }
    fn example_prompt(&self) -> &'static str {
        "GraphQL server with books and authors; answer queries with a few classic novels"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        let llm = json!({"type":"open_server","base_stack":"graphql","port":4000,"instruction":"A bookstore API with a few classic novels","startup_params":{"schema":"type Query { books: [Book!]! } type Book { title: String! author: String! }"}});
        let mut static_example = llm.clone();
        static_example["event_handlers"] = json!([
            {"event_pattern":"graphql_operation","handler":{"type":"static","actions":[{"type":"graphql_result","data":{"books":[{"title":"Dune","author":"Frank Herbert"}]}}]}}
        ]);
        let mut scripted = static_example.clone();
        scripted["event_handlers"][0]["handler"] = json!({"type":"script","language":"python","code":"import json\nprint(json.dumps({'actions':[{'type':'graphql_result','data':{'books':[{'title':'Dune','author':'Frank Herbert'}]}}]}))"});
        crate::llm::actions::StartupExamples::new(llm, scripted, static_example)
    }
    fn group_name(&self) -> &'static str {
        "AI & API"
    }
}

impl Server for GraphqlProtocol {
    fn spawn(
        &self,
        ctx: SpawnContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::spawn(ctx))
    }
    fn execute_action(&self, v: Value) -> Result<ActionResult> {
        ensure!(engine::budget_ok(&v), "answer exceeds the GraphQL bounds");
        match v["type"].as_str() {
            Some("graphql_result") => {
                ensure!(
                    v["data"].is_object(),
                    "data must be an object keyed by response key"
                );
                engine::handler_errors(v.get("errors"))?;
            }
            Some("graphql_error") => {
                ensure!(
                    v["message"]
                        .as_str()
                        .is_some_and(|m| !m.is_empty() && m.len() <= 4096),
                    "message must be 1..4096 bytes"
                );
                if let Some(x) = v.get("extensions").filter(|x| !x.is_null()) {
                    ensure!(x.is_object(), "extensions must be an object");
                }
            }
            _ => bail!("Unknown GraphQL server action"),
        }
        Ok(ActionResult::Custom {
            name: v["type"].as_str().unwrap_or_default().into(),
            data: v,
        })
    }
}
