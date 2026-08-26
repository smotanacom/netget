//! NFC (Near Field Communication) client implementation
//!
//! Uses PC/SC (Personal Computer/Smart Card) API for cross-platform NFC reader support:
//! - Windows: Native WinSCard.dll
//! - macOS: Native PCSC framework
//! - Linux: PCSC lite library (pcscd daemon)
//!
//! Supports ISO14443 A/B cards, MIFARE, NFC tags via APDU commands and NDEF messages.

pub mod actions;

use crate::client::llm_budget::call_llm_for_client;
use crate::client::nfc::actions::*;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::ClientId;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// Re-export protocol
pub use actions::NfcClientProtocol;

/// What applying one action to the PC/SC reader actually did.
///
/// `Sent` is reported only when an APDU was really handed to a card that
/// answered — those bytes crossed the contactless interface. Everything else
/// says specifically why nothing did.
pub enum NfcApplied {
    /// This many APDU bytes reached the card; the response APDU is attached so
    /// the caller can log it.
    Sent {
        bytes_sent: usize,
        response_hex: String,
    },
    /// The action ran but nothing reached a card; the string says why.
    Executed(String),
    /// The card session was ended.
    Disconnected,
}

/// NFC client implementation
pub struct NfcClient;

impl NfcClient {
    /// Connect to NFC reader and start LLM integration loop
    pub async fn connect_with_llm_actions(
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Value,
    ) -> Result<SocketAddr> {
        info!("Starting NFC client via PC/SC...");

        // Extract reader selection from startup params
        let reader_index = startup_params["reader_index"].as_u64().unwrap_or(0) as usize;
        let reader_name = startup_params["reader_name"]
            .as_str()
            .map(|s| s.to_string());

        // Create PC/SC context
        let ctx = pcsc::Context::establish(pcsc::Scope::User)
            .context("Failed to establish PC/SC context. Is pcscd running (Linux)?")?;

        // List available readers
        let readers_buf = ctx
            .list_readers_owned()
            .context("Failed to list PC/SC readers")?;

        if readers_buf.is_empty() {
            return Err(anyhow!(
                "No PC/SC readers found. Please connect an NFC reader (e.g., ACR122U)"
            ));
        }

        let readers: Vec<String> = readers_buf
            .iter()
            .map(|r| r.to_string_lossy().to_string())
            .collect();

        info!("Found {} PC/SC reader(s): {:?}", readers.len(), readers);

        // Select reader. Keep the `CString` form: that is what `Context::connect`
        // wants, and round-tripping through `String` can lose a non-UTF-8 name.
        let selected_reader: std::ffi::CString = if let Some(name) = reader_name {
            readers_buf
                .iter()
                .find(|r| r.to_string_lossy().contains(&name))
                .ok_or_else(|| anyhow!("Reader '{}' not found", name))?
                .clone()
        } else {
            readers_buf
                .get(reader_index)
                .ok_or_else(|| anyhow!("Reader index {} out of range", reader_index))?
                .clone()
        };

        let selected_reader_name = selected_reader.to_string_lossy().to_string();
        info!("Using PC/SC reader: {}", selected_reader_name);
        let _ = status_tx.send(format!("Using NFC reader: {}", selected_reader_name));

        // Command channel for injected actions (the dashboard's [ send ]).
        // Registered BEFORE the readers-listed LLM call below: a dashboard-created
        // client defaults to a `*` -> manual rule, so that call can park for minutes
        // waiting for a human, and the operator must be able to reach the reader while
        // it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn({
            let ctx = ctx.clone();
            let reader = selected_reader.clone();
            let app_state = app_state.clone();
            let llm_client = llm_client.clone();
            let status_tx = status_tx.clone();
            async move {
                Self::command_loop(
                    command_rx, ctx, reader, client_id, app_state, llm_client, status_tx,
                )
                .await;
            }
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Send initial event to LLM: readers listed
        {
            let event = Event::new(
                &NFC_READERS_LISTED_EVENT,
                json!({ "readers": readers.clone() }),
            );

            // Get default instruction from startup params or use default
            let instruction = startup_params["instruction"]
                .as_str()
                .unwrap_or("Monitor NFC reader and respond to card events");

            // Create protocol instance for action definitions
            let protocol = Arc::new(NfcClientProtocol);

            // A failing readers-listed call must not kill the client: the command
            // channel is already registered and an operator can drive the reader by
            // hand, which is exactly the case a `*` -> manual rule sets up.
            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                instruction,
                "", // No memory yet
                Some(&event),
                protocol.as_ref(),
                &status_tx,
            )
            .await
            {
                Ok(result) => {
                    for action in result.actions {
                        let applied = Self::apply_nfc_action(
                            &ctx,
                            &selected_reader,
                            action.clone(),
                            client_id,
                            &app_state,
                            &llm_client,
                            &status_tx,
                        )
                        .await;
                        if matches!(applied, Ok(NfcApplied::Disconnected)) {
                            break;
                        }
                        if let Err(e) = applied {
                            error!("NFC client {} action error: {}", client_id, e);
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error for NFC client {}: {}", client_id, e);
                }
            }
        }

        // Return dummy socket address (NFC doesn't use network sockets)
        Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// Drain injected commands until the channel closes (the client was removed,
    /// which drops the handle) or an injected `disconnect_card` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot serve this
    /// client: it owns no socket, and every NFC verb yields a
    /// `ClientActionResult::Custom` that only PC/SC can carry out. So the action goes
    /// through [`Self::apply_nfc_action`] — the same function the readers-listed LLM
    /// path uses — and the outcome is recorded and replied the way the generic arm
    /// does it.
    async fn command_loop(
        mut command_rx: tokio::sync::mpsc::Receiver<crate::state::client_handles::ClientCommand>,
        ctx: pcsc::Context,
        reader: std::ffi::CString,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::client_handles::ClientSendOutcome;
        use crate::state::AccessLogOwner;

        let protocol = NfcClientProtocol;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let mut extra_log: Vec<serde_json::Value> = Vec::new();

            let outcome: anyhow::Result<ClientSendOutcome> = match Self::apply_nfc_action(
                &ctx,
                &reader,
                action.clone(),
                client_id,
                &app_state,
                &llm_client,
                &status_tx,
            )
            .await
            {
                Ok(NfcApplied::Sent {
                    bytes_sent,
                    response_hex,
                }) => {
                    extra_log.push(json!({ "response_hex": response_hex }));
                    Ok(ClientSendOutcome::Sent { bytes_sent })
                }
                Ok(NfcApplied::Executed(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                Ok(NfcApplied::Disconnected) => Ok(ClientSendOutcome::Disconnected),
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
            };

            let mut responses = vec![match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => json!({ "error": e.to_string() }),
            }];
            responses.append(&mut extra_log);
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    responses,
                )
                .await;

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                break;
            }
        }

        // Every exit path lands here: drop the command handle so the dashboard stops
        // offering [ send ] on a client whose loop is gone.
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Carry one action out against the reader. Shared by the readers-listed LLM path
    /// and injected commands so the PC/SC calls exist exactly once.
    ///
    /// `Err` means the protocol rejected the action (unknown verb / bad params); a
    /// PC/SC failure is `Ok(Executed(..))` with the reason, because the action did run.
    /// NDEF file identifier, defaulting to E104 -- the value nearly every Type 4 tag uses
    /// and the one the Capability Container normally points at.
    fn ndef_file_id(data: &serde_json::Value) -> [u8; 2] {
        data.get("file_id")
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s.trim()).ok())
            .filter(|b| b.len() == 2)
            .map(|b| [b[0], b[1]])
            .unwrap_or([0xE1, 0x04])
    }

    /// Transmit one APDU on a freshly-connected card. Blocking PC/SC, off the runtime.
    async fn transmit_apdu(
        ctx: &pcsc::Context,
        reader: &std::ffi::CStr,
        apdu: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let ctx = ctx.clone();
        let reader = reader.to_owned();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let card = ctx
                .connect(&reader, pcsc::ShareMode::Shared, pcsc::Protocols::ANY)
                .context("PC/SC connect failed (is a card on the reader?)")?;
            let mut rx = vec![0u8; pcsc::MAX_BUFFER_SIZE];
            let response = card
                .transmit(&apdu, &mut rx)
                .context("PC/SC transmit failed")?;
            Ok(response.to_vec())
        })
        .await
        .context("PC/SC blocking task panicked")?
    }

    /// True when an APDU response ends in the success status word 0x9000.
    fn apdu_ok(response: &[u8]) -> bool {
        response.len() >= 2 && response[response.len() - 2..] == [0x90, 0x00]
    }

    /// Read the NDEF message from an NFC Forum Type 4 tag.
    ///
    /// The sequence is fixed by NFC Forum Type 4 Tag Operation: SELECT the NDEF
    /// application by AID D2760000850101, SELECT the NDEF file by its ID from the
    /// Capability Container, then READ BINARY -- first the two-byte NLEN, then that many
    /// bytes. It is expressed here as ordinary APDUs so it rides the same PC/SC transmit
    /// path `send_apdu` uses.
    ///
    /// Untested against real hardware: this machine has no PC/SC reader. The byte
    /// sequences are taken from the specification rather than observed, which is what
    /// `Experimental` is for -- see `src/client/nfc/CLAUDE.md`.
    async fn read_ndef(
        ctx: &pcsc::Context,
        reader: &std::ffi::CStr,
        file_id: [u8; 2],
    ) -> Result<Vec<u8>> {
        // SELECT the NDEF application (by name).
        let select_app = vec![
            0x00, 0xA4, 0x04, 0x00, 0x07, 0xD2, 0x76, 0x00, 0x00, 0x85, 0x01, 0x01, 0x00,
        ];
        let r = Self::transmit_apdu(ctx, reader, select_app).await?;
        if !Self::apdu_ok(&r) {
            anyhow::bail!(
                "SELECT NDEF application refused (SW {})",
                hex::encode_upper(&r)
            );
        }

        // SELECT the NDEF file (by identifier).
        let select_file = vec![0x00, 0xA4, 0x00, 0x0C, 0x02, file_id[0], file_id[1]];
        let r = Self::transmit_apdu(ctx, reader, select_file).await?;
        if !Self::apdu_ok(&r) {
            anyhow::bail!("SELECT NDEF file refused (SW {})", hex::encode_upper(&r));
        }

        // READ BINARY the 2-byte NLEN header, then the message itself.
        let r = Self::transmit_apdu(ctx, reader, vec![0x00, 0xB0, 0x00, 0x00, 0x02]).await?;
        if !Self::apdu_ok(&r) || r.len() < 4 {
            anyhow::bail!("READ BINARY of NLEN failed (SW {})", hex::encode_upper(&r));
        }
        let nlen = u16::from_be_bytes([r[0], r[1]]) as usize;
        if nlen == 0 {
            return Ok(Vec::new());
        }

        let mut message = Vec::with_capacity(nlen);
        let mut offset = 2usize;
        while message.len() < nlen {
            let want = std::cmp::min(0xFF, nlen - message.len()) as u8;
            let apdu = vec![0x00, 0xB0, (offset >> 8) as u8, (offset & 0xFF) as u8, want];
            let r = Self::transmit_apdu(ctx, reader, apdu).await?;
            if !Self::apdu_ok(&r) {
                anyhow::bail!("READ BINARY failed (SW {})", hex::encode_upper(&r));
            }
            message.extend_from_slice(&r[..r.len() - 2]);
            offset += want as usize;
        }
        Ok(message)
    }

    /// Write an NDEF message to an NFC Forum Type 4 tag.
    ///
    /// NLEN is zeroed first and written last, which the specification requires: a reader
    /// that interrupts the write mid-way then sees an empty message rather than a
    /// truncated one it would parse as valid.
    ///
    /// Untested against real hardware, as for [`Self::read_ndef`].
    async fn write_ndef(
        ctx: &pcsc::Context,
        reader: &std::ffi::CStr,
        file_id: [u8; 2],
        message: &[u8],
    ) -> Result<()> {
        let select_app = vec![
            0x00, 0xA4, 0x04, 0x00, 0x07, 0xD2, 0x76, 0x00, 0x00, 0x85, 0x01, 0x01, 0x00,
        ];
        let r = Self::transmit_apdu(ctx, reader, select_app).await?;
        if !Self::apdu_ok(&r) {
            anyhow::bail!(
                "SELECT NDEF application refused (SW {})",
                hex::encode_upper(&r)
            );
        }
        let select_file = vec![0x00, 0xA4, 0x00, 0x0C, 0x02, file_id[0], file_id[1]];
        let r = Self::transmit_apdu(ctx, reader, select_file).await?;
        if !Self::apdu_ok(&r) {
            anyhow::bail!("SELECT NDEF file refused (SW {})", hex::encode_upper(&r));
        }

        // NLEN = 0 while the body is in flight.
        let r = Self::transmit_apdu(ctx, reader, vec![0x00, 0xD6, 0x00, 0x00, 0x02, 0x00, 0x00])
            .await?;
        if !Self::apdu_ok(&r) {
            anyhow::bail!(
                "UPDATE BINARY of NLEN failed (SW {})",
                hex::encode_upper(&r)
            );
        }

        let mut written = 0usize;
        while written < message.len() {
            let chunk = std::cmp::min(0xFF - 6, message.len() - written);
            let offset = 2 + written;
            let mut apdu = vec![
                0x00,
                0xD6,
                (offset >> 8) as u8,
                (offset & 0xFF) as u8,
                chunk as u8,
            ];
            apdu.extend_from_slice(&message[written..written + chunk]);
            let r = Self::transmit_apdu(ctx, reader, apdu).await?;
            if !Self::apdu_ok(&r) {
                anyhow::bail!("UPDATE BINARY failed (SW {})", hex::encode_upper(&r));
            }
            written += chunk;
        }

        // Publish the real length last.
        let nlen = (message.len() as u16).to_be_bytes();
        let r = Self::transmit_apdu(
            ctx,
            reader,
            vec![0x00, 0xD6, 0x00, 0x00, 0x02, nlen[0], nlen[1]],
        )
        .await?;
        if !Self::apdu_ok(&r) {
            anyhow::bail!(
                "UPDATE BINARY of final NLEN failed (SW {})",
                hex::encode_upper(&r)
            );
        }
        Ok(())
    }

    async fn apply_nfc_action(
        ctx: &pcsc::Context,
        reader: &std::ffi::CStr,
        action: Value,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<NfcApplied> {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};

        match NfcClientProtocol.execute_action(action)? {
            ClientActionResult::Custom { name, data } if name == "send_apdu" => {
                let apdu_hex = data
                    .get("apdu_hex")
                    .and_then(|v| v.as_str())
                    .context("Missing apdu_hex in action data")?
                    .to_string();
                let apdu =
                    hex::decode(apdu_hex.trim()).context("apdu_hex is not valid hexadecimal")?;
                let sent = apdu.len();

                // PC/SC is a blocking C API: connect to whatever card is on the
                // reader, transmit, and drop the card handle, all on a blocking
                // thread. The handle is not kept between commands, so a card that
                // was removed and re-presented still works.
                let ctx = ctx.clone();
                let reader = reader.to_owned();
                let transmit = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    let card = ctx
                        .connect(&reader, pcsc::ShareMode::Shared, pcsc::Protocols::ANY)
                        .context("PC/SC connect failed (is a card on the reader?)")?;
                    let mut rx = vec![0u8; pcsc::MAX_BUFFER_SIZE];
                    let response = card
                        .transmit(&apdu, &mut rx)
                        .context("PC/SC transmit failed")?;
                    Ok(response.to_vec())
                })
                .await
                .context("PC/SC blocking task panicked")?;

                match transmit {
                    Ok(response) => {
                        let response_hex = hex::encode_upper(&response);
                        info!(
                            "NFC client {} APDU {} bytes -> response {}",
                            client_id, sent, response_hex
                        );
                        let _ = status_tx.send(format!(
                            "[CLIENT] NFC client {} APDU response: {}",
                            client_id, response_hex
                        ));

                        // `nfc_apdu_response` was declared and never emitted; an
                        // injected APDU is the first thing that actually raises it.
                        Self::notify_apdu_response(
                            &response, client_id, app_state, llm_client, status_tx,
                        )
                        .await;

                        Ok(NfcApplied::Sent {
                            bytes_sent: sent,
                            response_hex,
                        })
                    }
                    Err(e) => {
                        warn!("NFC client {} APDU failed: {:#}", client_id, e);
                        Ok(NfcApplied::Executed(format!(
                            "send_apdu did not reach a card: {e:#}"
                        )))
                    }
                }
            }
            ClientActionResult::Custom { name, data } if name == "read_ndef" => {
                let file_id = Self::ndef_file_id(&data);
                match Self::read_ndef(ctx, reader, file_id).await {
                    Ok(message) => {
                        let hex_msg = hex::encode_upper(&message);
                        info!(
                            "NFC client {} read {} byte NDEF message",
                            client_id,
                            message.len()
                        );
                        // NFC_NDEF_READ_EVENT was declared and never emitted; nothing
                        // could read an NDEF message at all until now.
                        Self::notify_ndef_read(
                            &message, client_id, app_state, llm_client, status_tx,
                        )
                        .await;
                        Ok(NfcApplied::Executed(format!(
                            "read_ndef: {} bytes ({})",
                            message.len(),
                            if hex_msg.is_empty() {
                                "empty"
                            } else {
                                &hex_msg
                            }
                        )))
                    }
                    Err(e) => Ok(NfcApplied::Executed(format!("read_ndef failed: {e:#}"))),
                }
            }
            ClientActionResult::Custom { name, data } if name == "write_ndef" => {
                let file_id = Self::ndef_file_id(&data);
                // Accept either a hex payload or plain text, and say which was used --
                // never guess, since "48656C6C6F" is both valid hex and valid text.
                let (bytes, how) = match data.get("message_hex").and_then(|v| v.as_str()) {
                    Some(h) => match hex::decode(h.trim()) {
                        Ok(b) => (b, "message_hex"),
                        Err(e) => {
                            return Ok(NfcApplied::Executed(format!(
                                "write_ndef: message_hex is not valid hexadecimal: {e}"
                            )))
                        }
                    },
                    None => match data.get("message").and_then(|v| v.as_str()) {
                        Some(t) => (t.as_bytes().to_vec(), "message"),
                        None => {
                            return Ok(NfcApplied::Executed(
                                "write_ndef needs message_hex or message".to_string(),
                            ))
                        }
                    },
                };
                match Self::write_ndef(ctx, reader, file_id, &bytes).await {
                    Ok(()) => Ok(NfcApplied::Sent {
                        bytes_sent: bytes.len(),
                        response_hex: format!("write_ndef ok ({} from {})", bytes.len(), how),
                    }),
                    Err(e) => Ok(NfcApplied::Executed(format!("write_ndef failed: {e:#}"))),
                }
            }
            ClientActionResult::Custom { name, .. } => Ok(NfcApplied::Executed(format!(
                "'{name}' is declared but not implemented by the NFC client; \
                 send_apdu / send_apdu_raw / read_ndef / write_ndef reach the card \
                 (see src/client/nfc/CLAUDE.md)"
            ))),
            ClientActionResult::Disconnect => {
                info!("NFC client {} disconnecting card session", client_id);
                app_state
                    .update_client_status(client_id, crate::state::ClientStatus::Disconnected)
                    .await;
                // Drop the command handle here rather than only in the command loop:
                // the LLM can disconnect too, and a handle left behind would offer
                // [ send ] into a client that is gone.
                app_state.remove_client_handle(client_id).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                Ok(NfcApplied::Disconnected)
            }
            ClientActionResult::WaitForMore => {
                Ok(NfcApplied::Executed("wait_for_more".to_string()))
            }
            other => Ok(NfcApplied::Executed(format!(
                "unhandled action result {other:?}"
            ))),
        }
    }

    /// Raise `nfc_ndef_read` so the model can act on the message that was read.
    ///
    /// The event was declared and never emitted, because nothing could read an NDEF
    /// message at all.
    async fn notify_ndef_read(
        message: &[u8],
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let event = Event::new(
            &NFC_NDEF_READ_EVENT,
            json!({
                "length": message.len(),
                "message_hex": hex::encode_upper(message),
                // Best-effort text view. NDEF records are typed and this is not a parse,
                // so the hex above stays the authoritative field.
                "message_text": String::from_utf8_lossy(message).to_string(),
            }),
        );
        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();
        if let Err(e) = call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &NfcClientProtocol,
            status_tx,
        )
        .await
        {
            error!("NFC client {} LLM error on nfc_ndef_read: {}", client_id, e);
        }
    }

    /// Raise `nfc_apdu_response` so the model can act on what the card answered.
    async fn notify_apdu_response(
        response: &[u8],
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let (data, sw) = if response.len() >= 2 {
            response.split_at(response.len() - 2)
        } else {
            (response, &[][..])
        };
        let event = Event::new(
            &NFC_APDU_RESPONSE_EVENT,
            json!({
                "response_hex": hex::encode_upper(response),
                "sw1": sw.first().map(|b| format!("{b:02X}")).unwrap_or_default(),
                "sw2": sw.get(1).map(|b| format!("{b:02X}")).unwrap_or_default(),
                "data_hex": hex::encode_upper(data),
            }),
        );
        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();
        if let Err(e) = call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &NfcClientProtocol,
            status_tx,
        )
        .await
        {
            error!("LLM error on nfc_apdu_response for {}: {}", client_id, e);
        }
    }
}
