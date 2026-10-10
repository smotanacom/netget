//! What the model does as a libp2p host: answer messages on the application protocols'
//! streams, open streams of its own to a connected peer, close them, and drop peers. Rust
//! owns the transport (Noise, yamux, multistream-select) and answers identify and ping.
use super::host;
use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition, StartupExamples,
};
use crate::protocol::{log_template::LogTemplate, EventType, SpawnContext};
use crate::state::app_state::AppState;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;

pub const SEND: &str = "libp2p_send";
pub const OPEN: &str = "libp2p_open_stream";
pub const CLOSE: &str = "libp2p_close_stream";
pub const DISCONNECT: &str = "libp2p_disconnect";

#[derive(Default, Clone)]
pub struct Libp2pProtocol;

impl Libp2pProtocol {
    pub fn new() -> Self {
        Self
    }
}

pub fn p(name: &str, type_hint: &str, description: &str, required: bool) -> Parameter {
    Parameter {
        name: name.into(),
        type_hint: type_hint.into(),
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
        log_template: Some(
            LogTemplate::new()
                .with_info(format!("-> libp2p {}", name.trim_start_matches("libp2p_"))),
        ),
    }
}

fn data_params() -> Vec<Parameter> {
    vec![
        p(
            "data",
            "string",
            "The message: text, or hex with encoding hex",
            true,
        ),
        p("encoding", "string", "utf8 (default) or hex", false),
    ]
}

pub fn send_action() -> ActionDefinition {
    let mut params = data_params();
    params.push(p(
        "stream_id",
        "number",
        "The stream to write on (default: the stream of the message being answered)",
        false,
    ));
    action(
        SEND,
        "Send one message on a stream (framed with its length, the libp2p convention).",
        params,
        json!({"type": SEND, "data": "hello from NetGet"}),
    )
}

pub fn open_action() -> ActionDefinition {
    let mut params = vec![p(
        "protocol",
        "string",
        "The protocol id to open, e.g. /netget/chat/1.0.0",
        true,
    )];
    params.extend(data_params().into_iter().map(|mut x| {
        x.required = false;
        x
    }));
    action(
        OPEN,
        "Open a stream to the peer on a protocol it supports, optionally sending a first message. Its replies arrive as libp2p_message events.",
        params,
        json!({"type": OPEN, "protocol": "/netget/chat/1.0.0", "data": "hi"}),
    )
}

pub fn close_action() -> ActionDefinition {
    action(
        CLOSE,
        "Close our half of a stream: the peer reads end-of-stream after what was sent.",
        vec![p(
            "stream_id",
            "number",
            "The stream to close (default: the stream of the message being answered)",
            false,
        )],
        json!({"type": CLOSE}),
    )
}

pub fn disconnect_action() -> ActionDefinition {
    action(
        DISCONNECT,
        "Close the whole connection to this peer.",
        vec![],
        json!({"type": DISCONNECT}),
    )
}

pub fn all_actions() -> Vec<ActionDefinition> {
    vec![
        send_action(),
        open_action(),
        close_action(),
        disconnect_action(),
    ]
}

pub static CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "libp2p_peer_connected",
        "A peer connected (Noise-authenticated, yamux-multiplexed) and answered identify. Open a stream to speak first, or do nothing and wait for its streams.",
        json!({"type": OPEN, "protocol": "/netget/chat/1.0.0", "data": "welcome"}),
    )
    .with_parameters(vec![
        p("peer_id", "string", "The peer's id, proven by its Noise handshake", true),
        p("remote_addr", "string", "The peer's address as a multiaddr", true),
        p("agent_version", "string", "What the peer's identify says it is, e.g. go-libp2p", true),
        p("protocols", "array", "The protocols the peer says it supports", true),
        p("listen_addrs", "array", "Where the peer says it listens", true),
    ])
    .with_actions(vec![open_action(), disconnect_action()])
});

pub static MESSAGE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "libp2p_message",
        "A peer sent a message on a stream of one of this host's application protocols.",
        json!({"type": SEND, "data": "hello back"}),
    )
    .with_parameters(vec![
        p("peer_id", "string", "The peer id of who sent it", true),
        p("stream_id", "number", "The stream it arrived on", true),
        p("protocol", "string", "The stream's protocol id", true),
        p(
            "data",
            "string",
            "The message (text, or hex when encoding is hex)",
            true,
        ),
        p(
            "encoding",
            "string",
            "How data is written: utf8 text, or hex for binary",
            true,
        ),
    ])
    .with_actions(all_actions())
});

pub fn check(v: &Value) -> Result<()> {
    match v["type"].as_str().unwrap_or_default() {
        SEND => {
            super::wire::data_bytes(v)?;
            if !v["stream_id"].is_null() {
                v["stream_id"]
                    .as_u64()
                    .context("stream_id must be a number")?;
            }
        }
        OPEN => {
            let proto = v["protocol"].as_str().context("protocol is required")?;
            ensure!(
                proto.starts_with('/') && proto.len() <= 256 && !proto.contains('\n'),
                "protocol must be a protocol id such as /netget/chat/1.0.0"
            );
            if !v["data"].is_null() {
                super::wire::data_bytes(v)?;
            }
        }
        CLOSE => {
            if !v["stream_id"].is_null() {
                v["stream_id"]
                    .as_u64()
                    .context("stream_id must be a number")?;
            }
        }
        DISCONNECT => {}
        other => bail!("Unknown libp2p action {other:?}"),
    }
    Ok(())
}

impl Protocol for Libp2pProtocol {
    fn protocol_name(&self) -> &'static str {
        "libp2p"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>libp2p"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "libp2p",
            "ipfs",
            "p2p",
            "noise",
            "yamux",
            "multistream",
            "peer id",
            "4001",
        ]
    }
    fn description(&self) -> &'static str {
        "libp2p host over TCP (multistream-select, Noise, yamux): answers identify and ping, and talks length-prefixed messages on its application protocols' streams"
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        Vec::new()
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        all_actions()
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![CONNECTED_EVENT.clone(), MESSAGE_EVENT.clone()]
    }
    fn default_binding(&self) -> Option<crate::protocol::BindingDefaults> {
        Some(crate::protocol::BindingDefaults::port_based("127.0.0.1", 0))
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "protocols".into(),
                type_hint: "array".into(),
                description: "Application protocol ids this host accepts streams for (identify and ping are always answered)".into(),
                required: false,
                example: json!(["/netget/chat/1.0.0", "/chat/1.0.0"]),
                default: Some(json!(host::DEFAULT_PROTOCOLS)),
            },
            ParameterDefinition {
                name: "private_key_seed".into(),
                type_hint: "string".into(),
                description: "Any text; its SHA-256 is the Ed25519 identity key, so the same text gives the same peer id (default: a new key each start)".into(),
                required: false,
                example: json!("my-stable-node"),
                default: None,
            },
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::*;
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::None)
            .well_known_port(4001)
            .implementation("Hand-rolled (src/server/libp2p): multistream-select 1.0; Noise XX (25519, ChaChaPoly, SHA256) with the libp2p handshake payload, on ring, x25519-dalek and ed25519-dalek; yamux with per-stream windows; identify (/ipfs/id/1.0.0, push accepted) and ping answered in Rust; Ed25519 peer ids")
            .llm_control("Every message on the application protocols' streams: reply, open streams to the peer, close them, drop the peer")
            .e2e_testing("tests/server/libp2p: go-libp2p (TCP, Noise, yamux) dials NetGet, verifies its peer id, runs identify and ping, and talks on /netget/chat/1.0.0; raw bytes for refusals and bounds")
            .notes("TCP only: no QUIC, WebTransport, WebRTC, TLS security or mplex. Ed25519 identities only (RSA/secp256k1/ECDSA peers are refused). No DHT, no gossipsub, no relay, no hole punching, no signed peer records. Messages are uvarint-length-prefixed, at most 1 MiB. A model failure resets the stream it was answering.")
            .max_inbound_bytes(host::MAX_MESSAGE)
            .build()
    }
    fn example_prompt(&self) -> &'static str {
        "libp2p peer on port 4001 that answers /netget/chat/1.0.0 messages"
    }
    fn get_startup_examples(&self) -> StartupExamples {
        let base = json!({"type":"open_server","base_stack":"libp2p","port":0,
            "instruction":"Answer every chat message with a short friendly reply"});
        let mut scripted = base.clone();
        scripted["event_handlers"] = json!([{"event_pattern":"libp2p_message","handler":{"type":"script","language":"python",
            "code":"import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'libp2p_send','data':'echo: '+e['data']}]}))"}},
            {"event_pattern":"*","handler":{"type":"static","actions":[]}}]);
        let mut static_example = base.clone();
        static_example["event_handlers"] = json!([{"event_pattern":"libp2p_message","handler":{"type":"static","actions":[{"type":SEND,"data":"hello"}]}},
            {"event_pattern":"*","handler":{"type":"static","actions":[]}}]);
        StartupExamples::new(base, scripted, static_example)
    }
    fn group_name(&self) -> &'static str {
        "Peer-to-peer"
    }
}

impl Server for Libp2pProtocol {
    fn spawn(
        &self,
        ctx: SpawnContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::spawn(ctx))
    }
    fn execute_action(&self, action: Value) -> Result<ActionResult> {
        check(&action)?;
        let name = action["type"].as_str().unwrap_or_default().to_string();
        Ok(ActionResult::Custom { name, data: action })
    }
}
