//! What the model does as a libp2p dialler: open streams on the remote's protocols, send and
//! close them, ping and re-identify the remote, disconnect. Messages the remote sends (on
//! streams either side opened) come back as events.
use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, ParameterDefinition, StartupExamples,
};
use crate::protocol::{ConnectContext, EventType};
use crate::server::libp2p::{
    actions::{action, p},
    host, wire,
};
use crate::state::app_state::AppState;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;

#[derive(Default)]
pub struct Libp2pClientProtocol;
impl Libp2pClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

fn data(required: bool) -> Vec<crate::llm::actions::Parameter> {
    vec![
        p(
            "data",
            "string",
            "The message: text, or hex with encoding hex",
            required,
        ),
        p("encoding", "string", "utf8 (default) or hex", false),
    ]
}

pub fn actions() -> Vec<ActionDefinition> {
    let mut open = vec![p(
        "protocol",
        "string",
        "The protocol id to open, e.g. /netget/chat/1.0.0",
        true,
    )];
    open.extend(data(false));
    let mut send = vec![p(
        "stream_id",
        "number",
        "The stream to write on (from libp2p_response or libp2p_message)",
        true,
    )];
    send.extend(data(true));
    vec![
        action(
            "libp2p_open_stream",
            "Open a stream on a protocol the remote supports, optionally with a first message.",
            open,
            json!({"type": "libp2p_open_stream", "protocol": "/netget/chat/1.0.0", "data": "hello"}),
        ),
        action(
            "libp2p_send",
            "Send one length-prefixed message on an open stream.",
            send,
            json!({"type": "libp2p_send", "stream_id": 1, "data": "hello again"}),
        ),
        action(
            "libp2p_close_stream",
            "Close our half of a stream; the remote reads end-of-stream.",
            vec![p(
                "stream_id",
                "number",
                "The stream to close, by its id",
                true,
            )],
            json!({"type": "libp2p_close_stream", "stream_id": 1}),
        ),
        action(
            "libp2p_ping",
            "Ping the remote (/ipfs/ping/1.0.0) and learn the round-trip time.",
            vec![],
            json!({"type": "libp2p_ping"}),
        ),
        action(
            "libp2p_identify",
            "Ask the remote's identify again: agent, protocols and addresses.",
            vec![],
            json!({"type": "libp2p_identify"}),
        ),
        action(
            "disconnect",
            "Close the connection.",
            vec![],
            json!({"type": "disconnect"}),
        ),
    ]
}

fn event(id: &str, description: &str, params: Vec<crate::llm::actions::Parameter>) -> EventType {
    EventType::new(
        id,
        description,
        json!({"type": "libp2p_open_stream", "protocol": "/netget/chat/1.0.0", "data": "hello"}),
    )
    .with_parameters(params)
    .with_actions(actions())
}

pub static CONNECTED_EVENT: LazyLock<EventType> =
    LazyLock::new(|| {
        event(
        "libp2p_connected",
        "Connected: the remote proved its peer id in the Noise handshake and answered identify.",
        vec![
            p("peer_id", "string", "The remote's peer id", true),
            p("agent_version", "string", "What the remote says it is, e.g. go-libp2p", true),
            p("protocols", "array", "The protocols the remote supports", true),
            p("listen_addrs", "array", "Where the remote says it listens", true),
            p("observed_addr", "string", "This client's address as the remote sees it", false),
        ],
    )
    });

pub static MESSAGE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    event(
        "libp2p_message",
        "The remote sent a message, on a stream either side opened.",
        vec![
            p(
                "stream_id",
                "number",
                "The stream it arrived on (use it with libp2p_send)",
                true,
            ),
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
        ],
    )
});

pub static RESPONSE_EVENT: LazyLock<EventType> =
    LazyLock::new(|| {
        event(
        "libp2p_response",
        "The outcome of an action that asks something of the remote (open_stream, ping, identify).",
        vec![
            p("operation", "string", "The action that was performed", true),
            p("ok", "boolean", "Whether it succeeded", true),
            p("stream_id", "number", "The new stream, for open_stream", false),
            p("rtt_ms", "number", "The round trip, for ping", false),
            p("result", "object", "The remote's identify, for identify", false),
            p("error", "string", "Why it failed, e.g. the remote refused the protocol", false),
        ],
    )
    });

pub fn check(v: &Value) -> Result<()> {
    let stream = || {
        v["stream_id"]
            .as_u64()
            .context("stream_id must be a number")
    };
    match v["type"].as_str().unwrap_or_default() {
        "libp2p_open_stream" => {
            let proto = v["protocol"].as_str().context("protocol is required")?;
            ensure!(
                proto.starts_with('/') && proto.len() <= 256 && !proto.contains('\n'),
                "protocol must be a protocol id such as /netget/chat/1.0.0"
            );
            if !v["data"].is_null() {
                wire::data_bytes(v)?;
            }
        }
        "libp2p_send" => {
            stream()?;
            ensure!(
                wire::data_bytes(v)?.len() <= host::MAX_MESSAGE,
                "message too long"
            );
        }
        "libp2p_close_stream" => {
            stream()?;
        }
        "libp2p_ping" | "libp2p_identify" | "disconnect" => {}
        t => bail!("Unknown libp2p client action {t:?}"),
    }
    Ok(())
}

impl Protocol for Libp2pClientProtocol {
    fn protocol_name(&self) -> &'static str {
        "libp2p"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>libp2p"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["libp2p", "ipfs", "p2p", "dial peer", "multiaddr"]
    }
    fn description(&self) -> &'static str {
        "libp2p dialler over TCP (Noise, yamux): identifies and pings the remote, and talks length-prefixed messages on its protocols' streams"
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        actions()
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            CONNECTED_EVENT.clone(),
            MESSAGE_EVENT.clone(),
            RESPONSE_EVENT.clone(),
        ]
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "peer_id".into(),
                type_hint: "string".into(),
                description: "The remote's expected peer id (12D3KooW…); the handshake fails if it proves another. Also accepted inside a /p2p/ multiaddr.".into(),
                required: false,
                example: json!("12D3KooWD3eckifWpRn9wQpMG9R9hX3sD158z7EqHWmweQAJU5SA"),
                default: None,
            },
            ParameterDefinition {
                name: "protocols".into(),
                type_hint: "array".into(),
                description: "Application protocol ids the remote may open streams to this client on".into(),
                required: false,
                example: json!(["/netget/chat/1.0.0"]),
                default: Some(json!(host::DEFAULT_PROTOCOLS)),
            },
            ParameterDefinition {
                name: "private_key_seed".into(),
                type_hint: "string".into(),
                description: "Any text; its SHA-256 is the Ed25519 identity key, so the same text gives the same peer id (default: a new key)".into(),
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
            .implementation("The server's hand-rolled stack in the dialler role: multistream-select, Noise XX as initiator (checking the remote's peer id when one is named), yamux, identify and ping; the remote's own identify and ping are answered")
            .llm_control("Which protocols to open streams on, what to send, when to ping, identify, close and disconnect, and how to answer the remote's messages")
            .e2e_testing("tests/client/libp2p: go-libp2p listening (TCP, Noise, yamux), which identifies NetGet, answers its chat stream and opens one of its own to NetGet")
            .notes("TCP only; Ed25519 identities only; no DHT, gossipsub, relay or hole punching. remote_addr is host:port or /ip4|ip6|dns/…/tcp/…[/p2p/<id>]. A handler chain stops after 8 follow-ups.")
            .build()
    }
    fn example_prompt(&self) -> &'static str {
        "Dial the libp2p peer /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... and say hello on /netget/chat/1.0.0"
    }
    fn get_startup_examples(&self) -> StartupExamples {
        let llm = json!({"type":"open_client","protocol":"libp2p","remote_addr":"127.0.0.1:4001",
            "instruction":"Say hello on /netget/chat/1.0.0 and answer whatever comes back"});
        let mut static_example = llm.clone();
        static_example["event_handlers"] = json!([{"event_pattern":"libp2p_connected","handler":{"type":"static","actions":[{"type":"libp2p_open_stream","protocol":"/netget/chat/1.0.0","data":"hello"}]}}]);
        let mut scripted = llm.clone();
        scripted["event_handlers"] = json!([{"event_pattern":"*","handler":{"type":"script","language":"python","code":"import json,sys\ni=json.load(sys.stdin); t=i['event_type_id']; e=i['event']\na=[]\nif t=='libp2p_connected': a=[{'type':'libp2p_open_stream','protocol':'/netget/chat/1.0.0','data':'hello'}]\nprint(json.dumps({'actions':a}))"}}]);
        StartupExamples::new(llm, scripted, static_example)
    }
    fn group_name(&self) -> &'static str {
        "Peer-to-peer"
    }
}

impl Client for Libp2pClientProtocol {
    fn connect(
        &self,
        ctx: ConnectContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::connect(ctx))
    }
    fn execute_action(&self, v: Value) -> Result<ClientActionResult> {
        let name = v["type"].as_str().unwrap_or_default().to_string();
        if name == "disconnect" {
            return Ok(ClientActionResult::Disconnect);
        }
        check(&v)?;
        Ok(ClientActionResult::Custom { name, data: v })
    }
}
