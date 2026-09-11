//! NFC (Near Field Communication) virtual tag server.
//!
//! NetGet emulates an **NFC Forum Type 4 tag**, which is defined as ISO 7816-4
//! APDU exchange over ISO-DEP. It cannot generate an RF field — no PC/SC reader
//! can be driven into card-emulation mode — so the tag is reachable over a bound
//! TCP socket instead, using the **vsmartcard `vpcd` framing**:
//!
//! ```text
//! reader → tag:  u16 big-endian length | payload
//! tag → reader:  u16 big-endian length | payload
//! ```
//!
//! A payload of exactly one byte is a vpcd control code (`00` power off, `01`
//! power on, `02` reset, `04` request ATR); anything longer is a command APDU.
//!
//! This is the same wire format `src/server/usb/smartcard/` already speaks, and
//! it is not a private invention: configured with `DEVICENAME /dev/null:<host>:<port>`
//! the real `vpcd` ifdhandler connects *out* to a listening virtual card, so a
//! host running `pcscd` sees this server as a genuine PC/SC reader with a card
//! in it. A test, meanwhile, only has to write a length-prefixed APDU to the
//! socket — no hardware, no daemon.
//!
//! ## What the model controls
//!
//! - `nfc_server_started` → `set_atr`, `set_ndef_message` configure the tag.
//! - `nfc_tag_selected` (SELECT by AID) and `nfc_apdu_received` (every other
//!   APDU) → `respond_to_apdu` supplies the response body and status word.
//!
//! Nothing is answered by hardcoded card logic: there is no file system and no
//! stored NDEF parser here, only the state the model itself set. If no handler
//! produces a response the tag replies `6F00` (fail closed), never a success;
//! if the backend itself failed it replies `6400` when that backend is merely
//! saturated, so a reader retries instead of recording a broken card.

pub mod actions;
pub mod apdu;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::nfc::actions::*;
use crate::server::nfc::apdu::{ApduCommand, ApduResponse, SW_WRONG_LENGTH};
use crate::state::app_state::AppState;
use crate::state::server::ServerId;
use crate::utils::WireFailure;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, trace};

// Re-export protocol
pub use actions::NfcServerProtocol;

/// Largest frame accepted from a reader. Bounds the read buffer so a hostile
/// length prefix cannot make the server allocate.
///
/// `ApduCommand::parse` understands the whole extended form, whose `Lc` reaches
/// 65535 — but a frame carrying one is refused here long before that, so the
/// extended form is usable only up to ~4089 data bytes. The cap is what makes
/// the allocation bounded, so it wins over the parser's theoretical range; a
/// reader needing more would have to raise this, not the parser.
const MAX_FRAME_LEN: usize = 4096;

/// vpcd control codes (1-byte frames).
const VPCD_CTRL_OFF: u8 = 0x00;
const VPCD_CTRL_ON: u8 = 0x01;
const VPCD_CTRL_RESET: u8 = 0x02;
const VPCD_CTRL_ATR: u8 = 0x04;

/// Default ATR advertised for an ISO 14443-4 (Type 4) contactless tag.
const DEFAULT_ATR_HEX: &str = "3B8F8001804F0CA0000003060300030000000068";

/// Virtual NFC tag state, entirely LLM-configured.
struct VirtualNfcTag {
    /// Answer to Reset, returned for the vpcd `ATR` control code.
    atr: Vec<u8>,
    /// Tag UID, surfaced to the model in every APDU event.
    uid: String,
    /// Tag type, surfaced to the model in every APDU event.
    tag_type: String,
    /// NDEF records the model configured at startup, surfaced to the model in
    /// every APDU event so it can serve them itself.
    ndef_records: Vec<Value>,
}

impl VirtualNfcTag {
    fn new(uid: String, tag_type: String) -> Self {
        Self {
            atr: hex::decode(DEFAULT_ATR_HEX).unwrap_or_default(),
            uid,
            tag_type,
            ndef_records: Vec::new(),
        }
    }
}

/// NFC virtual tag server.
pub struct NfcServer;

impl NfcServer {
    /// Bind the virtual tag and start accepting readers.
    pub async fn start(
        bind_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
        startup_params: Value,
    ) -> Result<SocketAddr> {
        let tag_type = startup_params["tag_type"]
            .as_str()
            .unwrap_or("type4")
            .to_string();
        let uid = startup_params["uid"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // Generate a random 7-byte UID (the Type 4 double-size form).
                let random_bytes: Vec<u8> = (0..7).map(|_| rand::random::<u8>()).collect();
                hex::encode(random_bytes).to_uppercase()
            });

        // Bind before anything else so a port conflict is reported as a startup
        // failure rather than a server that claims to be Running with no socket.
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(bind_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!(
            "NFC virtual tag listening on {} (type={}, UID={})",
            local_addr, tag_type, uid
        ));

        let tag_state = Arc::new(Mutex::new(VirtualNfcTag::new(
            uid.clone(),
            tag_type.clone(),
        )));
        let protocol = Arc::new(NfcServerProtocol);

        // Configure the tag before the first reader can reach it.
        let event = Event::new(
            &NFC_SERVER_STARTED_EVENT,
            json!({
                "tag_type": tag_type,
                "uid": uid,
                "listen_addr": local_addr.to_string(),
            }),
        );
        // Opening a socket must not require the model to be reachable: the LLM answers
        // traffic, it does not open listeners. This call used to propagate with `?`, so an
        // Ollama outage made `spawn()` return `Err` and the server never started at all — a
        // backend blip took down a server that would have worked fine for every reader that
        // arrived after the backend came back.
        //
        // The tag is fully usable without it: `VirtualNfcTag::new` has already applied the
        // requested type and UID and a default ATR, and every *reader* APDU still goes through
        // `call_llm` with its own failure handling. So a failure here means "unconfigured, not
        // broken", and is reported as exactly that rather than as a dead server.
        match call_llm(
            &llm_client,
            &app_state,
            server_id,
            None, // Server-level event, no connection yet
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(result) => {
                for message in result.messages {
                    let _ = status_tx.send(message);
                }
                for action_result in result.protocol_results {
                    if let Err(e) =
                        Self::apply_startup_action(tag_state.clone(), action_result, &status_tx)
                            .await
                    {
                        // Non-fatal: the tag stays up with its defaults.
                        Log::new(Some(&status_tx))
                            .warn(format!("NFC startup action failed: {}", e));
                    }
                }
            }
            Err(e) => {
                // Non-fatal: no reader has been accepted yet, so there is nobody to
                // answer; the tag is up with its built-in defaults (wire fallback), so
                // this is WARN not ERROR. Tagged so an operator can tell a backend
                // outage at startup from the model choosing to configure nothing.
                let failure = WireFailure::classify(&e);
                Log::new(Some(&status_tx)).warn(format!(
                    "NFC decision=fail_closed_llm_error at startup (category={}): {e}. \
                     The tag is up with its built-in defaults and carries no \
                     model-supplied NDEF records.",
                    if failure.is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    }
                ));
            }
        }

        {
            let tag = tag_state.lock().await;
            debug!(
                "Virtual NFC tag configured: atr={}, ndef_records={}",
                hex::encode_upper(&tag.atr),
                tag.ndef_records.len()
            );
        }

        let accept_state = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                let (stream, remote_addr) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("NFC accept error: {e}"));
                        break;
                    }
                };

                let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);
                let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                Log::new(Some(&status_tx)).info(format!(
                    "NFC reader {connection_id} connected from {remote_addr}"
                ));

                {
                    use crate::state::server::{
                        ConnectionState, ConnectionStatus, ProtocolConnectionInfo,
                    };
                    let now = std::time::Instant::now();
                    let conn_state = ConnectionState {
                        id: connection_id,
                        remote_addr,
                        local_addr: local_addr_conn,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: now,
                        status: ConnectionStatus::Active,
                        status_changed_at: now,
                        protocol_info: ProtocolConnectionInfo::new(json!({
                            "transport": "vpcd",
                        })),
                    };
                    app_state
                        .add_connection_to_server(server_id, conn_state)
                        .await;
                }
                let _ = status_tx.send("__UPDATE_UI__".to_string());

                let llm_client = llm_client.clone();
                let conn_state = app_state.clone();
                let status_tx = status_tx.clone();
                let tag_state = tag_state.clone();
                let protocol = protocol.clone();
                tokio::spawn(async move {
                    if let Err(e) = Self::handle_reader(
                        stream,
                        connection_id,
                        server_id,
                        tag_state,
                        llm_client,
                        conn_state.clone(),
                        status_tx.clone(),
                        protocol,
                    )
                    .await
                    {
                        Log::new(Some(&status_tx))
                            .error(format!("NFC reader {} error: {}", connection_id, e));
                    }
                    conn_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    Log::new(Some(&status_tx))
                        .info(format!("NFC reader {connection_id} disconnected"));
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                });
            }
        });

        accept_state
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Serve one reader connection.
    ///
    /// No Idle/Processing/Accumulating state machine is needed here: the vpcd
    /// framing is explicit, and the next frame is only read after the current
    /// one has been answered, so a connection can never have two LLM calls in
    /// flight and there is no partial-read reassembly to do.
    #[allow(clippy::too_many_arguments)]
    async fn handle_reader(
        stream: TcpStream,
        connection_id: ConnectionId,
        server_id: ServerId,
        tag_state: Arc<Mutex<VirtualNfcTag>>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<NfcServerProtocol>,
    ) -> Result<()> {
        let (mut read_half, mut write_half) = tokio::io::split(stream);
        let mut buffer = vec![0u8; MAX_FRAME_LEN];
        let log = Log::new(Some(&status_tx));

        loop {
            let frame_len = match read_half.read_u16().await {
                Ok(len) => len as usize,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!("NFC reader {} closed the connection", connection_id);
                    return Ok(());
                }
                Err(e) => {
                    debug!("NFC reader {} read error: {}", connection_id, e);
                    return Ok(());
                }
            };

            if frame_len == 0 {
                log.debug(format!(
                    "NFC reader {} sent an empty frame; ignoring",
                    connection_id
                ));
                continue;
            }
            if frame_len > MAX_FRAME_LEN {
                // Attacker-controlled length: refuse rather than allocate. Handled
                // defensively (connection closed), so WARN not ERROR.
                log.warn(format!(
                    "NFC reader {} announced a {}-byte frame (max {}); closing",
                    connection_id, frame_len, MAX_FRAME_LEN
                ));
                return Ok(());
            }

            if let Err(e) = read_half.read_exact(&mut buffer[..frame_len]).await {
                debug!("NFC reader {} disconnected mid-frame: {}", connection_id, e);
                return Ok(());
            }
            let frame = buffer[..frame_len].to_vec();
            log.trace(format!(
                "NFC <- {} ({} bytes) from {}",
                hex::encode_upper(&frame),
                frame_len,
                connection_id
            ));

            if frame_len == 1 {
                Self::handle_control(
                    frame[0],
                    connection_id,
                    &tag_state,
                    &mut write_half,
                    &status_tx,
                )
                .await?;
                continue;
            }

            let command = match ApduCommand::parse(&frame) {
                Ok(command) => command,
                Err(e) => {
                    // Non-fatal: answered 6700 locally (wire fallback), so WARN.
                    log.warn(format!(
                        "NFC reader {} sent a malformed APDU: {}",
                        connection_id, e
                    ));
                    Self::write_response(
                        &mut write_half,
                        ApduResponse::new(Vec::new(), SW_WRONG_LENGTH.0, SW_WRONG_LENGTH.1),
                        connection_id,
                        &status_tx,
                    )
                    .await?;
                    continue;
                }
            };

            let response = Self::respond_to_command(
                &command,
                connection_id,
                server_id,
                &tag_state,
                &llm_client,
                &app_state,
                &status_tx,
                protocol.as_ref(),
            )
            .await;

            Self::write_response(&mut write_half, response, connection_id, &status_tx).await?;
        }
    }

    /// Answer a vpcd control frame.
    async fn handle_control(
        code: u8,
        connection_id: ConnectionId,
        tag_state: &Arc<Mutex<VirtualNfcTag>>,
        write_half: &mut WriteHalf<TcpStream>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        match code {
            VPCD_CTRL_ATR => {
                let atr = tag_state.lock().await.atr.clone();
                Log::new(Some(status_tx)).debug(format!(
                    "NFC reader {} requested ATR ({} bytes)",
                    connection_id,
                    atr.len()
                ));
                Self::write_frame(write_half, &atr, connection_id, status_tx).await
            }
            VPCD_CTRL_ON | VPCD_CTRL_OFF | VPCD_CTRL_RESET => {
                // Power on/off/reset are acknowledged by silence in the vpcd
                // protocol; the reader follows them with an ATR request.
                Log::new(Some(status_tx)).debug(format!(
                    "NFC reader {} sent power control 0x{:02X}",
                    connection_id, code
                ));
                Ok(())
            }
            other => {
                Log::new(Some(status_tx)).warn(format!(
                    "NFC reader {connection_id} sent unknown control code 0x{other:02X}"
                ));
                Ok(())
            }
        }
    }

    /// Raise the appropriate event for a command APDU and turn the handler's
    /// answer into a response APDU.
    ///
    /// Fails **closed**: if the handler errors, returns nothing, or returns an
    /// unusable `respond_to_apdu`, the tag answers `6F00`. It never falls
    /// through to a success status word.
    #[allow(clippy::too_many_arguments)]
    async fn respond_to_command(
        command: &ApduCommand,
        connection_id: ConnectionId,
        server_id: ServerId,
        tag_state: &Arc<Mutex<VirtualNfcTag>>,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &NfcServerProtocol,
    ) -> ApduResponse {
        let log = Log::new(Some(status_tx));
        let mut event_data = {
            let tag = tag_state.lock().await;
            json!({
                "tag_type": tag.tag_type,
                "uid": tag.uid,
                "ndef_records": tag.ndef_records,
            })
        };

        let map = match event_data.as_object_mut() {
            Some(map) => map,
            None => return ApduResponse::card_error(),
        };
        map.insert("cla".into(), json!(format!("{:02X}", command.cla)));
        map.insert("p1".into(), json!(format!("{:02X}", command.p1)));
        map.insert("p2".into(), json!(format!("{:02X}", command.p2)));
        map.insert("le".into(), json!(command.le));

        let is_select = command.is_select_by_aid();
        if is_select {
            map.insert(
                "application_id".into(),
                json!(hex::encode_upper(&command.data)),
            );
        } else {
            map.insert("ins".into(), json!(format!("{:02X}", command.ins)));
            map.insert("ins_name".into(), json!(command.ins_name()));
            map.insert("lc".into(), json!(command.data.len()));
            map.insert("data_hex".into(), json!(hex::encode_upper(&command.data)));
            if let Some(text) = printable_text(&command.data) {
                map.insert("data_text".into(), json!(text));
            }
        }

        let event = if is_select {
            Event::new(&NFC_TAG_SELECTED_EVENT, event_data)
        } else {
            Event::new(&NFC_APDU_RECEIVED_EVENT, event_data)
        };

        // FileOnly: the nfc_* event template renders the equivalent line to the TUI.
        log.debug(format!(
            "NFC {} {} on {} (Lc={}, Le={:?})",
            event.id(),
            command.ins_name(),
            connection_id,
            command.data.len(),
            command.le
        ));

        let execution = match call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol,
        )
        .await
        {
            Ok(execution) => execution,
            Err(e) => {
                // The backend erred. The reader still gets an answer, but only a
                // *category*: 6400 when the backend is saturated (retryable), 6F00
                // otherwise. The error itself goes to the log and the status stream —
                // never onto the wire.
                let failure = WireFailure::classify(&e);
                let response = ApduResponse::for_wire_failure(failure);
                // Non-fatal: the tag answers a card error (wire fallback), so WARN.
                log.warn(format!(
                    "NFC decision=fail_closed_llm_error for {} on {} (category={}, \
                     answering {}): {}",
                    event.id(),
                    connection_id,
                    if failure.is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    },
                    response.status_word(),
                    e
                ));
                return response;
            }
        };

        for message in execution.messages {
            let _ = status_tx.send(message);
        }

        let mut response = None;
        let mut results = Vec::new();
        flatten_results(execution.protocol_results, &mut results);
        for result in results {
            if let ActionResult::Custom { name, data } = result {
                if name != "respond_to_apdu" {
                    continue;
                }
                match decode_apdu_response(&data) {
                    Ok(decoded) => {
                        if response.is_none() {
                            response = Some(decoded);
                        } else {
                            log.warn(format!(
                                "NFC handler returned more than one respond_to_apdu for {}; \
                                 ignoring the extra one",
                                connection_id
                            ));
                        }
                    }
                    Err(e) => {
                        log.warn(format!(
                            "NFC decision=fail_closed_undecodable respond_to_apdu on {}: {}",
                            connection_id, e
                        ));
                    }
                }
            }
        }

        match response {
            Some(response) => {
                // The handler answered. Tag the two model outcomes apart from each other
                // and from the fail-closed paths below: a status word outside the
                // 61xx/62xx/63xx/90xx success-and-warning range is the model refusing,
                // which is a real answer and must not read like an outage in the log.
                let decision = if matches!(response.sw1, 0x90 | 0x61 | 0x62 | 0x63) {
                    "model_answer"
                } else {
                    "model_reject"
                };
                log.debug(format!(
                    "NFC decision={} for {} on {} (SW={})",
                    decision,
                    event.id(),
                    connection_id,
                    response.status_word()
                ));
                response
            }
            None => {
                // The handler ran and said nothing usable. Distinct from an LLM error
                // above: this is the model declining to speak, and it fails closed with
                // 6F00 — never a success.
                // Non-fatal: the tag answers 6F00 (wire fallback), so WARN.
                log.warn(format!(
                    "NFC decision=fail_closed_no_action: handler produced no respond_to_apdu \
                     for {} on {}; answering 6F00",
                    event.id(),
                    connection_id
                ));
                ApduResponse::card_error()
            }
        }
    }

    /// Apply a startup action to the virtual tag.
    async fn apply_startup_action(
        tag_state: Arc<Mutex<VirtualNfcTag>>,
        result: ActionResult,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        match result {
            ActionResult::Custom { name, data } => match name.as_str() {
                "set_atr" => {
                    let atr_hex = data["atr_hex"]
                        .as_str()
                        .ok_or_else(|| anyhow!("Missing atr_hex"))?;
                    let atr = hex::decode(atr_hex)
                        .map_err(|e| anyhow!("Invalid hex in atr_hex ({atr_hex}): {e}"))?;
                    if atr.is_empty() || atr.len() > MAX_FRAME_LEN {
                        return Err(anyhow!(
                            "ATR must be 1..={} bytes, got {}",
                            MAX_FRAME_LEN,
                            atr.len()
                        ));
                    }
                    tag_state.lock().await.atr = atr;
                    Log::new(Some(status_tx)).info(format!("NFC set ATR: {atr_hex}"));
                    Ok(())
                }
                "set_ndef_message" => {
                    let records = data["records"]
                        .as_array()
                        .ok_or_else(|| anyhow!("Missing records"))?;
                    tag_state.lock().await.ndef_records = records.clone();
                    Log::new(Some(status_tx))
                        .info(format!("NFC set NDEF message: {} record(s)", records.len()));
                    Ok(())
                }
                "respond_to_apdu" => Err(anyhow!(
                    "respond_to_apdu is only valid in response to nfc_apdu_received or \
                     nfc_tag_selected, not at startup"
                )),
                other => Err(anyhow!("Unhandled NFC startup action: {other}")),
            },
            ActionResult::NoAction => Ok(()),
            other => Err(anyhow!("Unhandled NFC startup action result: {other:?}")),
        }
    }

    /// Send a response APDU as a vpcd frame.
    async fn write_response(
        write_half: &mut WriteHalf<TcpStream>,
        response: ApduResponse,
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let status_word = response.status_word();
        let bytes = response.into_bytes();
        // FileOnly: the respond_to_apdu action template already reports the response
        // to the TUI.
        Log::new(Some(status_tx)).debug(format!(
            "NFC response SW={} ({} data byte(s)) to {}",
            status_word,
            bytes.len().saturating_sub(2),
            connection_id
        ));
        Self::write_frame(write_half, &bytes, connection_id, status_tx).await
    }

    /// Send one length-prefixed frame.
    async fn write_frame(
        write_half: &mut WriteHalf<TcpStream>,
        payload: &[u8],
        connection_id: ConnectionId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        if payload.len() > u16::MAX as usize {
            // Cannot be framed; answer the card error instead of truncating (wire
            // fallback), so WARN not ERROR.
            Log::new(Some(status_tx)).warn(format!(
                "NFC response for {} is {} bytes, too large to frame; sending 6F00",
                connection_id,
                payload.len()
            ));
            let fallback = ApduResponse::card_error().into_bytes();
            write_half.write_u16(fallback.len() as u16).await?;
            write_half.write_all(&fallback).await?;
            write_half.flush().await?;
            return Ok(());
        }

        Log::new(Some(status_tx)).trace(format!(
            "NFC -> {} ({} bytes) to {}",
            hex::encode_upper(payload),
            payload.len(),
            connection_id
        ));
        write_half.write_u16(payload.len() as u16).await?;
        write_half.write_all(payload).await?;
        write_half.flush().await?;
        trace!("NFC sent {} byte(s) to {}", payload.len(), connection_id);
        Ok(())
    }
}

/// `ExecutionResult::protocol_results` can nest via `ActionResult::Multiple`;
/// flatten so a nested `respond_to_apdu` is not silently dropped.
fn flatten_results(results: Vec<ActionResult>, out: &mut Vec<ActionResult>) {
    for result in results {
        match result {
            ActionResult::Multiple(nested) => flatten_results(nested, out),
            other => out.push(other),
        }
    }
}

/// Decode the normalised `respond_to_apdu` payload produced by
/// `NfcServerProtocol::execute_action`.
fn decode_apdu_response(data: &Value) -> Result<ApduResponse> {
    let data_hex = data
        .get("data_hex")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let body = if data_hex.is_empty() {
        Vec::new()
    } else {
        hex::decode(data_hex).map_err(|e| anyhow!("Invalid hex in data_hex ({data_hex}): {e}"))?
    };

    let sw1 = decode_status_byte(data, "sw1")?;
    let sw2 = decode_status_byte(data, "sw2")?;
    Ok(ApduResponse::new(body, sw1, sw2))
}

/// Decode one status byte of a normalised `respond_to_apdu` payload.
///
/// **There is deliberately no default.** `execute_action` already refuses an action that
/// names no status word; this is the second layer, so nothing between the model's answer and
/// the wire can invent one and 9000 cannot be produced except by a handler that spelled it
/// out. A missing field here is an error, which the caller logs as
/// `decision=fail_closed_undecodable` and answers `6F00`.
fn decode_status_byte(data: &Value, field: &str) -> Result<u8> {
    let Some(value) = data.get(field).and_then(|v| v.as_str()) else {
        return Err(anyhow!(
            "'{field}' is missing from the respond_to_apdu payload; a status word is never \
             defaulted, because the default would be success"
        ));
    };
    let bytes =
        hex::decode(value).map_err(|e| anyhow!("Invalid hex in '{field}' ({value}): {e}"))?;
    match bytes.as_slice() {
        [byte] => Ok(*byte),
        other => Err(anyhow!(
            "'{field}' must be exactly one hex byte, got {} byte(s)",
            other.len()
        )),
    }
}

/// Render an APDU data field as text when every byte is printable ASCII, so the
/// model gets something readable alongside the hex.
fn printable_text(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    if data
        .iter()
        .all(|&b| b.is_ascii_graphic() || b == b' ' || b == b'\t')
    {
        Some(String::from_utf8_lossy(data).to_string())
    } else {
        None
    }
}
