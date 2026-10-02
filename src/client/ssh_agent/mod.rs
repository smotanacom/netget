//! SSH Agent client implementation
//!
//! Platform: Unix/Linux/macOS (uses Unix domain sockets)
#![cfg(unix)]

pub mod actions;

pub use actions::SshAgentClientProtocol;

use anyhow::{Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use futures::StreamExt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use tracing::{error, info};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::ssh_agent::actions::{
    SSH_AGENT_CLIENT_CONNECTED_EVENT, SSH_AGENT_CLIENT_RESPONSE_RECEIVED_EVENT,
};
use crate::llm::actions::client_trait::ClientActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// Bound server-controlled SSH agent frame lengths before allocating their bodies.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Decode complete SSH agent packets, stripping their four-byte big-endian length.
/// Keeping framing in the reader preserves partial packets across command-channel
/// select arms and separates multiple responses received in one socket read.
pub fn response_reader<R: AsyncRead + Unpin>(reader: R) -> AgentResponseReader<R> {
    response_reader_with_timeout(reader, crate::client::response_reader::RESPONSE_DEADLINE)
}

pub fn response_reader_with_timeout<R: AsyncRead + Unpin>(
    reader: R,
    timeout: std::time::Duration,
) -> AgentResponseReader<R> {
    AgentResponseReader {
        timeout,
        inner: FramedRead::new(
            ProgressReader {
                inner: reader,
                received: false,
            },
            LengthDelimitedCodec::builder()
                .max_frame_length(MAX_RESPONSE_BYTES)
                .new_codec(),
        ),
        deadline: None,
        ended: false,
    }
}

struct ProgressReader<R> {
    inner: R,
    received: bool,
}
impl<R: AsyncRead + Unpin> AsyncRead for ProgressReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        this.received |= buf.filled().len() > before;
        result
    }
}

/// Keeps an absolute partial-frame deadline across cancellation of `next()`.
pub struct AgentResponseReader<R> {
    timeout: std::time::Duration,
    inner: FramedRead<ProgressReader<R>, LengthDelimitedCodec>,
    deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    ended: bool,
}
impl<R: AsyncRead + Unpin> futures::Stream for AgentResponseReader<R> {
    type Item = std::io::Result<BytesMut>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future;
        let this = self.get_mut();
        if this.ended {
            return std::task::Poll::Ready(None);
        }
        // Enforce the original deadline even when the final bytes became ready
        // while a caller had cancelled (and was not polling) `next()`.
        if this
            .deadline
            .as_mut()
            .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
        {
            this.ended = true;
            return std::task::Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "SSH agent partial frame deadline exceeded",
            ))));
        }
        let had_buffer = !this.inner.read_buffer().is_empty();
        match std::pin::Pin::new(&mut this.inner).poll_next(cx) {
            std::task::Poll::Ready(value) => {
                this.deadline = None;
                this.inner.get_mut().received = false;
                return std::task::Poll::Ready(value);
            }
            std::task::Poll::Pending => {}
        }
        if (had_buffer || this.inner.get_ref().received) && this.deadline.is_none() {
            this.deadline = Some(Box::pin(tokio::time::sleep(this.timeout)));
        }
        if this
            .deadline
            .as_mut()
            .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
        {
            this.ended = true;
            return std::task::Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "SSH agent partial frame deadline exceeded",
            ))));
        }
        std::task::Poll::Pending
    }
}

/// SSH Agent message types
const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
const SSH_AGENTC_ADD_IDENTITY: u8 = 17;
const SSH_AGENTC_REMOVE_IDENTITY: u8 = 18;
const SSH_AGENTC_REMOVE_ALL_IDENTITIES: u8 = 19;

const SSH_AGENT_FAILURE: u8 = 5;
const SSH_AGENT_SUCCESS: u8 = 6;
/// Where an SSH Agent client connects when no address is given.
///
/// Matches the `socket_path` default of NetGet's own SSH Agent server, so the pair works out
/// of the box against fabricated keys. It is emphatically not `$SSH_AUTH_SOCK` - see
/// `connect_with_llm_actions`.
const DEFAULT_AGENT_SOCKET: &str = "./netget-ssh-agent.sock";

const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
const SSH_AGENT_SIGN_RESPONSE: u8 = 14;

/// Memory is shared with the connected/response event flow, never held across a model call.
struct ClientData {
    memory: String,
}

/// A framed writer that permanently loses its transport after an incomplete write.
/// Taking the transport out before awaiting also covers caller cancellation: its
/// drop closes the owned Unix write half, and subsequent commands cannot append
/// a new frame into an unfinished one.
pub struct AgentWriter<W> {
    transport: Option<W>,
}
impl<W: tokio::io::AsyncWrite + Unpin> AgentWriter<W> {
    pub fn new(transport: W) -> Self {
        Self {
            transport: Some(transport),
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        if let Some(mut transport) = self.transport.take() {
            transport.shutdown().await?;
        }
        Ok(())
    }
}

/// SSH Agent client that connects to an SSH agent
pub struct SshAgentClient;

impl SshAgentClient {
    /// Connect to an SSH Agent with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Resolve the socket path.
        //
        // This deliberately does NOT fall back to `$SSH_AUTH_SOCK`. It used to, and that was
        // the single most dangerous default in this protocol: with no address given, an
        // LLM-driven client silently attached to the operator's *live* ssh-agent and could
        // then enumerate their real identities and ask the agent to sign arbitrary bytes with
        // their real private keys. Nothing about the request said that was happening, and the
        // model - not the operator - chose what to sign.
        //
        // The default is now NetGet's own agent socket, matching the `socket_path` default in
        // `src/server/ssh_agent/`, so an empty address pairs the client with a NetGet server
        // whose keys are fabricated. Pointing this at a real agent is still possible and is
        // still a legitimate thing to do - it just has to be asked for by name.
        let socket_path = if remote_addr.is_empty() {
            PathBuf::from(DEFAULT_AGENT_SOCKET)
        } else {
            PathBuf::from(&remote_addr)
        };

        info!(
            "SSH Agent client {} connecting to {:?}",
            client_id, socket_path
        );

        // Connect to Unix socket
        let stream = UnixStream::connect(&socket_path).await.context(format!(
            "Failed to connect to SSH Agent at {:?}",
            socket_path
        ))?;

        info!("SSH Agent client {} connected", client_id);

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] SSH Agent client {} connected to {:?}",
            client_id, socket_path
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream
        let (read_half, write_half) = stream.into_split();
        let write_half_arc = Arc::new(Mutex::new(AgentWriter::new(write_half)));

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            memory: String::new(),
        }));

        // Spawn read loop
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        // Command channel: lets the dashboard inject actions into this loop
        // via AppState::send_to_client (see client/command_support.rs).
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        let command_writer = write_half_arc.clone();
        let command_state = app_state.clone();
        let command_status = status_tx.clone();
        let command_task = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                if Self::handle_injected_command(
                    &command_writer,
                    command,
                    client_id,
                    &command_state,
                    &command_status,
                )
                .await
                {
                    command_state
                        .update_client_status(client_id, ClientStatus::Disconnected)
                        .await;
                    command_state.remove_client_handle(client_id).await;
                    break;
                }
            }
        });
        app_state
            .register_client_task(client_id, command_task)
            .await;
        let task_handle = tokio::spawn(async move {
            let protocol = Arc::new(SshAgentClientProtocol::new());
            let connected = Event::new(
                &SSH_AGENT_CLIENT_CONNECTED_EVENT,
                serde_json::json!({"socket_path": socket_path.to_string_lossy()}),
            );
            let result: Result<()> = async {
                if Self::process_event(
                    &connected,
                    client_id,
                    &llm_client,
                    &app_state,
                    &status_tx,
                    &protocol,
                    &write_half_arc,
                    &client_data,
                )
                .await?
                {
                    return Ok(());
                }
                let mut reader = response_reader(read_half);
                // Frame processing is serial. FramedRead and socket backpressure retain
                // complete/partial frames while an event handler runs; no queue is discarded.
                while let Some(frame) = reader.next().await {
                    let data = Self::parse_response(&frame?)?;
                    let event = Event::new(&SSH_AGENT_CLIENT_RESPONSE_RECEIVED_EVENT, data);
                    if Self::process_event(
                        &event,
                        client_id,
                        &llm_client,
                        &app_state,
                        &status_tx,
                        &protocol,
                        &write_half_arc,
                        &client_data,
                    )
                    .await?
                    {
                        break;
                    }
                }
                Ok(())
            }
            .await;
            let status = match result {
                Ok(()) => ClientStatus::Disconnected,
                Err(error) => {
                    error!("SSH agent client {} failed: {}", client_id, error);
                    ClientStatus::Error(error.to_string())
                }
            };
            app_state.update_client_status(client_id, status).await;
            app_state.remove_client_handle(client_id).await;
            let _ = status_tx.send("__UPDATE_UI__".into());
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        // Return dummy socket address (Unix sockets don't have IP addresses)
        Ok("127.0.0.1:0".parse().unwrap())
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_event(
        event: &Event,
        client_id: ClientId,
        llm: &OllamaClient,
        state: &Arc<AppState>,
        status: &mpsc::UnboundedSender<String>,
        protocol: &Arc<SshAgentClientProtocol>,
        writer: &Arc<Mutex<AgentWriter<tokio::net::unix::OwnedWriteHalf>>>,
        memory: &Arc<Mutex<ClientData>>,
    ) -> Result<bool> {
        use crate::llm::actions::client_trait::Client;
        let Some(instruction) = state.get_instruction_for_client(client_id).await else {
            return Ok(false);
        };
        let current_memory = memory.lock().await.memory.clone();
        let ClientLlmResult {
            actions,
            memory_updates,
        } = call_llm_for_client(
            llm,
            state,
            client_id.to_string(),
            &instruction,
            &current_memory,
            Some(event),
            protocol.as_ref(),
            status,
        )
        .await?;
        if let Some(value) = memory_updates {
            memory.lock().await.memory = value;
        }
        for action in actions {
            match protocol.execute_action(action)? {
                ClientActionResult::Custom { name, data } => {
                    Self::handle_custom_action(&name, data, client_id, writer, state, status)
                        .await?
                }
                ClientActionResult::Disconnect => {
                    writer.lock().await.shutdown().await?;
                    return Ok(true);
                }
                ClientActionResult::NoAction | ClientActionResult::WaitForMore => {}
                _ => anyhow::bail!("unsupported SSH agent action result"),
            }
        }
        Ok(false)
    }

    /// Parse SSH Agent response message
    pub fn parse_response(data: &[u8]) -> Result<serde_json::Value> {
        if data.is_empty() {
            anyhow::bail!("Empty response");
        }

        // SSH Agent wire format: [uint32: length][byte: type][data...]
        // Assume length prefix already consumed by reader
        let msg_type = data[0];
        let mut cursor = &data[1..];

        match msg_type {
            SSH_AGENT_SUCCESS => Ok(serde_json::json!({
                "response_type": "success",
                "response_data": {}
            })),
            SSH_AGENT_FAILURE => Ok(serde_json::json!({
                "response_type": "failure",
                "response_data": {}
            })),
            SSH_AGENT_IDENTITIES_ANSWER => {
                let num_keys = Self::read_uint32(&mut cursor)?;
                let mut identities = Vec::new();

                for _ in 0..num_keys {
                    let key_blob = Self::read_string(&mut cursor)?;
                    let comment = Self::read_string(&mut cursor)?;

                    identities.push(serde_json::json!({
                        "public_key_blob_hex": hex::encode(key_blob),
                        "comment": String::from_utf8_lossy(comment),
                    }));
                }

                Ok(serde_json::json!({
                    "response_type": "identities",
                    "response_data": {
                        "count": num_keys,
                        "identities": identities,
                    }
                }))
            }
            SSH_AGENT_SIGN_RESPONSE => {
                let signature_blob = Self::read_string(&mut cursor)?;

                Ok(serde_json::json!({
                    "response_type": "signature",
                    "response_data": {
                        "signature_hex": hex::encode(signature_blob),
                    }
                }))
            }
            _ => {
                anyhow::bail!("Unknown SSH Agent response type: {}", msg_type);
            }
        }
    }

    /// Encode every action through one implementation shared by injected and model actions.
    pub fn encode_custom_action(action_name: &str, data: &serde_json::Value) -> Result<Bytes> {
        let mut message = BytesMut::new();
        match action_name {
            "request_identities" => message.put_u8(SSH_AGENTC_REQUEST_IDENTITIES),
            "remove_all_identities" => message.put_u8(SSH_AGENTC_REMOVE_ALL_IDENTITIES),
            "sign_request" => {
                let key = hex::decode(
                    data["public_key_blob_hex"]
                        .as_str()
                        .context("Missing public_key_blob_hex")?,
                )?;
                let payload = hex::decode(data["data_hex"].as_str().context("Missing data_hex")?)?;
                let flags = match data.get("flags") {
                    None => 0,
                    Some(value) => u32::try_from(
                        value
                            .as_u64()
                            .context("flags must be an unsigned integer")?,
                    )
                    .context("flags exceed u32")?,
                };
                message.put_u8(SSH_AGENTC_SIGN_REQUEST);
                Self::write_string(&mut message, &key)?;
                Self::write_string(&mut message, &payload)?;
                message.put_u32(flags);
            }
            "add_identity" => {
                let key_type = data["key_type"].as_str().context("Missing key_type")?;
                let key = hex::decode(
                    data["public_key_blob_hex"]
                        .as_str()
                        .context("Missing public_key_blob_hex")?,
                )?;
                let private = hex::decode(
                    data["private_key_blob_hex"]
                        .as_str()
                        .context("Missing private_key_blob_hex")?,
                )?;
                message.put_u8(SSH_AGENTC_ADD_IDENTITY);
                Self::write_string(&mut message, key_type.as_bytes())?;
                Self::write_string(&mut message, &key)?;
                Self::write_string(&mut message, &private)?;
                Self::write_string(
                    &mut message,
                    data["comment"].as_str().unwrap_or("").as_bytes(),
                )?;
            }
            "remove_identity" => {
                let key = hex::decode(
                    data["public_key_blob_hex"]
                        .as_str()
                        .context("Missing public_key_blob_hex")?,
                )?;
                message.put_u8(SSH_AGENTC_REMOVE_IDENTITY);
                Self::write_string(&mut message, &key)?;
            }
            _ => anyhow::bail!("Unknown custom action: {action_name}"),
        }
        if message.len() > MAX_RESPONSE_BYTES {
            anyhow::bail!("SSH agent request exceeds byte cap");
        }
        Ok(message.freeze())
    }

    async fn handle_custom_action(
        name: &str,
        data: serde_json::Value,
        _client_id: ClientId,
        writer: &Arc<Mutex<AgentWriter<tokio::net::unix::OwnedWriteHalf>>>,
        _state: &Arc<AppState>,
        _status: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        Self::send_message(Self::encode_custom_action(name, &data)?, writer).await?;
        Ok(())
    }

    /// Execute an injected action on the actual agent socket and report its wire outcome.
    pub async fn handle_injected_command<W: tokio::io::AsyncWrite + Unpin>(
        writer: &Arc<Mutex<AgentWriter<W>>>,
        command: crate::state::client_handles::ClientCommand,
        client_id: ClientId,
        state: &Arc<AppState>,
        status: &mpsc::UnboundedSender<String>,
    ) -> bool {
        use crate::llm::actions::client_trait::Client;
        use crate::state::client_handles::ClientSendOutcome;
        let action = command.action.clone();
        let outcome: Result<ClientSendOutcome> = async {
            match SshAgentClientProtocol.execute_action(action.clone())? {
                ClientActionResult::Custom { name, data } => {
                    let bytes_sent =
                        Self::send_message(Self::encode_custom_action(&name, &data)?, writer)
                            .await?;
                    Ok(ClientSendOutcome::Sent { bytes_sent })
                }
                ClientActionResult::Disconnect => {
                    writer.lock().await.shutdown().await?;
                    Ok(ClientSendOutcome::Disconnected)
                }
                ClientActionResult::NoAction | ClientActionResult::WaitForMore => {
                    Ok(ClientSendOutcome::Executed {
                        detail: "no agent packet requested".into(),
                    })
                }
                _ => anyhow::bail!("unsupported SSH agent action result"),
            }
        }
        .await;
        let disconnected = matches!(&outcome, Ok(ClientSendOutcome::Disconnected));
        let detail = match &outcome {
            Ok(value) => serde_json::to_value(value).unwrap_or_default(),
            Err(error) => serde_json::json!({"error": error.to_string()}),
        };
        state
            .record_access_log(
                crate::state::AccessLogOwner::Client(client_id.as_u32()),
                "ssh-agent",
                None,
                "injected_action",
                action,
                vec![detail],
            )
            .await;
        crate::client::command_support::reply(command, outcome);
        let _ = status.send("__UPDATE_UI__".into());
        disconnected
    }

    async fn send_message<W: tokio::io::AsyncWrite + Unpin>(
        data: Bytes,
        writer: &Arc<Mutex<AgentWriter<W>>>,
    ) -> Result<usize> {
        Self::send_message_with_timeout(
            data,
            writer,
            crate::client::response_reader::RESPONSE_DEADLINE,
        )
        .await
    }

    /// Bound the complete queued write, including waiting for the writer lock.
    pub async fn send_message_with_timeout<W: tokio::io::AsyncWrite + Unpin>(
        data: Bytes,
        writer: &Arc<Mutex<AgentWriter<W>>>,
        timeout: std::time::Duration,
    ) -> Result<usize> {
        if data.len() > MAX_RESPONSE_BYTES {
            anyhow::bail!("SSH agent request exceeds byte cap");
        }
        let length = u32::try_from(data.len()).context("SSH agent packet length exceeds u32")?;
        let mut packet = BytesMut::with_capacity(data.len() + 4);
        packet.put_u32(length);
        packet.extend_from_slice(&data);
        tokio::time::timeout(timeout, async {
            let mut writer = writer.lock().await;
            let mut transport = writer
                .transport
                .take()
                .context("SSH agent writer is unusable after an interrupted write or disconnect")?;
            transport.write_all(&packet).await?;
            writer.transport = Some(transport);
            Ok::<_, anyhow::Error>(packet.len())
        })
        .await
        .context("SSH agent write deadline exceeded")?
    }

    /// Read SSH wire format string (uint32 length + bytes)
    fn read_string<'a>(cursor: &mut &'a [u8]) -> Result<&'a [u8]> {
        if cursor.len() < 4 {
            anyhow::bail!("Not enough data to read string length");
        }
        let len = u32::from_be_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]) as usize;
        *cursor = &cursor[4..];

        if cursor.len() < len {
            anyhow::bail!("Not enough data to read string of length {}", len);
        }
        let result = &cursor[..len];
        *cursor = &cursor[len..];
        Ok(result)
    }

    /// Read SSH wire format uint32
    fn read_uint32(cursor: &mut &[u8]) -> Result<u32> {
        if cursor.len() < 4 {
            anyhow::bail!("Not enough data to read uint32");
        }
        let result = u32::from_be_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]);
        *cursor = &cursor[4..];
        Ok(result)
    }

    /// Write SSH wire format string (uint32 length + bytes)
    fn write_string(buffer: &mut BytesMut, data: &[u8]) -> Result<()> {
        if data.len()
            > MAX_RESPONSE_BYTES
                .saturating_sub(buffer.len())
                .saturating_sub(4)
        {
            anyhow::bail!("SSH agent request exceeds byte cap");
        }
        buffer.put_u32(u32::try_from(data.len()).context("SSH string length exceeds u32")?);
        buffer.put_slice(data);
        Ok(())
    }
}
