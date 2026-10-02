//! A deterministic echo server implemented against NetGet's public protocol API.
//! The embedding application must register this type; it is not a runtime-loaded plugin.

use anyhow::{Context, Result};
use netget::llm::actions::protocol_trait::{ActionResult, Protocol, Server};
use netget::llm::actions::{ActionDefinition, Parameter, StartupExamples};
use netget::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};
use netget::protocol::SpawnContext;
use netget::state::app_state::AppState;
use serde_json::{json, Value};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info};

/// Echoes each received chunk without consulting a model.
#[derive(Clone, Default)]
pub struct EchoProtocol;

impl EchoProtocol {
    pub fn new() -> Self {
        Self
    }
}

impl Protocol for EchoProtocol {
    fn protocol_name(&self) -> &'static str {
        "Echo"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>ECHO"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["echo", "echo protocol"]
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }
    fn description(&self) -> &'static str {
        "Deterministic TCP echo example; no model calls"
    }
    fn example_prompt(&self) -> &'static str {
        "Start an echo server on port 7777"
    }
    fn metadata(&self) -> ProtocolMetadataV2 {
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("Tokio TCP listener with registered listener and peer tasks")
            .llm_control("None: received bytes are echoed deterministically")
            .e2e_testing(
                "CPU-only loopback echo and stop test in tests/test_infrastructure_review_test.rs",
            )
            .notes("Example embedding API, not a built-in protocol or dynamically loaded plugin")
            .max_inbound_bytes(8192)
            .build()
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![ActionDefinition {
            name: "send_echo_data".into(),
            description: "Encode an echo response; the deterministic listener echoes directly"
                .into(),
            parameters: vec![Parameter {
                name: "data".into(),
                type_hint: "string".into(),
                description: "Data to echo back".into(),
                required: true,
            }],
            example: json!({"type": "send_echo_data", "data": "Hello, World!"}),
            log_template: None,
        }]
    }
    fn get_startup_examples(&self) -> StartupExamples {
        // Echo is deterministic in every mode: handler configuration is not
        // consumed by its listener. These describe its registration/startup shape.
        StartupExamples::new(
            json!({"type":"open_server", "base_stack":"echo", "port":7777, "instruction":"Echo received bytes"}),
            json!({"type":"open_server", "base_stack":"echo", "port":7777, "event_handlers":[]}),
            json!({"type":"open_server", "base_stack":"echo", "port":7777, "event_handlers":[]}),
        )
    }
}

impl Server for EchoProtocol {
    fn spawn(&self, ctx: SpawnContext) -> Pin<Box<dyn Future<Output = Result<SocketAddr>> + Send>> {
        Box::pin(async move {
            let address = ctx
                .socket_addr()
                .unwrap_or_else(|| ctx.legacy_listen_addr());
            let listener = TcpListener::bind(address).await?;
            let local_addr = listener.local_addr()?;
            info!("[ECHO] Server listening on {}", local_addr);
            let state = ctx.state.clone();
            let server_id = ctx.server_id;
            ctx.state
                .spawn_server_task(server_id, async move {
                    loop {
                        let (mut stream, peer_addr) = match listener.accept().await {
                            Ok(peer) => peer,
                            Err(error) => {
                                debug!("[ECHO] Accept error: {}", error);
                                break;
                            }
                        };
                        state
                            .spawn_server_task(server_id, async move {
                                let mut buffer = [0; 8192];
                                loop {
                                    match stream.read(&mut buffer).await {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            if stream.write_all(&buffer[..n]).await.is_err() {
                                                break;
                                            }
                                        }
                                        Err(error) => {
                                            debug!(
                                                "[ECHO] Read from {} failed: {}",
                                                peer_addr, error
                                            );
                                            break;
                                        }
                                    }
                                }
                            })
                            .await;
                    }
                })
                .await;
            Ok(local_addr)
        })
    }

    fn execute_action(&self, action: Value) -> Result<ActionResult> {
        let action_type = action["type"].as_str().context("Missing action type")?;
        match action_type {
            "send_echo_data" => Ok(ActionResult::Output(
                action["data"]
                    .as_str()
                    .context("Missing data parameter")?
                    .as_bytes()
                    .to_vec(),
            )),
            _ => anyhow::bail!("Unknown action type: {action_type}"),
        }
    }
}
