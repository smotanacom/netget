//! CoAP protocol actions.
//!
//! The model decides two things a constrained-device server actually decides: what a
//! resource *is* (its representation and media type) and which response code the request
//! deserves. Everything the transport determines — the message type of the reply, the
//! message id, the token — is reconstructed by `src/server/coap/mod.rs` from the request,
//! so the model can neither break reliability matching nor be asked to echo an
//! identifier it has no business handling.

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

use super::codec;

/// Names carried by the [`ActionResult::Custom`] results this protocol produces.
pub const RESULT_RESPONSE: &str = "coap_response";
pub const RESULT_RESET: &str = "coap_reset";
pub const RESULT_IGNORE: &str = "coap_ignore";

/// CoAP server protocol handler.
#[derive(Default)]
pub struct CoapProtocol;

impl CoapProtocol {
    pub fn new() -> Self {
        Self
    }
}

impl Protocol for CoapProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // A plain CoAP server only answers requests. Unsolicited notifications belong to
        // Observe (RFC 7641), which is not implemented — see src/server/coap/CLAUDE.md.
        vec![]
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_coap_response_action(),
            send_coap_reset_action(),
            ignore_coap_request_action(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "CoAP"
    }

    fn get_event_types(&self) -> Vec<EventType> {
        get_coap_event_types()
    }

    fn stack_name(&self) -> &'static str {
        "ETH>IP>UDP>COAP"
    }

    fn keywords(&self) -> Vec<&'static str> {
        // "coap" and multi-word phrases only; nothing generic like "iot" or "sensor",
        // which would swallow requests meant for MQTT or Modbus.
        vec![
            "coap",
            "coap server",
            "constrained application protocol",
            "rfc 7252",
        ]
    }

    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .connectionless()
            // Beta: exercised against a real, independent client — coap-lite —
            // covering RFC 7252 message-layer encode/decode by an independent codec. Not Stable: Stable additionally wants spec
            // compliance and scripting support reviewed, which has not been done here.
            .state(DevelopmentState::Beta)
            // 5683 is above 1023; declaring PrivilegedPort here could never fire.
            .privilege_requirement(PrivilegeRequirement::None)
            .implementation(
                "Hand-rolled RFC 7252 codec (src/server/coap/codec.rs): 4-byte header, \
                 CON/NON/ACK/RST, tokens, option delta/length with both extension forms, \
                 payload marker, GET/POST/PUT/DELETE",
            )
            .llm_control(
                "Resource representations, media type and response code. Message type, \
                 message id and token echo are server-side",
            )
            .e2e_testing("coap-lite 0.13 codec and the coap 0.27 UDP client (independent)")
            .notes(
                "Validated against coap-lite 0.13, an independent codec, which decoded this \
                 server's replies and encoded the requests it answered: CON/ACK with token \
                 and message-id echo, Uri-Path and Uri-Query options, Content-Format, 2.05 \
                 and 4.04 response codes, NON requests, and the RST reply to a CoAP ping. \
                 Untested/not implemented: Observe (RFC 7641), Block-wise transfer \
                 (RFC 7959), DTLS/CoAPS on 5684, separate (non-piggybacked) responses, \
                 retransmission of Confirmable responses, and multicast",
            )
            .build()
    }

    fn description(&self) -> &'static str {
        "CoAP server impersonating a constrained IoT device (RFC 7252)"
    }

    fn example_prompt(&self) -> &'static str {
        "Pretend to be a soil moisture sensor via coap on port 5683; GET /sensors/moisture \
         returns JSON with a percentage, everything else is 4.04"
    }

    fn group_name(&self) -> &'static str {
        "Application"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        // Deterministic: answer every request with a fixed JSON sensor reading,
        // no LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "coap_request":
    actions = [{"type": "send_coap_response", "code": "2.05",
                "payload": '{"pct": 41.2}', "content_format": "application/json"}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: the model invents the resource tree and its representations.
            json!({
                "type": "open_server",
                "port": 5683,
                "base_stack": "coap",
                "instruction": "Act as a soil moisture sensor. GET /sensors/moisture returns \
                                2.05 with JSON like {\"pct\": 41.2}. GET /.well-known/core \
                                returns the link-format listing. Anything else is 4.04."
            }),
            // Script mode: deterministic routing, no LLM call per request.
            json!({
                "type": "open_server",
                "port": 5683,
                "base_stack": "coap",
                "event_handlers": [{
                    "event_pattern": "coap_request",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }]
            }),
            // Static mode: one fixed representation for every request.
            json!({
                "type": "open_server",
                "port": 5683,
                "base_stack": "coap",
                "event_handlers": [{
                    "event_pattern": "coap_request",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_coap_response",
                            "code": "2.05",
                            "payload": "{\"pct\": 41.2}",
                            "content_format": "application/json"
                        }]
                    }
                }]
            }),
        )
    }
}

impl Server for CoapProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            let listen_addr = ctx.legacy_listen_addr();
            super::CoapServer::spawn_with_llm_actions(
                listen_addr,
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
            "send_coap_response" => {
                let code_value = action
                    .get("code")
                    .context("send_coap_response requires a 'code', e.g. \"2.05\" or \"4.04\"")?;

                let code = if let Some(s) = code_value.as_str() {
                    codec::parse_code_string(s).ok_or_else(|| {
                        anyhow::anyhow!(
                            "send_coap_response 'code' {s:?} is not a CoAP response code. Write \
                             it as class.detail, e.g. \"2.05\" (Content), \"2.04\" (Changed), \
                             \"4.04\" (Not Found), \"5.03\" (Service Unavailable)."
                        )
                    })?
                } else {
                    anyhow::bail!(
                        "send_coap_response 'code' must be a string such as \"2.05\", got \
                         {code_value}"
                    );
                };

                let class = codec::code_class(code);
                if !(2..=5).contains(&class) || class == 3 {
                    anyhow::bail!(
                        "send_coap_response 'code' {} is not a response code: class must be 2 \
                         (success), 4 (client error) or 5 (server error). Class 0 is a request \
                         method, and 1/3/6/7 are reserved.",
                        codec::code_to_string(code)
                    );
                }

                let payload = decode_payload(&action)?;

                // Content-Format: explicit wins; otherwise text/plain when there is a
                // payload at all, so a client is never handed bytes with no media type.
                let content_format = match action.get("content_format") {
                    Some(serde_json::Value::String(s)) => {
                        Some(codec::content_format_id(s).ok_or_else(|| {
                            anyhow::anyhow!(
                                "send_coap_response 'content_format' {s:?} is not a CoAP media \
                                 type. Use \"text/plain\", \"application/json\", \
                                 \"application/cbor\", \"application/xml\", \
                                 \"application/octet-stream\" or \
                                 \"application/link-format\", or the numeric identifier."
                            )
                        })?)
                    }
                    Some(serde_json::Value::Number(n)) => {
                        let id = n.as_u64().unwrap_or(u64::MAX);
                        if id > u16::MAX as u64 {
                            anyhow::bail!(
                                "send_coap_response 'content_format' {id} is not a valid \
                                 Content-Format identifier"
                            );
                        }
                        Some(id as u16)
                    }
                    Some(serde_json::Value::Null) | None => {
                        if payload.is_empty() {
                            None
                        } else {
                            Some(0) // text/plain;charset=utf-8
                        }
                    }
                    Some(other) => anyhow::bail!(
                        "send_coap_response 'content_format' must be a media type string or a \
                         numeric identifier, got {other}"
                    ),
                };

                Ok(ActionResult::Custom {
                    name: RESULT_RESPONSE.to_string(),
                    data: json!({
                        "code": code,
                        "payload_hex": hex::encode(&payload),
                        "content_format": content_format,
                    }),
                })
            }
            "send_coap_reset" => Ok(ActionResult::Custom {
                name: RESULT_RESET.to_string(),
                data: json!({}),
            }),
            "ignore_coap_request" => Ok(ActionResult::Custom {
                name: RESULT_IGNORE.to_string(),
                data: json!({}),
            }),
            _ => Err(anyhow::anyhow!("Unknown CoAP action: {action_type}")),
        }
    }
}

/// Turn the action's `payload` into the exact bytes to put on the wire, honouring the
/// explicit `encoding` field.
///
/// There is deliberately no sniffing: `"48656c6c6f"` is both valid text and valid hex,
/// and only the sender knows which it means. A documented `encoding` that the executor
/// then ignores is the `send_tcp_data` bug; this function is the reason it cannot recur
/// here.
fn decode_payload(action: &serde_json::Value) -> Result<Vec<u8>> {
    let payload = match action.get("payload") {
        None | Some(serde_json::Value::Null) => return Ok(Vec::new()),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => anyhow::bail!(
            "send_coap_response 'payload' must be a string (use \"encoding\": \"hex\" for \
             binary), got {other}"
        ),
    };

    let encoding = action
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("utf8");

    let bytes = decode_with_encoding(&payload, encoding)?;

    if bytes.len() > codec::MAX_PAYLOAD_LEN {
        anyhow::bail!(
            "send_coap_response 'payload' is {} bytes, over the {}-byte limit. CoAP has no \
             fragmentation of its own and this server does not implement Block-wise transfer \
             (RFC 7959), so a representation has to fit one datagram: RFC 7252 §4.6 puts the \
             safe maximum at {} bytes when the path MTU is unknown. Return a smaller \
             representation - a summary, fewer records, or a link to the detail - rather than \
             a response no constrained client can receive.",
            bytes.len(),
            codec::MAX_PAYLOAD_LEN,
            codec::MAX_PAYLOAD_LEN
        );
    }

    Ok(bytes)
}

/// Turn the `payload` string into bytes according to `encoding`, with no size opinion.
fn decode_with_encoding(payload: &str, encoding: &str) -> Result<Vec<u8>> {
    let payload = payload.to_string();
    match encoding {
        "utf8" => Ok(payload.into_bytes()),
        "hex" => {
            let cleaned: String = payload
                .chars()
                .filter(|c| !c.is_ascii_whitespace() && *c != ':')
                .collect();
            let cleaned = cleaned.strip_prefix("0x").unwrap_or(&cleaned);
            if cleaned.len() % 2 != 0 {
                anyhow::bail!(
                    "Invalid hex in 'payload': expected an even number of hex digits, got {} \
                     ({payload:?}). Each byte is two hex digits.",
                    cleaned.len()
                );
            }
            hex::decode(cleaned).map_err(|e| {
                anyhow::anyhow!(
                    "Invalid hex in 'payload' ({payload:?}): {e}. Use only 0-9/a-f, two digits \
                     per byte. To send this string as literal text, omit 'encoding' or set it \
                     to \"utf8\"."
                )
            })
        }
        other => anyhow::bail!(
            "Invalid 'encoding' value {other:?}. Valid values are \"utf8\" (default, send the \
             characters of 'payload' as-is) and \"hex\" (decode 'payload' as hex-encoded \
             bytes)."
        ),
    }
}

// ===========================================================================
// Action definitions
// ===========================================================================

fn send_coap_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_coap_response".to_string(),
        description: "Answer a CoAP request. You decide the resource's representation and which \
             response code it deserves: 2.05 Content for a successful GET, 2.01 Created / \
             2.04 Changed for a POST or PUT, 2.02 Deleted for a DELETE, 4.04 Not Found for a \
             resource this device does not have, 4.05 Method Not Allowed, 4.00 Bad Request, \
             5.03 Service Unavailable. Invent representations that are plausible for the \
             device you are impersonating and keep them consistent across requests with \
             set_memory. The message type of the reply (ACK for a CON request, NON for a NON \
             request), the message id and the token echo are handled for you."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "code".to_string(),
                type_hint: "string".to_string(),
                description:
                    "CoAP response code written as class.detail, e.g. \"2.05\", \"2.04\", \
                     \"4.04\", \"5.03\". Class must be 2, 4 or 5."
                        .to_string(),
                required: true,
            },
            Parameter {
                name: "payload".to_string(),
                type_hint: "string".to_string(),
                description: "Representation to return. Interpreted according to 'encoding': by \
                     default the characters of this string are sent as UTF-8. Omit for a \
                     response with no body (a DELETE confirmation, a 4.04)."
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "encoding".to_string(),
                type_hint: "string".to_string(),
                description:
                    "How to turn 'payload' into bytes. \"utf8\" (the default when omitted) \
                     sends the characters unchanged - use it for text, JSON and XML. \"hex\" \
                     decodes 'payload' as hex-encoded bytes, two hex digits per byte - use it \
                     for CBOR or any other binary representation, e.g. {\"payload\": \
                     \"a1636f6e f5\", \"encoding\": \"hex\"}. There is no auto-detection."
                        .to_string(),
                required: false,
            },
            Parameter {
                name: "content_format".to_string(),
                type_hint: "string".to_string(),
                description: "Media type of 'payload': \"text/plain\", \"application/json\", \
                     \"application/cbor\", \"application/xml\", \"application/octet-stream\", \
                     \"application/link-format\", or the numeric CoAP identifier. Defaults to \
                     text/plain when a payload is present and this is omitted."
                    .to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_coap_response",
            "code": "2.05",
            "payload": "{\"pct\": 41.2}",
            "content_format": "application/json"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> {code}")
                .with_debug("CoAP response {code} ({output_bytes}B)")
                .with_trace("CoAP response: {preview(payload,200)}"),
        ),
    }
}

fn send_coap_reset_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_coap_reset".to_string(),
        description:
            "Reject the message with a CoAP Reset (RST). Use this when the device cannot make \
             any sense of the request at all - not as a way to say 'not found', which is \
             4.04. A Reset tells the client to stop retransmitting and that no response is \
             coming."
                .to_string(),
        parameters: vec![],
        example: json!({ "type": "send_coap_reset" }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> RST")
                .with_debug("CoAP reset"),
        ),
    }
}

fn ignore_coap_request_action() -> ActionDefinition {
    ActionDefinition {
        name: "ignore_coap_request".to_string(),
        description:
            "Send nothing at all. Appropriate for a Non-confirmable request the device would \
             not answer, or to model a sleeping or unreachable node. A Confirmable client \
             will retransmit and eventually time out, which is the behaviour you are asking \
             for when you choose this."
                .to_string(),
        parameters: vec![],
        example: json!({ "type": "ignore_coap_request" }),
        log_template: Some(LogTemplate::new().with_debug("CoAP request deliberately unanswered")),
    }
}

pub static SEND_COAP_RESPONSE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(send_coap_response_action);
pub static SEND_COAP_RESET_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(send_coap_reset_action);
pub static IGNORE_COAP_REQUEST_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(ignore_coap_request_action);

// ===========================================================================
// Event types
// ===========================================================================

/// A GET/POST/PUT/DELETE arrived. This is the only event this protocol declares, and
/// `src/server/coap/mod.rs` emits it for every well-formed request.
pub static COAP_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "coap_request",
        "A CoAP client is requesting a resource. Decide what this device holds at that path \
         and answer with a representation and a response code, or refuse.",
        json!({
            "type": "send_coap_response",
            "code": "2.05",
            "payload": "{\"pct\": 41.2}",
            "content_format": "application/json"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "method".to_string(),
            type_hint: "string".to_string(),
            description: "GET, POST, PUT or DELETE".to_string(),
            required: true,
        },
        Parameter {
            name: "path".to_string(),
            type_hint: "string".to_string(),
            description: "Requested path assembled from the Uri-Path options, e.g. \
                 \"/sensors/moisture\". \"/\" when no Uri-Path option was sent."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "path_segments".to_string(),
            type_hint: "array of strings".to_string(),
            description: "The Uri-Path options as individual segments".to_string(),
            required: true,
        },
        Parameter {
            name: "query".to_string(),
            type_hint: "string".to_string(),
            description:
                "Uri-Query options joined with '&', e.g. \"unit=C&since=60\". Absent when the \
                 request carried no query."
                    .to_string(),
            required: false,
        },
        Parameter {
            name: "message_type".to_string(),
            type_hint: "string".to_string(),
            description:
                "\"CON\" for a Confirmable request (the client expects an acknowledgement and \
                 will retransmit without one) or \"NON\" for a Non-confirmable one. The reply \
                 is matched to it automatically; you do not need to act on this."
                    .to_string(),
            required: true,
        },
        Parameter {
            name: "message_id".to_string(),
            type_hint: "integer".to_string(),
            description: "The client's message id. Echoed back automatically - informational only."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "content_format".to_string(),
            type_hint: "string".to_string(),
            description: "Media type of the request payload, when the client declared one"
                .to_string(),
            required: false,
        },
        Parameter {
            name: "accept".to_string(),
            type_hint: "string".to_string(),
            description: "Media type the client asked for in the Accept option, when it sent one. \
                 Prefer it when you can represent the resource that way."
                .to_string(),
            required: false,
        },
        Parameter {
            name: "payload".to_string(),
            type_hint: "string".to_string(),
            description:
                "Request body, for POST and PUT. Read it according to 'payload_encoding'. \
                 Absent when the request carried no body."
                    .to_string(),
            required: false,
        },
        Parameter {
            name: "payload_encoding".to_string(),
            type_hint: "string".to_string(),
            description:
                "How to read 'payload': \"utf8\" means it is the received bytes as literal \
                 text, \"hex\" means it is the received bytes hex-encoded (used whenever they \
                 are not all printable)."
                    .to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        SEND_COAP_RESPONSE_ACTION.clone(),
        SEND_COAP_RESET_ACTION.clone(),
        IGNORE_COAP_REQUEST_ACTION.clone(),
    ])
    .with_alternative_example(json!({
        "type": "send_coap_response",
        "code": "4.04"
    }))
    .with_alternative_example(json!({ "type": "send_coap_reset" }))
    .with_log_template(
        LogTemplate::new()
            .with_info("{client_ip}:{client_port} {method} {path}")
            .with_debug("CoAP {message_type} {method} {path} mid={message_id}")
            .with_trace("CoAP request: {json_pretty(.)}"),
    )
});

/// All CoAP event types. The single entry is emitted for every well-formed request.
pub fn get_coap_event_types() -> Vec<EventType> {
    vec![COAP_REQUEST_EVENT.clone()]
}
