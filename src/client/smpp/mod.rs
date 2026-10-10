//! SMPP 3.4 ESME client: one bound session; submits are pipelined and matched to their
//! responses by sequence number; deliver_sm (messages and receipts) is acknowledged in Rust.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::smpp::wire::{self, Pdu};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::SmppClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::{
    io::{AsyncWriteExt, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};

pub const DEFAULT_BIND: &str = "transceiver";
/// The session may stay quiet indefinitely; the model keeps it alive with enquire_link.
const READ_IDLE: Duration = Duration::from_secs(365 * 24 * 3600);
/// Submits awaiting their response at once.
const MAX_PENDING: usize = 64;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let get = |name: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(name))
            .transpose()?
            .flatten())
    };
    let system_id = get("system_id")?.context("system_id is required")?;
    let password = get("password")?.unwrap_or_default();
    let system_type = get("system_type")?.unwrap_or_default();
    anyhow::ensure!(
        system_id.len() <= 15 && password.len() <= 8 && system_type.len() <= 12,
        "system_id is at most 15 characters, password 8, system_type 12"
    );
    let bind_name = get("bind")?.unwrap_or_else(|| DEFAULT_BIND.into());
    let command_id = match bind_name.as_str() {
        "transceiver" => wire::BIND_TRANSCEIVER,
        "transmitter" => wire::BIND_TRANSMITTER,
        "receiver" => wire::BIND_RECEIVER,
        other => anyhow::bail!("bind must be transceiver, transmitter or receiver, not {other}"),
    };
    let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("SMPP connect deadline")??;
    let local = stream.local_addr()?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let bind = wire::Bind {
        system_id,
        password,
        system_type,
        interface_version: 0x34,
        address_range: String::new(),
    };
    writer
        .write_all(&Pdu::new(command_id, 0, 1, wire::encode_bind(&bind)).encode())
        .await?;
    let resp = wire::read_pdu(&mut reader, wire::IO_TIMEOUT)
        .await?
        .context("SMSC closed the connection during bind")?;
    anyhow::ensure!(
        resp.command_id == command_id | wire::RESP && resp.sequence == 1,
        "SMSC answered the bind with command 0x{:08X}",
        resp.command_id
    );
    anyhow::ensure!(
        resp.status == wire::ESME_ROK,
        "SMSC refused the bind: {}",
        wire::status_name(resp.status)
    );
    let smsc_system_id = wire::Reader::new(&resp.body)
        .cstring(16)
        .unwrap_or_default();
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (pdu_tx, pdu_rx) = mpsc::channel::<Result<Pdu>>(64);
    let reader_task = tokio::spawn(async move {
        loop {
            let item = wire::read_pdu(&mut reader, READ_IDLE).await;
            let end = !matches!(item, Ok(Some(_)));
            let sent = match item {
                Ok(Some(p)) => pdu_tx.send(Ok(p)).await,
                Ok(None) => {
                    pdu_tx
                        .send(Err(anyhow::anyhow!("SMSC closed the connection")))
                        .await
                }
                Err(e) => pdu_tx.send(Err(e)).await,
            };
            if end || sent.is_err() {
                return;
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(64);
    event_tx.try_send(Event::new(
        &actions::BOUND_EVENT,
        json!({"smsc_system_id": smsc_system_id, "bind": bind_name}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = SmppClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("SMPP client handler: {e}"))
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
            writer,
            pdu_rx,
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
                Log::new(Some(&session_ctx.status_tx)).warn(format!("SMPP client ended: {e}"));
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

fn deliver_event(message: &wire::Message) -> Value {
    let text = wire::decode_text(message.data_coding, &message.payload);
    let is_receipt = message.esm_class & 0x3C == 0x04;
    let mut event = json!({
        "source_addr": message.source_addr,
        "destination_addr": message.destination_addr,
        "is_receipt": is_receipt,
    });
    match &text {
        Some(t) => {
            event["text"] = json!(t);
            if is_receipt {
                if let Some(fields) = wire::parse_receipt(t) {
                    event["receipt"] = Value::Object(fields);
                }
            }
        }
        None => {
            event["data"] = json!(message
                .payload
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>())
        }
    }
    event
}

async fn session(
    ctx: &ConnectContext,
    mut writer: WriteHalf<TcpStream>,
    mut pdus: mpsc::Receiver<Result<Pdu>>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut sequence = 1u32;
    // Sequence number -> (what to report it as, the injected caller waiting on it).
    let mut pending: HashMap<u32, (Value, Option<ClientCommand>)> = HashMap::new();
    loop {
        let (action, mut injected) = tokio::select! {
            item = pdus.recv() => {
                let Some(item) = item else { return Ok(()) };
                let pdu = item?;
                match pdu.command_id {
                    wire::DELIVER_SM => {
                        let parsed = wire::parse_message(&pdu.body);
                        let status = if parsed.is_ok() { wire::ESME_ROK } else { wire::ESME_RINVMSGLEN };
                        writer.write_all(&Pdu::new(wire::DELIVER_SM | wire::RESP, status, pdu.sequence, vec![0]).encode()).await?;
                        if let Ok(message) = parsed {
                            events.try_send(Event::new(&actions::DELIVER_EVENT, deliver_event(&message)))
                                .context("SMPP event queue full; consumer stalled")?;
                        }
                    }
                    wire::ENQUIRE_LINK => {
                        writer.write_all(&Pdu::new(wire::ENQUIRE_LINK | wire::RESP, 0, pdu.sequence, Vec::new()).encode()).await?;
                    }
                    wire::UNBIND => {
                        writer.write_all(&Pdu::new(wire::UNBIND | wire::RESP, 0, pdu.sequence, Vec::new()).encode()).await?;
                        return Ok(());
                    }
                    id if id & wire::RESP != 0 || id == wire::GENERIC_NACK => {
                        if let Some((mut report, caller)) = pending.remove(&pdu.sequence) {
                            report["status"] = json!(wire::status_name(pdu.status));
                            let event_type = if report["kind"] == "enquire" {
                                &*actions::LINK_OK_EVENT
                            } else {
                                if pdu.status == wire::ESME_ROK {
                                    if let Ok(id) = wire::Reader::new(&pdu.body).cstring(65) {
                                        report["message_id"] = json!(id);
                                    }
                                }
                                &*actions::SUBMIT_RESULT_EVENT
                            };
                            if let Some(obj) = report.as_object_mut() {
                                obj.remove("kind");
                            }
                            if let Some(command) = caller {
                                crate::client::command_support::reply(command, Ok(ClientSendOutcome::Executed { detail: report.to_string() }));
                            }
                            events.try_send(Event::new(event_type, report))
                                .context("SMPP event queue full; consumer stalled")?;
                        }
                    }
                    _ => {
                        writer.write_all(&Pdu::new(wire::GENERIC_NACK, wire::ESME_RINVCMDID, pdu.sequence, Vec::new()).encode()).await?;
                    }
                }
                continue;
            }
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        match SmppClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                sequence += 1;
                let _ = writer
                    .write_all(&Pdu::new(wire::UNBIND, 0, sequence, Vec::new()).encode())
                    .await;
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) if pending.len() >= MAX_PENDING => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: "too many requests awaiting the SMSC".into(),
                        }),
                    );
                }
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "SMPP",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        sequence = sequence % 0x7FFF_FFFF + 1;
        let (pdu, report) = if action["type"] == "smpp_enquire_link" {
            (
                Pdu::new(wire::ENQUIRE_LINK, 0, sequence, Vec::new()),
                json!({"kind": "enquire"}),
            )
        } else {
            let (coding, payload) = wire::encode_text(action["text"].as_str().unwrap_or_default());
            let message = wire::Message {
                source_ton: 0,
                source_npi: 0,
                source_addr: action["source_addr"].as_str().unwrap_or_default().into(),
                dest_ton: 1,
                dest_npi: 1,
                destination_addr: action["destination_addr"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                registered_delivery: u8::from(action["registered_delivery"] == true),
                data_coding: coding,
                payload,
                ..Default::default()
            };
            (
                Pdu::new(wire::SUBMIT_SM, 0, sequence, wire::encode_message(&message)),
                json!({"kind": "submit", "destination_addr": message.destination_addr}),
            )
        };
        tokio::time::timeout(wire::IO_TIMEOUT, writer.write_all(&pdu.encode()))
            .await
            .context("SMPP write deadline")??;
        pending.insert(sequence, (report, injected.take()));
    }
}
