//! SIP protocol actions implementation

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

pub struct SipProtocol;

impl SipProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SipProtocol {
    /// None. SIP has no user-triggered action that does anything.
    ///
    /// `send_sip_invite`, `send_sip_bye` and `update_registration` used to be advertised
    /// here. All three were `Ok(ActionResult::NoAction)` stubs — every parameter they
    /// declared (`to`, `from`, `sdp`, `call_id`, `bindings`) was read by nothing, and neither
    /// the action names nor those fields appear anywhere in `mod.rs`. So a user asking to
    /// place or end a call got silence, and the model was told the capability existed.
    ///
    /// Implementing them needs outbound dialog state this server does not keep (it answers
    /// requests; it does not originate them) and a registration database that outlives a
    /// request. Removed rather than left advertised, as the proxy's six configuration actions
    /// and `ssh_agent/modify_instruction` were: an action that silently does nothing is worse
    /// than an absent one, because the model will choose it.
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            sip_register_action(),
            sip_invite_action(),
            sip_bye_action(),
            sip_ack_action(),
            sip_options_action(),
            sip_cancel_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "SIP"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_sip_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>UDP>SIP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["sip", "voip", "session initiation"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .connectionless()
            .state(DevelopmentState::Experimental)
            // Not rsipstack, and not a compliant stack. `Cargo.toml` declares `sip = []` -
            // there is no SIP dependency at all. `mod.rs` hand-parses the request line and
            // headers and hand-builds the status line. The old claim named a library version
            // this build has never linked, which is exactly the sort of thing an operator
            // reads as evidence of maturity.
            .implementation(
                "Manual line-based parser and response builder (no SIP library; Cargo declares \
                 `sip = []`). Request line + headers + optional SDP body only: no transaction \
                 layer, no retransmissions, no digest auth, no dialog state, UDP only",
            )
            .llm_control("Registration decisions + call routing + SDP generation")
            // No `rvoip-sip-client` exists in this tree. The evidence is a mocked-LLM UDP
            // exchange plus netget's own SIP client - which makes it circular, and is why this
            // stays Experimental rather than Beta.
            .e2e_testing(
                "Mocked-LLM UDP e2e over the six methods (tests/server/sip/e2e_test.rs), a \
                 fail-closed 503/ACK-silence test, and an RTP interop test. No third-party SIP \
                 client (the client side of tests/client/sip is netget's own), so this is not \
                 independent-client evidence",
            )
            .notes(
                "Scripting candidate, VoIP signaling honeypot. No registration database: the \
                 model decides each REGISTER on its own and nothing is stored between requests \
                 (protocols must not implement storage). REGISTER and INVITE are admission \
                 decisions and fail closed - an LLM error is 503, an action with no \
                 status_code or no SIP response action at all is 500, never a defaulted 200",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "SIP server for VoIP signaling"
    }
    fn example_prompt(&self) -> &'static str {
        "Start a SIP server on port 5060 for VoIP registration and call signaling"
    }
    fn group_name(&self) -> &'static str {
        "Proxy & Network"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        // Deterministic: answer REGISTER, INVITE and OPTIONS each with 200 OK,
        // no LLM call. One script handles all three events.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
et = data["event_type_id"]
if et == "sip_register":
    actions = [{"type": "sip_register", "status_code": 200}]
elif et == "sip_invite":
    actions = [{"type": "sip_invite", "status_code": 200}]
elif et == "sip_options":
    actions = [{"type": "sip_options", "status_code": 200}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode
            json!({
                "type": "open_server",
                "port": 5060,
                "base_stack": "sip",
                "instruction": "SIP VoIP signaling server. Accept REGISTER requests with 200 OK. For INVITE requests, respond with 200 OK and SDP. Log all call setup attempts."
            }),
            // Script mode
            json!({
                "type": "open_server",
                "port": 5060,
                "base_stack": "sip",
                "event_handlers": [{
                    "event_pattern": "sip_register",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }, {
                    "event_pattern": "sip_invite",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }, {
                    "event_pattern": "sip_options",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }]
            }),
            // Static mode
            json!({
                "type": "open_server",
                "port": 5060,
                "base_stack": "sip",
                "event_handlers": [{
                    "event_pattern": "sip_register",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "sip_register",
                            "status_code": 200,
                            "expires": 3600
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for SipProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::sip::SipServer;
            SipServer::spawn_with_llm_actions(
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
            "sip_register" => self.execute_sip_register(action),
            "sip_invite" => self.execute_sip_invite(action),
            "sip_bye" => self.execute_sip_bye(action),
            "sip_ack" => Ok(ActionResult::NoAction), // ACK doesn't require response
            "sip_options" => self.execute_sip_options(action),
            "sip_cancel" => self.execute_sip_cancel(action),
            _ => Err(anyhow::anyhow!("Unknown SIP action: {}", action_type)),
        }
    }
}

impl SipProtocol {
    /// Execute SIP REGISTER action (network event)
    /// Just validates the action - actual response data comes from action JSON itself
    fn execute_sip_register(&self, _action: serde_json::Value) -> Result<ActionResult> {
        // Validation happens in mod.rs when parsing the action
        Ok(ActionResult::NoAction)
    }

    /// Execute SIP INVITE action (network event)
    fn execute_sip_invite(&self, _action: serde_json::Value) -> Result<ActionResult> {
        Ok(ActionResult::NoAction)
    }

    /// Execute SIP BYE action (network event)
    fn execute_sip_bye(&self, _action: serde_json::Value) -> Result<ActionResult> {
        Ok(ActionResult::NoAction)
    }

    /// Execute SIP OPTIONS action (network event)
    fn execute_sip_options(&self, _action: serde_json::Value) -> Result<ActionResult> {
        Ok(ActionResult::NoAction)
    }

    /// Execute SIP CANCEL action (network event)
    fn execute_sip_cancel(&self, _action: serde_json::Value) -> Result<ActionResult> {
        Ok(ActionResult::NoAction)
    }
}

/// SIP event types
fn get_sip_event_types() -> Vec<EventType> {
    vec![
        SIP_REGISTER_EVENT.clone(),
        SIP_INVITE_EVENT.clone(),
        SIP_BYE_EVENT.clone(),
        SIP_ACK_EVENT.clone(),
        SIP_OPTIONS_EVENT.clone(),
        SIP_CANCEL_EVENT.clone(),
    ]
}

/// SIP REGISTER event
pub static SIP_REGISTER_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sip_register",
        "A client is registering its location with REGISTER. Answer with sip_register: 200 to \
         accept the binding, 401/403 to challenge or refuse it.",
        json!({
            "type": "sip_register",
            "status_code": 200,
            "expires": 3600
        }),
    )
    // Each SIP event is answered by exactly one action - the reply to a REGISTER is a REGISTER
    // response and nothing else - so every event lists only its own. `call_llm` builds the
    // model's tool list from the event type rather than from get_sync_actions(), so before this
    // the model was offered none of the six and every reply it produced was rejected.
    .with_actions(vec![sip_register_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SIP REGISTER")
            .with_debug("SIP REGISTER request")
            .with_trace("SIP: {json_pretty(.)}"),
    )
});

/// SIP INVITE event
pub static SIP_INVITE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sip_invite",
        "A client is initiating a session with INVITE. Answer with sip_invite carrying the status \
         code and, for a 200, the answering SDP.",
        json!({
            "type": "sip_invite",
            "status_code": 200,
            "sdp": "v=0\no=- 0 0 IN IP4 127.0.0.1\ns=Call\nc=IN IP4 127.0.0.1\nt=0 0\nm=audio 8000 RTP/AVP 0\n"
        }),
    )
    .with_actions(vec![sip_invite_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SIP INVITE")
            .with_debug("SIP INVITE request")
            .with_trace("SIP: {json_pretty(.)}"),
    )
});

/// SIP BYE event
pub static SIP_BYE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sip_bye",
        "The far end is terminating the session with BYE. Answer with sip_bye; 200 acknowledges \
         the teardown.",
        json!({"type": "sip_bye", "status_code": 200}),
    )
    .with_actions(vec![sip_bye_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SIP BYE")
            .with_debug("SIP BYE request")
            .with_trace("SIP: {json_pretty(.)}"),
    )
});

/// SIP ACK event
pub static SIP_ACK_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sip_ack",
        "The client acknowledged an INVITE response. ACK is never answered on the wire, so \
         sip_ack simply records that the dialog is established.",
        json!({"type": "sip_ack"}),
    )
    .with_actions(vec![sip_ack_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SIP ACK")
            .with_debug("SIP ACK request")
            .with_trace("SIP: {json_pretty(.)}"),
    )
});

/// SIP OPTIONS event
pub static SIP_OPTIONS_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sip_options",
        "A client is querying this server's capabilities with OPTIONS. Answer with sip_options \
         listing the methods you support.",
        json!({
            "type": "sip_options",
            "status_code": 200,
            "allow_methods": ["INVITE", "ACK", "BYE", "REGISTER", "OPTIONS"]
        }),
    )
    .with_actions(vec![sip_options_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SIP OPTIONS")
            .with_debug("SIP OPTIONS request")
            .with_trace("SIP: {json_pretty(.)}"),
    )
});

/// SIP CANCEL event
pub static SIP_CANCEL_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "sip_cancel",
        "The client is cancelling a pending INVITE. Answer with sip_cancel; 200 confirms the \
         cancellation.",
        json!({"type": "sip_cancel", "status_code": 200}),
    )
    .with_actions(vec![sip_cancel_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("SIP CANCEL")
            .with_debug("SIP CANCEL request")
            .with_trace("SIP: {json_pretty(.)}"),
    )
});

// Action definitions
fn sip_register_action() -> ActionDefinition {
    ActionDefinition {
        name: "sip_register".to_string(),
        description: "Respond to SIP REGISTER request".to_string(),
        parameters: vec![
            Parameter {
                name: "status_code".to_string(),
                type_hint: "number".to_string(),
                description: "SIP status code (200=OK, 403=Forbidden, 401=Unauthorized)"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "reason_phrase".to_string(),
                type_hint: "string".to_string(),
                description: "Optional reason phrase (default based on status code)".to_string(),
                required: false,
            },
            Parameter {
                name: "expires".to_string(),
                type_hint: "number".to_string(),
                description: "Registration expiration in seconds (default 3600)".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "sip_register",
            "status_code": 200,
            "expires": 3600
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SIP {status_code} REGISTER")
                .with_debug("SIP sip_register: status={status_code}, expires={expires}"),
        ),
    }
}

fn sip_invite_action() -> ActionDefinition {
    ActionDefinition {
        name: "sip_invite".to_string(),
        description: "Respond to SIP INVITE request".to_string(),
        parameters: vec![
            Parameter {
                name: "status_code".to_string(),
                type_hint: "number".to_string(),
                description: "SIP status code (200=OK, 486=Busy, 603=Decline, 180=Ringing)"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "reason_phrase".to_string(),
                type_hint: "string".to_string(),
                description: "Optional reason phrase".to_string(),
                required: false,
            },
            Parameter {
                name: "sdp".to_string(),
                type_hint: "string".to_string(),
                description: "Session Description Protocol body (required for 200 OK)".to_string(),
                required: false,
            },
            Parameter {
                name: "rtp_audio".to_string(),
                type_hint: "object".to_string(),
                description: "Optional media to actually stream on a 200 OK: {content, tone_hz, \
                              payload_type, duration_ms}. When NetGet is built with the `rtp` \
                              feature, accepting a call streams this as real RTP to the m=audio \
                              target in the caller's INVITE SDP, so the negotiated session carries \
                              media rather than only promising it. Ignored without the `rtp` feature."
                    .to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "sip_invite",
            "status_code": 200,
            "sdp": "v=0\no=- 0 0 IN IP4 127.0.0.1\ns=Call\nc=IN IP4 127.0.0.1\nt=0 0\nm=audio 8000 RTP/AVP 0\n",
            "rtp_audio": {"content": "tone", "tone_hz": 440, "payload_type": "pcmu", "duration_ms": 2000}
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SIP {status_code} INVITE")
                .with_debug("SIP sip_invite: status={status_code}"),
        ),
    }
}

fn sip_bye_action() -> ActionDefinition {
    ActionDefinition {
        name: "sip_bye".to_string(),
        description: "Respond to SIP BYE request".to_string(),
        parameters: vec![Parameter {
            name: "status_code".to_string(),
            type_hint: "number".to_string(),
            // Required, and there is no default. `build_sip_response` answers 500 when the
            // field is absent rather than defaulting to 200 - a defaulted 200 would turn a
            // forgotten field into an acceptance. The description used to promise a 200
            // default the executor has never applied.
            description: "SIP status code; 200 acknowledges the teardown. Required - an action \
                          with no status_code is answered 500, not 200"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "sip_bye",
            "status_code": 200
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SIP {status_code} BYE")
                .with_debug("SIP sip_bye: status={status_code}"),
        ),
    }
}

fn sip_ack_action() -> ActionDefinition {
    ActionDefinition {
        name: "sip_ack".to_string(),
        description: "Process SIP ACK (no response needed)".to_string(),
        parameters: vec![],
        example: json!({
            "type": "sip_ack"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SIP ACK processed")
                .with_debug("SIP sip_ack"),
        ),
    }
}

fn sip_options_action() -> ActionDefinition {
    ActionDefinition {
        name: "sip_options".to_string(),
        description: "Respond to SIP OPTIONS request".to_string(),
        parameters: vec![
            Parameter {
                name: "status_code".to_string(),
                type_hint: "number".to_string(),
                description: "SIP status code; 200 answers the capability query. Required - an \
                              action with no status_code is answered 500, not 200"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "allow_methods".to_string(),
                type_hint: "array".to_string(),
                description: "Array of supported SIP methods".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "sip_options",
            "status_code": 200,
            "allow_methods": ["INVITE", "ACK", "BYE", "REGISTER", "OPTIONS"]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SIP {status_code} OPTIONS")
                .with_debug("SIP sip_options: status={status_code}, methods={allow_methods_len}"),
        ),
    }
}

fn sip_cancel_action() -> ActionDefinition {
    ActionDefinition {
        name: "sip_cancel".to_string(),
        description: "Respond to SIP CANCEL request".to_string(),
        parameters: vec![Parameter {
            name: "status_code".to_string(),
            type_hint: "number".to_string(),
            description: "SIP status code; 200 confirms the cancellation. Required - an action \
                          with no status_code is answered 500, not 200"
                .to_string(),
            required: true,
        }],
        example: json!({
            "type": "sip_cancel",
            "status_code": 200
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SIP {status_code} CANCEL")
                .with_debug("SIP sip_cancel: status={status_code}"),
        ),
    }
}

// User-triggered actions
