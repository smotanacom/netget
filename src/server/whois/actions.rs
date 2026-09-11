//! WHOIS protocol actions implementation

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

/// Strip control characters from a value that occupies one line of a WHOIS record.
///
/// A WHOIS *response* is free text and may be anything — `send_whois_response` exists for
/// exactly that and is deliberately not filtered. A WHOIS **record** is not free text: it is
/// `Key: value` lines, and a `registrar` of `"Foo\r\nRegistrant Name: Bar"` forges a field
/// the model never asserted. Whoever reads the output — a human, or more dangerously a script
/// grepping for `Registrant Name:` — cannot tell a forged line from a real one.
///
/// This is the family's signature defect (FTP's `send_ftp_response` documented that text must
/// not contain CR/LF and checked nothing, turning a 331 into a successful login), and it is
/// worth noting that **both of this server's neighbours added the guard independently and
/// WHOIS did not** — `gopher`'s `sanitize_field`, `finger`'s `strip_controls`, `ident`'s
/// `sanitize_token`. WHOIS is the protocol the simple ones in this repo were told to copy, so
/// it is the one place the omission propagates from.
///
/// The guarantee is **no forged line**, not "the words disappear". `"Foo\r\nRegistrant Name:
/// Bar"` still says `Registrant Name: Bar` — it just says it inside the `Registrar:` field,
/// where a line-oriented reader attributes it correctly.
///
/// CR, LF and tab become a space rather than vanishing, which is `gopher`'s choice next door
/// and the better one: deleting them concatenates the two sides into a single word
/// (`Good RegistrarRegistrant`), which reads as one value and is its own small lie. Every
/// other control is dropped — ESC included, because this text lands in a terminal and on the
/// operator's own dashboard.
fn sanitize_line_field(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            other if other.is_control() => None,
            other => Some(other),
        })
        .collect()
}

pub struct WhoisProtocol;

impl WhoisProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for WhoisProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        Vec::new() // WHOIS has no async actions
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_whois_response_action(),
            send_whois_record_action(),
            send_error_action(),
            close_connection_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "WHOIS"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_whois_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>WHOIS"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["whois"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Beta)
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(43))
            .implementation("Manual TCP connection handling")
            .llm_control("WHOIS query responses (domain, registrant, contact info)")
            .e2e_testing(
                "tests/server/whois/e2e_test.rs, 6 LLM calls. One test drives the real whois(1) \
                 binary (`whois -h localhost -p PORT example.com`) and asserts the record it \
                 prints; the rest use a raw TCP socket for the cases whois(1) cannot reach - an \
                 error reply, two queries on one connection, and connection logging. Note macOS \
                 whois(1) segfaults when -h is given an IP literal; a hostname works. \
                 line_framing_test.rs additionally pins that a query split across four TCP \
                 segments raises one event, and that 16 KiB with no newline is refused; \
                 peer_inject_test.rs covers dashboard injection with zero LLM calls.",
            )
            .notes(
                "Simple line-based protocol. RFC 3912 has the server close as soon as its output \
                 is finished; this server keeps reading so several queries can share a \
                 connection, so a handler that answers without close_connection leaves whois(1) \
                 blocked on EOF until the 15s post-reply idle timeout closes it. Pair the answer \
                 with close_connection. Queries are read as lines, capped at 4 KiB.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "WHOIS domain lookup server"
    }
    fn example_prompt(&self) -> &'static str {
        "WHOIS server on port 43 - respond with fake registrar information for any domain"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        StartupExamples::new(
            // LLM-driven example
            json!({
                "type": "open_server",
                "port": 43,
                "base_stack": "whois",
                "instruction": "WHOIS server responding with fake registration info for any domain"
            }),
            // Script-based example
            json!({
                "type": "open_server",
                "port": 43,
                "base_stack": "whois",
                "event_handlers": [{
                    "event_pattern": "whois_query",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "# Return WHOIS record for queried domain\ndomain = event.get('query', 'unknown.com').strip()\nrespond([{'type': 'send_whois_record', 'domain': domain, 'registrar': 'Example Registrar Inc.', 'registrant': 'Example Organization', 'name_servers': ['ns1.example.com', 'ns2.example.com']}])"
                    }
                }]
            }),
            // Static handler example
            json!({
                "type": "open_server",
                "port": 43,
                "base_stack": "whois",
                "event_handlers": [{
                    "event_pattern": "whois_query",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_whois_record",
                            "domain": "example.com",
                            "registrar": "Example Registrar Inc.",
                            "registrant": "Example Organization",
                            "name_servers": ["ns1.example.com", "ns2.example.com"]
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for WhoisProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::whois::WhoisServer;
            WhoisServer::spawn_with_llm_actions(
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

        match action_type {
            "send_whois_response" => self.execute_send_whois_response(action),
            "send_whois_record" => self.execute_send_whois_record(action),
            "send_error" => self.execute_send_error(action),
            "close_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown WHOIS action: {}", action_type)),
        }
    }
}

impl WhoisProtocol {
    fn execute_send_whois_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let response = action
            .get("response")
            .and_then(|v| v.as_str())
            .context("Missing 'response' parameter")?;

        // WHOIS protocol ends responses with CRLF
        let mut data = response.to_string();
        if !data.ends_with('\n') {
            data.push_str("\r\n");
        }

        Ok(ActionResult::Output(data.into_bytes()))
    }

    fn execute_send_whois_record(&self, action: serde_json::Value) -> Result<ActionResult> {
        let domain = action
            .get("domain")
            .and_then(|v| v.as_str())
            .context("Missing 'domain' parameter")?;

        let registrar = action
            .get("registrar")
            .and_then(|v| v.as_str())
            .unwrap_or("Example Registrar, Inc.");

        let registrant = action
            .get("registrant")
            .and_then(|v| v.as_str())
            .unwrap_or("Registrant Contact");

        let admin_contact = action
            .get("admin_contact")
            .and_then(|v| v.as_str())
            .unwrap_or("Admin Contact");

        let name_servers = action
            .get("name_servers")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        // Every field is one line of a `Key: value` record, so each is sanitised: a CR or LF
        // in any of them would forge a field the model never asserted. See
        // `sanitize_line_field`. Use `send_whois_response` when free text is wanted.
        let mut response = String::new();
        response.push_str(&format!("Domain Name: {}\r\n", sanitize_line_field(domain)));
        response.push_str(&format!(
            "Registrar: {}\r\n",
            sanitize_line_field(registrar)
        ));
        response.push_str(&format!(
            "Registrant Name: {}\r\n",
            sanitize_line_field(registrant)
        ));
        response.push_str(&format!(
            "Admin Name: {}\r\n",
            sanitize_line_field(admin_contact)
        ));

        for ns in name_servers {
            response.push_str(&format!("Name Server: {}\r\n", sanitize_line_field(ns)));
        }

        response.push_str("\r\n");
        Ok(ActionResult::Output(response.into_bytes()))
    }

    fn execute_send_error(&self, action: serde_json::Value) -> Result<ActionResult> {
        let error_msg = action
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Domain not found");

        // One line, so it is sanitised: `Error: nope\r\nDomain Name: evil.test` would forge a
        // record out of a refusal.
        let response = format!("Error: {}\r\n\r\n", sanitize_line_field(error_msg));
        Ok(ActionResult::Output(response.into_bytes()))
    }
}

fn send_whois_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_whois_response".to_string(),
        description: "Send custom WHOIS response - free text, written exactly as given (a CRLF \
                      is appended if missing), so this is the action to use for multi-line \
                      output. Use send_whois_record instead when the answer is a record: its \
                      fields are single lines and are sanitised, and this one is not. RFC 3912 \
                      has the server close as soon as the output is finished, and a real client \
                      (whois(1)) reads until EOF, so pair this with close_connection unless you \
                      intend the client to block"
            .to_string(),
        parameters: vec![Parameter {
            name: "response".to_string(),
            type_hint: "string".to_string(),
            description: "Response text to send".to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_whois_response",
            "response": "Domain Name: example.com\nRegistrar: Example Inc.\n"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> WHOIS response ({response_len}B)")
                .with_debug("WHOIS send_whois_response: {response_len} bytes"),
        ),
    }
}

fn send_whois_record_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_whois_record".to_string(),
        description: "Send formatted WHOIS record. RFC 3912 has the server close as soon as the \
                      output is finished, and a real client (whois(1)) reads until EOF, so pair \
                      this with close_connection unless you intend the client to block"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name".to_string(),
                required: true,
            },
            Parameter {
                name: "registrar".to_string(),
                type_hint: "string".to_string(),
                description: "Registrar name".to_string(),
                required: false,
            },
            Parameter {
                name: "registrant".to_string(),
                type_hint: "string".to_string(),
                description: "Registrant name".to_string(),
                required: false,
            },
            Parameter {
                name: "admin_contact".to_string(),
                type_hint: "string".to_string(),
                description: "Admin contact name".to_string(),
                required: false,
            },
            Parameter {
                name: "name_servers".to_string(),
                type_hint: "array".to_string(),
                description: "List of nameservers".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_whois_record",
            "domain": "example.com",
            "registrar": "VeriSign Registry",
            "registrant": "Example Inc.",
            "name_servers": ["ns1.example.com", "ns2.example.com"]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> WHOIS {domain} ({registrar})")
                .with_debug("WHOIS send_whois_record: domain={domain}, registrar={registrar}"),
        ),
    }
}

fn send_error_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_error".to_string(),
        description: "Send error message for domain not found".to_string(),
        parameters: vec![Parameter {
            name: "message".to_string(),
            type_hint: "string".to_string(),
            description: "Error message".to_string(),
            required: false,
        }],
        example: json!({
            "type": "send_error",
            "message": "Domain not found in database"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> WHOIS error: {message}")
                .with_debug("WHOIS send_error: {message}"),
        ),
    }
}

fn close_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_connection".to_string(),
        description: "Close the WHOIS connection".to_string(),
        parameters: vec![],
        example: json!({"type": "close_connection"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("WHOIS connection closed")
                .with_debug("WHOIS close_connection"),
        ),
    }
}

/// WHOIS query event - triggered when client sends a query
pub static WHOIS_QUERY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "whois_query",
        "Client sent a WHOIS domain lookup query",
        // Rendered verbatim into the protocol documentation the model reads
        // (src/llm/actions/tools.rs and src/mcp_stdio/docs.rs), so it must be an action the
        // executor actually accepts - the previous {"type": "placeholder"} was not.
        json!({
            "type": "send_whois_record",
            "domain": "example.com",
            "registrar": "Example Registrar, Inc.",
            "registrant": "Example Organization",
            "name_servers": ["ns1.example.com", "ns2.example.com"]
        }),
    )
    .with_parameters(vec![Parameter {
        name: "query".to_string(),
        type_hint: "string".to_string(),
        description: "The WHOIS query string".to_string(),
        required: true,
    }])
    .with_log_template(
        LogTemplate::new()
            .with_info("WHOIS {query}")
            .with_debug("WHOIS query: {query}")
            .with_trace("WHOIS: {json_pretty(.)}"),
    )
    .with_actions(vec![
        send_whois_response_action(),
        send_whois_record_action(),
        send_error_action(),
        close_connection_action(),
    ])
    .with_alternative_example(json!({
        "type": "send_error",
        "message": "No match for domain \"example.invalid\""
    }))
});

pub fn get_whois_event_types() -> Vec<EventType> {
    vec![WHOIS_QUERY_EVENT.clone()]
}
