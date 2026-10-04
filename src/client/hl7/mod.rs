//! HL7 v2 MLLP sender: one message in flight, its acknowledgment matched on MSA-2.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::hl7::wire::{self, Header};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::Hl7ClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const DEFAULT_APPLICATION: &str = "NETGET";
pub const DEFAULT_VERSION: &str = "2.5";
pub const DEFAULT_PROCESSING: &str = "P";

struct Identity {
    sending_application: String,
    sending_facility: String,
    receiving_application: String,
    receiving_facility: String,
    version: String,
    processing_id: String,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str, d: &str| -> Result<String> {
        let v = p
            .map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| d.to_owned());
        wire::field(&v)?;
        Ok(v)
    };
    let identity = Identity {
        sending_application: s("sending_application", DEFAULT_APPLICATION)?,
        sending_facility: s("sending_facility", "")?,
        receiving_application: s("receiving_application", "")?,
        receiving_facility: s("receiving_facility", "")?,
        version: s("version", DEFAULT_VERSION)?,
        processing_id: s("processing_id", DEFAULT_PROCESSING)?,
    };
    ensure!(
        matches!(identity.processing_id.as_str(), "P" | "T" | "D"),
        "processing_id must be P, T or D"
    );
    let stream = tokio::time::timeout(
        wire::IO_TIMEOUT,
        tokio::net::TcpStream::connect(&ctx.remote_addr),
    )
    .await
    .context("MLLP connect deadline")??;
    let local = stream.local_addr()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"remote_addr": ctx.remote_addr}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = Hl7ClientProtocol;
        while let Some(event) = event_rx.recv().await {
            let instruction = events_ctx
                .state
                .get_instruction_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = events_ctx
                .state
                .get_memory_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &protocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("HL7 client handler: {e}"))
                }
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            &identity,
            stream,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("HL7 client ended: {e}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, status)
            .await;
        session_ctx
            .state
            .remove_client_handle(session_ctx.client_id)
            .await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

fn reject(command: Option<ClientCommand>, error: String) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, Ok(ClientSendOutcome::Rejected { error }));
    }
}

async fn session(
    ctx: &ConnectContext,
    id: &Identity,
    stream: tokio::net::TcpStream,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut next = 1u64;
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
            // MLLP receivers never speak first; bytes here are a protocol error, EOF a close.
            unsolicited = wire::read_frame(&mut reader, std::time::Duration::from_secs(u32::MAX as u64)) => {
                return match unsolicited? { None => Ok(()), Some(_) => anyhow::bail!("MLLP receiver sent a frame with no message pending") };
            }
        };
        let data = match Hl7ClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Custom { data, .. }) => data,
            Ok(ClientActionResult::Disconnect) => {
                let _ = writer.shutdown().await;
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => {
                reject(command, "unsupported action".into());
                continue;
            }
            Err(e) => {
                reject(command, e.to_string());
                continue;
            }
        };
        let control = format!("NGC{next}");
        next += 1;
        let segments = wire::segments_from(&data["segments"])?;
        let header = Header {
            sending_application: &id.sending_application,
            sending_facility: &id.sending_facility,
            receiving_application: &id.receiving_application,
            receiving_facility: &id.receiving_facility,
            message_type: data["message_type"].as_str().unwrap_or(""),
            control_id: &control,
            processing_id: data["processing_id"].as_str().unwrap_or(&id.processing_id),
            version: &id.version,
        };
        let message = match wire::build(&header, &segments) {
            Ok(m) => m,
            Err(e) => {
                reject(command, e.to_string());
                continue;
            }
        };
        let written = wire::write_frame(&mut writer, &message).await;
        if command.is_some() {
            // Type and control id only: bodies carry patient data.
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "HL7",
                    None,
                    "injected_action",
                    json!({"message_type": header.message_type, "control_id": control}),
                    vec![json!({"sent": written.is_ok()})],
                )
                .await;
        }
        if let Some(c) = command {
            crate::client::command_support::reply(
                c,
                written
                    .as_ref()
                    .map(|n| ClientSendOutcome::Sent { bytes_sent: *n })
                    .map_err(|e| anyhow::anyhow!(e.to_string())),
            );
        }
        written?;
        let bytes = wire::read_frame(&mut reader, wire::IO_TIMEOUT)
            .await?
            .context("receiver closed before acknowledging")?;
        let ack = wire::parse(&bytes)?;
        let msa = ack
            .segments
            .iter()
            .find(|s| s.id == "MSA")
            .context("acknowledgment has no MSA segment")?;
        let code = msa.fields.first().cloned().unwrap_or_default();
        ensure!(
            wire::ACK_CODES.contains(&code.as_str()),
            "MSA-1 '{code}' is not an acknowledgment code"
        );
        let acked = msa.fields.get(1).cloned().unwrap_or_default();
        ensure!(
            acked == control,
            "MSA-2 '{acked}' does not acknowledge control id {control}"
        );
        events
            .try_send(Event::new(
                &actions::ACK_EVENT,
                json!({
                    "code": code,
                    "control_id": acked,
                    "text": msa.fields.get(2).cloned().unwrap_or_default(),
                    "message_type": ack.message_type(),
                    "segments": ack.segments,
                }),
            ))
            .context("HL7 event queue full; consumer stalled")?;
    }
}
