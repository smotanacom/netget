//! IPP protocol actions implementation.
//!
//! The LLM answers IPP operations with **structured** actions — an IPP status name and an
//! attribute object — and this file encodes them into the IPP binary format (RFC 8010). No
//! action carries raw bytes, hex or base64: the previous `ipp_response.body` parameter, which
//! the docs described as "hex-encoded IPP response data", was passed straight through
//! `hex::encode(body.as_bytes())`, so a model following the documentation put the ASCII text
//! `"0200000000000001..."` on the wire as the response body. It is gone.
//!
//! There is no print queue and no job store here, deliberately: the protocol implements no
//! storage, so job IDs, job states and printer attributes all come from the model.

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

/// IPP protocol action handler
pub struct IppProtocol {}

impl IppProtocol {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for IppProtocol {
    fn default() -> Self {
        Self::new()
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for IppProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        vec![
                crate::llm::actions::ParameterDefinition {
                    name: "send_first".to_string(),
                    type_hint: "boolean".to_string(),
                    description: "Accepted and ignored: IPP is strictly request/response over HTTP, so the server never speaks first".to_string(),
                    required: false,
                    example: serde_json::json!(false),
                },
            ]
    }

    /// No async actions.
    ///
    /// `list_print_jobs` used to be declared here. It returned a hardcoded `{"jobs": []}` with
    /// a `// This is a placeholder` comment, because there is no job store to list — and there
    /// must not be one: the protocol implements no storage, the model owns job state. Removed.
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        Vec::new()
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        ipp_response_actions()
    }
    fn protocol_name(&self) -> &'static str {
        "IPP"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_ipp_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP>IPP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["ipp", "printer", "print"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            // 631 is below 1024 and is the port every IPP client tries by default, so the
            // preflight check in server_startup.rs should fire rather than letting the bind
            // fail with a bare EPERM.
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(631))
            .implementation("hyper HTTP/1 + manual IPP (RFC 8010) attribute encoding")
            .llm_control("IPP status code, printer attributes, job attributes, HTTP status")
            .e2e_testing("ipptool / curl --data-binary, and tests/server/ipp/test.rs")
            .notes(
                "Responses are built from structured attributes, never from model-supplied \
                 bytes. The request-id is echoed by the server, not by the model. No job \
                 store: job IDs and states come from the model. Requests are parsed only far \
                 enough to name the operation - request attributes are not decoded, so the \
                 model is told which operation was asked for but not what it asked for. No \
                 IPPS, no authentication, no CUPS extensions.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "Internet Printing Protocol server"
    }
    fn example_prompt(&self) -> &'static str {
        "Start an IPP server on port 631"
    }
    fn group_name(&self) -> &'static str {
        "Web & File"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // Deterministic: report a fixed idle printer for every IPP request, no
        // LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "ipp_request_received":
    actions = [{"type": "ipp_printer_attributes",
                "attributes": {"printer-name": "NetGet Printer",
                               "printer-state": "idle",
                               "printer-is-accepting-jobs": True}}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: LLM handles all IPP responses intelligently
            json!({
                "type": "open_server",
                "port": 631,
                "base_stack": "ipp",
                "instruction": "IPP printer server. Answer Get-Printer-Attributes with printer-name 'NetGet Printer', printer-state 'idle'. Accept print jobs, assigning increasing job ids."
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_server",
                "port": 631,
                "base_stack": "ipp",
                "event_handlers": [{
                    "event_pattern": "ipp_request_received",
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
                "port": 631,
                "base_stack": "ipp",
                "event_handlers": [{
                    "event_pattern": "ipp_request_received",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "ipp_printer_attributes",
                            "attributes": {
                                "printer-name": "NetGet Printer",
                                "printer-state": "idle",
                                "printer-is-accepting-jobs": true
                            }
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for IppProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::ipp::IppServer;
            let send_first = ctx
                .startup_params
                .as_ref()
                .map(|p| p.get_optional_bool("send_first"))
                .transpose()?
                .flatten()
                .unwrap_or(false);

            IppServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                send_first,
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
            "ipp_response" => self.execute_ipp_response(action),
            "ipp_printer_attributes" => self.execute_ipp_printer_attributes(action),
            "ipp_job_attributes" => self.execute_ipp_job_attributes(action),
            _ => Err(anyhow::anyhow!("Unknown IPP action: {}", action_type)),
        }
    }
}

impl IppProtocol {
    fn execute_ipp_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        // `http_status` is the documented name; `status` is accepted because that is what the
        // action was originally called and prompts in the wild still use it.
        // Range-checked, not narrowed. `as u16` on a `u64` wraps in silence, so an
        // `http_status` of 65736 became 200 — the model's refusal delivered as the success it
        // was refusing. Refusing beats clamping: 599 is not what the model asked for either.
        let http_status = match action.get("http_status").or_else(|| action.get("status")) {
            None => 200,
            Some(v) if v.is_null() => 200,
            Some(v) => {
                let raw = v.as_u64().with_context(|| {
                    format!("send_ipp_response 'http_status' must be a number, got {v}")
                })?;
                if !(100..=599).contains(&raw) {
                    anyhow::bail!(
                        "send_ipp_response 'http_status' {raw} is not an HTTP status code; \
                         use 100-599. IPP carries its own outcome in 'ipp_status' and answers \
                         200 even for an IPP-level error."
                    );
                }
                raw as u16
            }
        };

        let ipp_status_name = action
            .get("ipp_status")
            .and_then(|v| v.as_str())
            .unwrap_or("successful-ok");
        let ipp_status = ipp_status_code(ipp_status_name);

        let status_message = action.get("status_message").and_then(|v| v.as_str());

        debug!(
            "IPP response: http={} ipp_status={} (0x{:04x})",
            http_status, ipp_status_name, ipp_status
        );

        let body = build_ipp_response(ipp_status, status_message, None);
        Ok(ipp_wire_response(http_status, body))
    }

    fn execute_ipp_printer_attributes(&self, action: serde_json::Value) -> Result<ActionResult> {
        let attributes = action
            .get("attributes")
            .and_then(|v| v.as_object())
            .context("Missing 'attributes' object")?;

        debug!("IPP printer attributes: {} attrs", attributes.len());

        let ipp_status = ipp_status_code(
            action
                .get("ipp_status")
                .and_then(|v| v.as_str())
                .unwrap_or("successful-ok"),
        );

        let body = build_ipp_response(ipp_status, None, Some((PRINTER_ATTRIBUTES_TAG, attributes)));
        Ok(ipp_wire_response(200, body))
    }

    fn execute_ipp_job_attributes(&self, action: serde_json::Value) -> Result<ActionResult> {
        let attributes = action
            .get("attributes")
            .and_then(|v| v.as_object())
            .context("Missing 'attributes' object")?;

        debug!("IPP job attributes: {} attrs", attributes.len());

        let ipp_status = ipp_status_code(
            action
                .get("ipp_status")
                .and_then(|v| v.as_str())
                .unwrap_or("successful-ok"),
        );

        let body = build_ipp_response(ipp_status, None, Some((JOB_ATTRIBUTES_TAG, attributes)));
        Ok(ipp_wire_response(200, body))
    }
}

/// Package an encoded IPP body for `handle_ipp_request_with_llm`.
///
/// The hex here is transport between the executor and the HTTP handler, never something the
/// model writes or reads — `ActionResult::Custom` carries `serde_json::Value`, which has no
/// byte type. The handler decodes it and stamps the real request-id into it.
fn ipp_wire_response(http_status: u16, body: Vec<u8>) -> ActionResult {
    ActionResult::Custom {
        name: "ipp_response".to_string(),
        data: json!({
            "http_status": http_status,
            "body_hex": hex::encode(&body),
        }),
    }
}

fn ipp_response_actions() -> Vec<ActionDefinition> {
    vec![
        ipp_response_action(),
        ipp_printer_attributes_action(),
        ipp_job_attributes_action(),
    ]
}

/// Action definition: Send a bare IPP response (status only, no attribute group)
pub fn ipp_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "ipp_response".to_string(),
        description: "Answer an IPP operation with a status code and no attributes. Use for \
                      acknowledgements and for rejecting an operation."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "ipp_status".to_string(),
                type_hint: "string".to_string(),
                description: "IPP status name: successful-ok (default), client-error-bad-request, \
                              client-error-not-found, client-error-not-authorized, \
                              client-error-document-format-not-supported, \
                              server-error-not-accepting-jobs, server-error-internal-error, \
                              server-error-busy"
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "status_message".to_string(),
                type_hint: "string".to_string(),
                description: "Human-readable explanation returned as the status-message attribute"
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "http_status".to_string(),
                type_hint: "number".to_string(),
                description: "HTTP status code (default 200). IPP errors belong in ipp_status; \
                              use this only for HTTP-level failures such as 401 or 404."
                    .to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "ipp_response",
            "ipp_status": "successful-ok"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> IPP {ipp_status}")
                .with_debug("IPP ipp_response: ipp_status={ipp_status} http_status={http_status}"),
        ),
    }
}

/// Action definition: Send printer attributes response
pub fn ipp_printer_attributes_action() -> ActionDefinition {
    ActionDefinition {
        name: "ipp_printer_attributes".to_string(),
        description: "Answer Get-Printer-Attributes with a printer attribute group".to_string(),
        parameters: vec![
            Parameter {
                name: "attributes".to_string(),
                type_hint: "object".to_string(),
                description: "Printer attributes by name. Strings, numbers, booleans and arrays \
                              of them are all accepted and encoded with the right IPP value \
                              tag. printer-state takes 'idle', 'processing' or 'stopped'. \
                              Example: {\"printer-name\": \"My Printer\", \"printer-state\": \
                              \"idle\", \"printer-is-accepting-jobs\": true, \
                              \"printer-uri-supported\": [\"ipp://localhost:631/printers/p1\"]}"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "ipp_status".to_string(),
                type_hint: "string".to_string(),
                description: "IPP status name (default successful-ok)".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "ipp_printer_attributes",
            "attributes": {
                "printer-name": "My Printer",
                "printer-state": "idle",
                "printer-is-accepting-jobs": true,
                "printer-uri-supported": ["ipp://localhost:631/printers/my-printer"],
                "document-format-supported": ["application/pdf", "text/plain"]
            }
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> IPP printer attributes")
                .with_debug("IPP ipp_printer_attributes: {attributes_len} attrs"),
        ),
    }
}

/// Action definition: Send job attributes response
pub fn ipp_job_attributes_action() -> ActionDefinition {
    ActionDefinition {
        name: "ipp_job_attributes".to_string(),
        description: "Answer a job operation (Print-Job, Get-Job-Attributes, Create-Job) with a \
                      job attribute group"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "attributes".to_string(),
                type_hint: "object".to_string(),
                description: "Job attributes by name. job-state takes 'pending', 'held', \
                              'processing', 'stopped', 'canceled', 'aborted' or 'completed'. \
                              Example: {\"job-id\": 123, \"job-state\": \"completed\", \
                              \"job-name\": \"document.pdf\"}"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "ipp_status".to_string(),
                type_hint: "string".to_string(),
                description: "IPP status name (default successful-ok)".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "ipp_job_attributes",
            "attributes": {
                "job-id": 123,
                "job-uri": "ipp://localhost:631/jobs/123",
                "job-state": "completed",
                "job-name": "document.pdf"
            }
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> IPP job attributes")
                .with_debug("IPP ipp_job_attributes: {attributes_len} attrs"),
        ),
    }
}

// ============================================================================
// IPP binary encoding (RFC 8010)
// ============================================================================

/// Delimiter tag opening the operation-attributes group.
const OPERATION_ATTRIBUTES_TAG: u8 = 0x01;
/// Delimiter tag opening the job-attributes group.
const JOB_ATTRIBUTES_TAG: u8 = 0x02;
/// Delimiter tag opening the printer-attributes group.
const PRINTER_ATTRIBUTES_TAG: u8 = 0x04;
/// Delimiter tag ending all attribute groups.
const END_OF_ATTRIBUTES_TAG: u8 = 0x03;

// Value tags (RFC 8010 section 3.5.2).
const TAG_INTEGER: u8 = 0x21;
const TAG_BOOLEAN: u8 = 0x22;
const TAG_ENUM: u8 = 0x23;
const TAG_TEXT: u8 = 0x41;
const TAG_NAME: u8 = 0x42;
const TAG_KEYWORD: u8 = 0x44;
const TAG_URI: u8 = 0x45;
const TAG_CHARSET: u8 = 0x47;
const TAG_NATURAL_LANGUAGE: u8 = 0x48;
const TAG_MIME_MEDIA_TYPE: u8 = 0x49;

/// Byte offset of the request-id field in an IPP message.
///
/// `handle_ipp_request_with_llm` overwrites these four bytes with the request's own id before
/// sending, so a response always correlates even though no action carries a request-id.
pub const REQUEST_ID_OFFSET: usize = 4;

/// Map an IPP status name to its wire code (RFC 8011 appendix B).
///
/// Unknown names become `server-error-internal-error` rather than `successful-ok`: telling a
/// client everything is fine because the model invented a status name is the worse failure.
fn ipp_status_code(name: &str) -> u16 {
    match name {
        "successful-ok" => 0x0000,
        "successful-ok-ignored-or-substituted-attributes" => 0x0001,
        "successful-ok-conflicting-attributes" => 0x0002,
        "client-error-bad-request" => 0x0400,
        "client-error-forbidden" => 0x0401,
        "client-error-not-authenticated" => 0x0402,
        "client-error-not-authorized" => 0x0403,
        "client-error-not-possible" => 0x0404,
        "client-error-timeout" => 0x0405,
        "client-error-not-found" => 0x0406,
        "client-error-gone" => 0x0407,
        "client-error-request-entity-too-large" => 0x0408,
        "client-error-request-value-too-long" => 0x0409,
        "client-error-document-format-not-supported" => 0x040A,
        "client-error-attributes-or-values-not-supported" => 0x040B,
        "client-error-uri-scheme-not-supported" => 0x040C,
        "client-error-charset-not-supported" => 0x040D,
        "client-error-conflicting-attributes" => 0x040E,
        "client-error-compression-not-supported" => 0x040F,
        "client-error-document-access-error" => 0x0412,
        "server-error-internal-error" => 0x0500,
        "server-error-operation-not-supported" => 0x0501,
        "server-error-service-unavailable" => 0x0502,
        "server-error-version-not-supported" => 0x0503,
        "server-error-device-error" => 0x0504,
        "server-error-temporary-error" => 0x0505,
        "server-error-not-accepting-jobs" => 0x0506,
        "server-error-busy" => 0x0507,
        "server-error-job-canceled" => 0x0508,
        other => {
            tracing::warn!(
                "Unknown IPP status name '{}', encoding server-error-internal-error",
                other
            );
            0x0500
        }
    }
}

/// `printer-state` is an enum in IPP, not a keyword: clients read the integer.
fn printer_state_enum(value: &str) -> Option<i32> {
    match value {
        "idle" => Some(3),
        "processing" => Some(4),
        "stopped" => Some(5),
        _ => None,
    }
}

/// `job-state` is likewise an enum.
fn job_state_enum(value: &str) -> Option<i32> {
    match value {
        "pending" => Some(3),
        "pending-held" | "held" => Some(4),
        "processing" => Some(5),
        "processing-stopped" | "stopped" => Some(6),
        "canceled" => Some(7),
        "aborted" => Some(8),
        "completed" => Some(9),
        _ => None,
    }
}

/// Append a length-prefixed byte string using IPP's two-byte big-endian length.
///
/// The previous encoder wrote name lengths as `[0x00, len as u8]`, which silently truncated
/// any name of 256 bytes or more and produced an unparseable message.
fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

/// Encode one attribute value. `name` is empty for the second and later values of a set,
/// which is how IPP expresses multi-valued attributes.
fn push_attribute_value(out: &mut Vec<u8>, name: &str, attr_name: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::Bool(b) => {
            out.push(TAG_BOOLEAN);
            push_len_prefixed(out, name.as_bytes());
            push_len_prefixed(out, &[u8::from(*b)]);
        }
        serde_json::Value::Number(n) => {
            let v = n.as_i64().unwrap_or(0) as i32;
            out.push(TAG_INTEGER);
            push_len_prefixed(out, name.as_bytes());
            push_len_prefixed(out, &v.to_be_bytes());
        }
        serde_json::Value::String(s) => {
            // Enum-valued attributes carry an integer on the wire; a client reading
            // printer-state as a keyword string will not recognise it.
            let as_enum = match attr_name {
                "printer-state" => printer_state_enum(s),
                "job-state" => job_state_enum(s),
                _ => None,
            };
            if let Some(v) = as_enum {
                out.push(TAG_ENUM);
                push_len_prefixed(out, name.as_bytes());
                push_len_prefixed(out, &v.to_be_bytes());
                return;
            }

            let tag = if attr_name.contains("charset") {
                TAG_CHARSET
            } else if attr_name.contains("natural-language") {
                TAG_NATURAL_LANGUAGE
            } else if attr_name.ends_with("-uri") || attr_name.ends_with("-uri-supported") {
                TAG_URI
            } else if attr_name.contains("document-format")
                || s.contains('/') && s.contains("application")
            {
                TAG_MIME_MEDIA_TYPE
            } else if attr_name.ends_with("-name") {
                TAG_NAME
            } else if attr_name.ends_with("-supported")
                || attr_name.ends_with("-default")
                || attr_name.ends_with("-requested")
                || attr_name.ends_with("-reasons")
            {
                TAG_KEYWORD
            } else {
                TAG_TEXT
            };
            out.push(tag);
            push_len_prefixed(out, name.as_bytes());
            push_len_prefixed(out, s.as_bytes());
        }
        other => {
            // Objects and null have no IPP representation; send the JSON text so the value is
            // at least visible to the operator rather than dropped silently.
            out.push(TAG_TEXT);
            push_len_prefixed(out, name.as_bytes());
            push_len_prefixed(out, other.to_string().as_bytes());
        }
    }
}

/// Encode an attribute group's contents (the delimiter tag is written by the caller).
fn push_attributes(out: &mut Vec<u8>, attributes: &serde_json::Map<String, serde_json::Value>) {
    for (name, value) in attributes {
        match value {
            serde_json::Value::Array(values) => {
                // Multi-valued: the first value carries the name, the rest carry a
                // zero-length name (RFC 8010 "additional value").
                for (idx, v) in values.iter().enumerate() {
                    let value_name = if idx == 0 { name.as_str() } else { "" };
                    push_attribute_value(out, value_name, name, v);
                }
                if values.is_empty() {
                    // An attribute with no values still has to exist on the wire.
                    out.push(TAG_KEYWORD);
                    push_len_prefixed(out, name.as_bytes());
                    push_len_prefixed(out, b"none");
                }
            }
            v => push_attribute_value(out, name, name, v),
        }
    }
}

/// Build a complete IPP response message.
///
/// Layout: version(2) status(2) request-id(4) operation-group [extra-group] end-tag. The
/// request-id is written as 0 and stamped with the request's real id by the HTTP handler.
fn build_ipp_response(
    status_code: u16,
    status_message: Option<&str>,
    group: Option<(u8, &serde_json::Map<String, serde_json::Value>)>,
) -> Vec<u8> {
    let mut out = Vec::new();

    out.extend_from_slice(&[0x02, 0x00]); // IPP version 2.0
    out.extend_from_slice(&status_code.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // request-id placeholder, stamped later

    // Operation attributes group. attributes-charset and attributes-natural-language must be
    // the first two attributes of the first group, in that order.
    out.push(OPERATION_ATTRIBUTES_TAG);
    out.push(TAG_CHARSET);
    push_len_prefixed(&mut out, b"attributes-charset");
    push_len_prefixed(&mut out, b"utf-8");
    out.push(TAG_NATURAL_LANGUAGE);
    push_len_prefixed(&mut out, b"attributes-natural-language");
    push_len_prefixed(&mut out, b"en-us");
    if let Some(message) = status_message {
        out.push(TAG_TEXT);
        push_len_prefixed(&mut out, b"status-message");
        push_len_prefixed(&mut out, message.as_bytes());
    }

    if let Some((group_tag, attributes)) = group {
        out.push(group_tag);
        push_attributes(&mut out, attributes);
    }

    out.push(END_OF_ATTRIBUTES_TAG);
    out
}

// ============================================================================
// IPP Action Constants
// ============================================================================

pub static IPP_RESPONSE_ACTION: LazyLock<ActionDefinition> = LazyLock::new(ipp_response_action);
pub static IPP_PRINTER_ATTRIBUTES_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(ipp_printer_attributes_action);
pub static IPP_JOB_ATTRIBUTES_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(ipp_job_attributes_action);

// ============================================================================
// IPP Event Type Constants
// ============================================================================

/// IPP request event - triggered when client sends an IPP request
pub static IPP_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ipp_request_received",
        "IPP request received from client",
        json!({
            "type": "ipp_printer_attributes",
            "attributes": {
                "printer-name": "NetGet Printer",
                "printer-state": "idle",
                "printer-is-accepting-jobs": true
            }
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "method".to_string(),
            type_hint: "string".to_string(),
            description: "HTTP method (POST for every real IPP operation)".to_string(),
            required: true,
        },
        Parameter {
            name: "uri".to_string(),
            type_hint: "string".to_string(),
            description: "Request URI, which names the printer or job queue".to_string(),
            required: true,
        },
        Parameter {
            name: "operation".to_string(),
            type_hint: "string".to_string(),
            description: "IPP operation name (e.g., Print-Job, Get-Printer-Attributes). \
                          'Empty' when the request carried no IPP body, 'Malformed' when it \
                          was too short to parse."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "request_id".to_string(),
            type_hint: "number".to_string(),
            description: "IPP request-id. Informational: the server echoes it into the \
                          response itself, so no action needs to carry it."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "ipp_version".to_string(),
            type_hint: "string".to_string(),
            description: "IPP version the client used, e.g. '2.0'".to_string(),
            required: true,
        },
    ])
    .with_actions(ipp_response_actions())
    .with_alternative_example(json!({
        "type": "ipp_job_attributes",
        "attributes": {"job-id": 1, "job-state": "processing", "job-name": "document.pdf"}
    }))
    .with_alternative_example(json!({
        "type": "ipp_response",
        "ipp_status": "server-error-not-accepting-jobs",
        "status_message": "Printer is offline"
    }))
    .with_log_template(
        LogTemplate::new()
            .with_info("IPP {operation}")
            .with_debug("IPP operation={operation} uri={uri} request_id={request_id}")
            .with_trace("IPP: {json_pretty(.)}"),
    )
});

/// Get IPP event types
pub fn get_ipp_event_types() -> Vec<EventType> {
    vec![IPP_REQUEST_EVENT.clone()]
}
