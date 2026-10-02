//! GELF UDP/TCP emitter. The command loop owns the only socket and also handles initial actions.
pub mod actions;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::gelf::codec::{self, Compression, Transport};
use crate::state::{client_handles::ClientSendOutcome, AccessLogOwner, ClientStatus};
use crate::{console_error, console_info};
pub use actions::GelfClientProtocol;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct GelfClient;
impl GelfClient {
    pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        let transport = Transport::parse(
            &ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_string("transport"))
                .transpose()?
                .flatten()
                .unwrap_or_else(|| codec::DEFAULT_TRANSPORT.into()),
        )?;
        let compression = Compression::parse(
            &ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_string("compression"))
                .transpose()?
                .flatten()
                .unwrap_or_else(|| codec::DEFAULT_COMPRESSION.into()),
            transport,
        )?;
        let chunk_size = usize::try_from(
            ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_u64("chunk_size"))
                .transpose()?
                .flatten()
                .unwrap_or(codec::DEFAULT_CHUNK_SIZE as u64),
        )?;
        anyhow::ensure!(
            (13..=codec::MAX_DATAGRAM_BYTES).contains(&chunk_size),
            "chunk_size must be 13..8192"
        );
        let (peer, local_addr, mut reader, mut writer) = match transport {
            Transport::Tcp => {
                let socket = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
                    .await
                    .context("GELF connect deadline exceeded")??;
                let peer = socket.peer_addr()?;
                let local = socket.local_addr()?;
                let (reader, writer) = socket.into_split();
                (peer, local, Some(reader), Writer::Tcp(writer))
            }
            Transport::Udp => {
                let peer =
                    tokio::time::timeout(IO_TIMEOUT, tokio::net::lookup_host(&ctx.remote_addr))
                        .await
                        .context("GELF resolve deadline exceeded")??
                        .next()
                        .context("collector resolved to no addresses")?;
                let socket = tokio::net::UdpSocket::bind(if peer.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                })
                .await?;
                socket.connect(peer).await?;
                let local = socket.local_addr()?;
                (peer, local, None, Writer::Udp(socket))
            }
        };
        let mut commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        console_info!(
            ctx.status_tx,
            "GELF emitter {} connected to {}",
            ctx.client_id,
            peer
        );
        let registrar = ctx.state.clone();
        let client_id = ctx.client_id;
        let handle = tokio::spawn(async move {
            let protocol = GelfClientProtocol::new();
            let connected = async {
                let event = Event::new(
                    &actions::GELF_CONNECTED_EVENT,
                    json!({"remote_addr":peer.to_string(),"local_addr":local_addr.to_string(),"transport":transport.as_str()}),
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
                    .is_some_and(|c| c.find_handler("gelf_connected").is_some());
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
            let mut unexpected = [0u8; 1];
            'session: loop {
                tokio::select! {
                    result = async { match reader.as_mut() { Some(reader)=>reader.read(&mut unexpected).await, None=>std::future::pending().await } } => {
                        match result {
                            Ok(0) => { console_info!(ctx.status_tx, "GELF collector closed the connection"); },
                            Ok(_) => { console_error!(ctx.status_tx, "GELF collector sent unexpected response data; closing"); },
                            Err(error) => { console_error!(ctx.status_tx, "GELF collector read failed: {}", error); }
                        }
                        break;
                    }
                    result = &mut connected, if !connected_done => {
                        connected_done = true;
                        match result {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates { ctx.state.set_memory_for_client(client_id, memory).await; }
                                for action in result.actions {
                                    match Self::apply(&mut writer, &protocol, compression, chunk_size, action).await {
                                        Ok(ClientSendOutcome::Disconnected) => break 'session,
                                        Ok(ClientSendOutcome::Rejected { error }) => { console_error!(ctx.status_tx, "GELF initial action rejected: {}", error); }
                                        Ok(_) => {},
                                        Err(error) => { console_error!(ctx.status_tx, "GELF send failed: {}", error); break 'session; }
                                    }
                                }
                            }
                            Err(error) => { console_error!(ctx.status_tx, "GELF connected handler failed: {}", error); }
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break; };
                        let action = command.action.clone();
                        let result = Self::apply(&mut writer, &protocol, compression, chunk_size, action.clone()).await;
                        let disconnect = result.is_err() || matches!(result, Ok(ClientSendOutcome::Disconnected));
                        let response = match &result { Ok(outcome) => serde_json::to_value(outcome).unwrap_or(Value::Null), Err(error) => json!({"error":error.to_string()}) };
                        ctx.state.record_access_log(AccessLogOwner::Client(client_id.as_u32()), "GELF", None, "injected_action", action, vec![response]).await;
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
        writer: &mut Writer,
        protocol: &GelfClientProtocol,
        compression: Compression,
        chunk_size: usize,
        action: Value,
    ) -> Result<ClientSendOutcome> {
        let result = match protocol.execute_action(action) {
            Ok(r) => r,
            Err(error) => {
                return Ok(ClientSendOutcome::Rejected {
                    error: error.to_string(),
                })
            }
        };
        match result {
            ClientActionResult::Custom { name, data } if name == "send_gelf_message" => {
                let message: codec::Message = serde_json::from_value(data)?;
                // Produce and validate every byte before the first transport write.
                let frames = match writer {
                    Writer::Udp(_) => {
                        codec::encode_udp(&message, compression, chunk_size, rand::random())
                    }
                    Writer::Tcp(_) => codec::encode_json(&message).map(|mut b| {
                        b.push(0);
                        vec![b]
                    }),
                };
                let frames = match frames {
                    Ok(b) => b,
                    Err(error) => {
                        return Ok(ClientSendOutcome::Rejected {
                            error: error.to_string(),
                        })
                    }
                };
                let bytes_sent = frames.iter().map(Vec::len).sum();
                tokio::time::timeout(IO_TIMEOUT,async {
                    for bytes in frames {
                        match writer {
                            Writer::Tcp(stream)=>stream.write_all(&bytes).await?,
                            Writer::Udp(socket)=>{let n=socket.send(&bytes).await?;anyhow::ensure!(n==bytes.len(),"partial UDP write");}
                        }
                    }
                    Ok::<(),anyhow::Error>(())
                }).await.context("GELF write deadline exceeded; transport closed to avoid replay after partial send")??;
                Ok(ClientSendOutcome::Sent { bytes_sent })
            }
            ClientActionResult::Disconnect => Ok(ClientSendOutcome::Disconnected),
            _ => bail!("unsupported GELF action result"),
        }
    }
}
enum Writer {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    Udp(tokio::net::UdpSocket),
}
