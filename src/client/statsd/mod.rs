//! StatsD emitter. The command loop owns the only socket and also handles initial actions.
pub mod actions;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::statsd::codec::{self, Dialect, DEFAULT_DIALECT};
use crate::state::{client_handles::ClientSendOutcome, AccessLogOwner, ClientStatus};
use crate::{console_error, console_info};
pub use actions::StatsdClientProtocol;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::net::UdpSocket;

pub struct StatsdClient;
impl StatsdClient {
    pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        let dialect = Dialect::parse(
            &ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_string("dialect"))
                .transpose()?
                .flatten()
                .unwrap_or_else(|| DEFAULT_DIALECT.into()),
        )?;
        let peer = tokio::net::lookup_host(&ctx.remote_addr)
            .await
            .context("resolve StatsD collector")?
            .next()
            .context("collector resolved to no addresses")?;
        let socket = UdpSocket::bind(if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        socket.connect(peer).await?;
        let local_addr = socket.local_addr()?;
        let mut commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        console_info!(
            ctx.status_tx,
            "StatsD emitter {} ready for {} ({})",
            ctx.client_id,
            peer,
            dialect.as_str()
        );
        let registrar = ctx.state.clone();
        let client_id = ctx.client_id;
        let handle = tokio::spawn(async move {
            let protocol = StatsdClientProtocol::new();
            let connected = async {
                let event = Event::new(
                    &actions::STATSD_CONNECTED_EVENT,
                    json!({"remote_addr":peer.to_string(),"local_addr":local_addr.to_string(),"dialect":dialect.as_str()}),
                );
                let instruction = ctx
                    .state
                    .get_instruction_for_client(client_id)
                    .await
                    .unwrap_or_default();
                let configured = ctx
                    .state
                    .get_client_event_handler_config(client_id)
                    .await
                    .is_some_and(|c| c.find_handler("statsd_connected").is_some());
                if instruction.is_empty() && !configured {
                    return Ok(crate::llm::ClientLlmResult {
                        actions: vec![],
                        memory_updates: None,
                    });
                }
                let memory = ctx
                    .state
                    .get_memory_for_client(client_id)
                    .await
                    .unwrap_or_default();
                crate::client::llm_budget::call_llm_for_client(
                    &ctx.llm_client,
                    &ctx.state,
                    client_id.to_string(),
                    &instruction,
                    &memory,
                    Some(&event),
                    &protocol,
                    &ctx.status_tx,
                )
                .await
            };
            tokio::pin!(connected);
            let mut connected_done = false;
            'session: loop {
                tokio::select! {
                    result = &mut connected, if !connected_done => {
                        connected_done = true;
                        match result {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates { ctx.state.set_memory_for_client(client_id, memory).await; }
                                for action in result.actions {
                                    match Self::apply(&socket, &protocol, dialect, action).await {
                                        Ok(ClientSendOutcome::Disconnected) => break 'session,
                                        Ok(ClientSendOutcome::Rejected { error }) => { console_error!(ctx.status_tx, "StatsD initial action rejected: {}", error); }
                                        Ok(_) => {},
                                        Err(error) => { console_error!(ctx.status_tx, "StatsD send failed: {}", error); }
                                    }
                                }
                            }
                            Err(error) => { console_error!(ctx.status_tx, "StatsD connected handler failed: {}", error); }
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break; };
                        let action = command.action.clone();
                        let result = Self::apply(&socket, &protocol, dialect, action.clone()).await;
                        let disconnect = matches!(result, Ok(ClientSendOutcome::Disconnected));
                        let response = match &result { Ok(outcome) => serde_json::to_value(outcome).unwrap_or(Value::Null), Err(error) => json!({"error":error.to_string()}) };
                        ctx.state.record_access_log(AccessLogOwner::Client(client_id.as_u32()), "StatsD", None, "injected_action", action, vec![response]).await;
                        crate::client::command_support::reply(command, result);
                        if disconnect { break; }
                    }
                }
                let _ = ctx.status_tx.send("__UPDATE_UI__".into());
            }
            // Dropping the pending connected-handler future also cancels it when
            // disconnect arrives while a manual/model response is outstanding.
            ctx.state.remove_client_handle(client_id).await;
            ctx.state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
            let _ = ctx.status_tx.send("__UPDATE_UI__".into());
        });
        registrar.register_client_task(client_id, handle).await;
        Ok(local_addr)
    }
    async fn apply(
        socket: &UdpSocket,
        protocol: &StatsdClientProtocol,
        dialect: Dialect,
        action: Value,
    ) -> Result<ClientSendOutcome> {
        let result = match protocol.execute_action(action) {
            Ok(result) => result,
            Err(error) => {
                return Ok(ClientSendOutcome::Rejected {
                    error: error.to_string(),
                })
            }
        };
        match result {
            ClientActionResult::Custom { name, data } if name == "send_statsd_batch" => {
                let records: Vec<codec::Record> = serde_json::from_value(data)?;
                let bytes = match codec::encode_datagram(&records, dialect) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return Ok(ClientSendOutcome::Rejected {
                            error: error.to_string(),
                        })
                    }
                };
                let bytes_sent = socket.send(&bytes).await?;
                Ok(ClientSendOutcome::Sent { bytes_sent })
            }
            ClientActionResult::Disconnect => Ok(ClientSendOutcome::Disconnected),
            _ => bail!("unsupported StatsD action result"),
        }
    }
}
