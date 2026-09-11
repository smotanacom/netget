//! Modbus TCP server.
//!
//! Owns framing and the connection state machine; the model owns the data. See
//! `src/server/modbus/CLAUDE.md`.

pub mod actions;
pub mod codec;

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;

use actions::{
    ModbusProtocol, MODBUS_READ_BITS_EVENT, MODBUS_READ_REGISTERS_EVENT,
    MODBUS_WRITE_REQUEST_EVENT, RESULT_BITS, RESULT_EXCEPTION, RESULT_REGISTERS, RESULT_WRITE_ACK,
};
use codec::{ModbusRequest, MAX_ADU_LEN};

/// Per-connection LLM processing state, mirroring `src/server/tcp/mod.rs`.
#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    /// No request in flight; ready to process.
    Idle,
    /// An LLM call is in flight; arriving bytes are queued into `buffer`.
    Processing,
    /// A partial ADU is buffered and we are waiting for the rest of it.
    Accumulating,
}

struct ConnectionData {
    state: ConnectionState,
    /// Doubles as the framing accumulator and the queue for bytes that arrive mid-call.
    buffer: Vec<u8>,
    write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
}

/// How much one connection may accumulate before we give up on it: eight ADUs' worth.
///
/// The buffer is both the framing accumulator and the queue for bytes that arrive while a
/// request is with the model, so this bound has to be enforced on *append* as well as in
/// the framing loop. Checking it only in the loop leaves the queue unbounded for the whole
/// duration of an LLM call, which at the shipped `--llm-queue-timeout` of 120s is long
/// enough for a peer to push gigabytes into it.
const MAX_BUFFERED: usize = MAX_ADU_LEN * 8;

/// Why a request is being answered the way it is.
///
/// The wire cannot carry the distinction: a model that deliberately refuses with exception
/// 0x04 and a model that never answered at all both put `04` on the wire. So the log
/// carries it instead, with stable tokens - `decision=fail_closed_` finds every request the
/// model did not actually answer. Mirrors `src/server/radius/`, which is the reference for
/// this split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// The model supplied the values, or accepted the write.
    ModelAnswer,
    /// The model deliberately refused, with a Modbus exception of its choosing.
    ModelReject,
    /// The specification determined the answer; the model was never asked.
    SpecReject,
    /// Addressed to a unit id this device does not answer for.
    UnitMismatch,
    /// The LLM backend failed. Nobody decided anything.
    FailClosedLlmError,
    /// The model was asked and returned nothing this request could use.
    FailClosedNoAction,
    /// The model answered a different question: bits for a register read, a write ack for
    /// a read, or the wrong number of values for the quantity requested.
    FailClosedWrongShape,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::ModelAnswer => "model_answer",
            Decision::ModelReject => "model_reject",
            Decision::SpecReject => "spec_reject",
            Decision::UnitMismatch => "unit_mismatch",
            Decision::FailClosedLlmError => "fail_closed_llm_error",
            Decision::FailClosedNoAction => "fail_closed_no_action",
            Decision::FailClosedWrongShape => "fail_closed_wrong_shape",
        }
    }

    fn is_fail_closed(self) -> bool {
        matches!(
            self,
            Decision::FailClosedLlmError
                | Decision::FailClosedNoAction
                | Decision::FailClosedWrongShape
        )
    }
}

/// Modbus TCP server.
pub struct ModbusServer;

impl ModbusServer {
    /// Bind, start accepting, and register the accept loop so `stop_server` releases the port.
    ///
    /// Returns `Err` if the socket cannot be bound, so `server_startup` records
    /// `ServerStatus::Error` rather than a server that is not listening.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        unit_id_filter: Option<u8>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("Modbus server listening on {local_addr}"));

        let connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let protocol = Arc::new(ModbusProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            Log::new(Some(&status_tx)).info(format!("Modbus accept loop started on {local_addr}"));

            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        info!(
                            "Modbus accepted connection {} from {}",
                            connection_id, remote_addr
                        );

                        let (mut read_half, write_half) = tokio::io::split(stream);
                        let write_half = Arc::new(Mutex::new(write_half));

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ServerConnectionState {
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
                                    protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                                        "state": "Idle",
                                        "unit_id_filter": unit_id_filter,
                                    })),
                                },
                            )
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        // Register before spawning the reader, so a client that writes
                        // immediately after connect() cannot have its first frame dropped.
                        connections.lock().await.insert(
                            connection_id,
                            ConnectionData {
                                state: ConnectionState::Idle,
                                buffer: Vec::new(),
                                write_half: write_half.clone(),
                            },
                        );

                        // Peer messaging: the dashboard's "disconnect this peer" injects
                        // `close_connection` into THIS connection through the same executor
                        // the LLM path uses. The four Modbus wire verbs are request-bound
                        // (they return `ActionResult::Custom` and are framed against the
                        // request's transaction id), so an injected one is reported as
                        // executed without writing — a Modbus server never initiates.
                        let peer_rx = crate::server::peer_support::register_peer_channel(
                            &app_state,
                            server_id,
                            connection_id.as_u32(),
                        )
                        .await;
                        crate::server::peer_support::spawn_peer_command_task(
                            peer_rx,
                            protocol.clone(),
                            app_state.clone(),
                            server_id,
                            connection_id.as_u32(),
                            write_half.clone(),
                            status_tx.clone(),
                        );

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let conns_clone = connections.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let mut read_buf = vec![0u8; 4096];
                            let log = Log::new(Some(&status_clone));
                            loop {
                                match read_half.read(&mut read_buf).await {
                                    Ok(0) => {
                                        conns_clone.lock().await.remove(&connection_id);
                                        state_clone
                                            .remove_peer_handle(server_id, connection_id.as_u32())
                                            .await;
                                        state_clone
                                            .close_connection_on_server(server_id, connection_id)
                                            .await;
                                        log.info(format!(
                                            "Modbus connection {connection_id} closed"
                                        ));
                                        let _ = status_clone.send("__UPDATE_UI__".to_string());
                                        break;
                                    }
                                    Ok(n) => {
                                        let data = read_buf[..n].to_vec();
                                        // Summary + full payload FileOnly: the modbus_* event
                                        // templates render the equivalent line to the TUI.
                                        log.debug(format!(
                                            "Modbus received {} bytes on {}",
                                            n, connection_id
                                        ));
                                        log.trace(format!(
                                            "Modbus received (hex): {}",
                                            hex::encode(&data)
                                        ));
                                        state_clone
                                            .update_connection_stats(
                                                server_id,
                                                connection_id,
                                                Some(n as u64),
                                                None,
                                                Some(1),
                                                None,
                                            )
                                            .await;

                                        let llm = llm_clone.clone();
                                        let st = state_clone.clone();
                                        let stx = status_clone.clone();
                                        let cs = conns_clone.clone();
                                        let pr = protocol_clone.clone();
                                        tokio::spawn(async move {
                                            Self::handle_data(
                                                connection_id,
                                                server_id,
                                                data,
                                                unit_id_filter,
                                                llm,
                                                st,
                                                stx,
                                                cs,
                                                pr,
                                            )
                                            .await;
                                        });
                                    }
                                    Err(e) => {
                                        error!("Modbus read error on {}: {}", connection_id, e);
                                        conns_clone.lock().await.remove(&connection_id);
                                        state_clone
                                            .remove_peer_handle(server_id, connection_id.as_u32())
                                            .await;
                                        state_clone
                                            .close_connection_on_server(server_id, connection_id)
                                            .await;
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!("Modbus accept error: {}", e);
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Drive the per-connection state machine over newly arrived bytes.
    #[allow(clippy::too_many_arguments)]
    async fn handle_data(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        data: Vec<u8>,
        unit_id_filter: Option<u8>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        protocol: Arc<ModbusProtocol>,
    ) {
        let log = Log::new(Some(&status_tx));

        // Append, and bail out early if a request is already in flight: the in-flight
        // invocation will pick these bytes up when it loops.
        {
            let mut conns = connections.lock().await;
            let Some(conn) = conns.get_mut(&connection_id) else {
                return; // Connection closed while we waited for the lock
            };
            conn.buffer.extend_from_slice(&data);

            // Enforced here as well as in the framing loop below: while a request is with
            // the model this function is the *only* code touching the buffer, and it
            // returns early, so a loop-only check would let a flooding peer grow the queue
            // without bound for the whole duration of the call.
            if conn.buffer.len() > MAX_BUFFERED {
                error!(
                    "Modbus {} buffered {} bytes without a complete frame; closing",
                    connection_id,
                    conn.buffer.len()
                );
                conn.buffer.clear();
                drop(conns);
                Self::close(
                    connection_id,
                    server_id,
                    &app_state,
                    &connections,
                    &status_tx,
                )
                .await;
                return;
            }

            if conn.state == ConnectionState::Processing {
                log.debug(format!(
                    "Queued {} bytes for Modbus {connection_id}",
                    data.len()
                ));
                return;
            }
            conn.state = ConnectionState::Processing;
        }

        loop {
            // Pull one complete ADU out of the accumulator.
            let parsed = {
                let mut conns = connections.lock().await;
                let Some(conn) = conns.get_mut(&connection_id) else {
                    return;
                };

                // Guard against a peer that never sends a parseable frame.
                if conn.buffer.len() > MAX_BUFFERED {
                    error!(
                        "Modbus {} buffered {} bytes without a complete frame; closing",
                        connection_id,
                        conn.buffer.len()
                    );
                    conn.buffer.clear();
                    drop(conns);
                    Self::close(
                        connection_id,
                        server_id,
                        &app_state,
                        &connections,
                        &status_tx,
                    )
                    .await;
                    return;
                }

                match codec::try_parse_adu(&conn.buffer) {
                    Ok(Some((adu, consumed))) => {
                        conn.buffer.drain(..consumed);
                        Ok(Some(adu))
                    }
                    Ok(None) => {
                        // Nothing more to do: park in Accumulating (or Idle when the
                        // accumulator is empty) and let the next read wake us.
                        conn.state = if conn.buffer.is_empty() {
                            ConnectionState::Idle
                        } else {
                            ConnectionState::Accumulating
                        };
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            };

            let adu = match parsed {
                Ok(Some(adu)) => adu,
                Ok(None) => return,
                Err(e) => {
                    // Neither error is answerable: we no longer know where the next frame
                    // starts, so the only honest signal is to close.
                    log.error(format!("Modbus framing error on {connection_id}: {e}"));
                    Self::close(
                        connection_id,
                        server_id,
                        &app_state,
                        &connections,
                        &status_tx,
                    )
                    .await;
                    return;
                }
            };

            let function_code = adu.pdu.first().copied().unwrap_or(0);

            // Unit routing, when the operator pinned this server to one unit id.
            if let Some(expected) = unit_id_filter {
                if adu.unit_id != expected {
                    warn!(
                        "Modbus {} addressed unit {} but this device answers for unit {} \
                         decision={}",
                        connection_id,
                        adu.unit_id,
                        expected,
                        Decision::UnitMismatch.as_str()
                    );
                    let pdu =
                        codec::encode_exception(function_code, codec::EXC_GATEWAY_TARGET_FAILED);
                    Self::send_pdu(
                        connection_id,
                        server_id,
                        adu.transaction_id,
                        adu.unit_id,
                        &pdu,
                        &app_state,
                        &connections,
                        &status_tx,
                    )
                    .await;
                    continue;
                }
            }

            // Spec-determined failures are answered here, with no model round-trip: an
            // unknown function code is always 0x01, a zero quantity always 0x03.
            let request = match codec::parse_request(&adu.pdu) {
                Ok(r) => r,
                Err(exception_code) => {
                    debug!(
                        "Modbus {} rejecting fc={:#04x} with exception {:#04x} ({}) decision={}",
                        connection_id,
                        function_code,
                        exception_code,
                        codec::exception_name(exception_code),
                        Decision::SpecReject.as_str()
                    );
                    let pdu = codec::encode_exception(function_code, exception_code);
                    Self::send_pdu(
                        connection_id,
                        server_id,
                        adu.transaction_id,
                        adu.unit_id,
                        &pdu,
                        &app_state,
                        &connections,
                        &status_tx,
                    )
                    .await;
                    continue;
                }
            };

            let (event_type, mut event_data) = Self::build_event(&adu, &request);
            event_data["unit_id"] = serde_json::json!(adu.unit_id);
            let event = Event::new(event_type, event_data);

            let (decision, pdu) = match call_llm(
                &llm_client,
                &app_state,
                server_id,
                Some(connection_id),
                &event,
                protocol.as_ref(),
            )
            .await
            {
                Ok(execution_result) => {
                    for msg in execution_result.messages {
                        let _ = status_tx.send(msg);
                    }
                    Self::pdu_from_results(
                        connection_id,
                        &request,
                        &execution_result.protocol_results,
                    )
                }
                Err(e) => {
                    // Non-fatal: the client still gets a device-failure exception (wire
                    // fallback), so this is WARN not ERROR. The error text stays in the
                    // log; the wire gets only the exception code.
                    log.warn(format!("Modbus LLM error on {connection_id}: {e}"));
                    // Fail closed and say so on the wire. Writing nothing would leave the
                    // client blocked until its own timeout with no diagnostic.
                    (
                        Decision::FailClosedLlmError,
                        codec::encode_exception(
                            request.function_code(),
                            codec::EXC_SERVER_DEVICE_FAILURE,
                        ),
                    )
                }
            };

            // Log the decision before writing it, and make the fail-closed paths loud.
            // On this protocol a fabricated value has physical consequences, so an
            // operator has to be able to tell "the device said so" from "nobody
            // answered" - and the wire cannot: both can be exception 0x04.
            let summary = format!(
                "Modbus {} {} unit={} @{} x{} decision={}",
                connection_id,
                request.function_name(),
                adu.unit_id,
                request.start_address(),
                request.quantity(),
                decision.as_str()
            );
            if decision.is_fail_closed() {
                log.error(format!(
                    "{summary} (answered with an exception because no usable answer was \
                     produced)"
                ));
            } else {
                log.debug(&summary);
            }

            Self::send_pdu(
                connection_id,
                server_id,
                adu.transaction_id,
                adu.unit_id,
                &pdu,
                &app_state,
                &connections,
                &status_tx,
            )
            .await;
        }
    }

    /// Choose the event type and build its data from a decoded request.
    fn build_event(
        adu: &codec::Adu,
        request: &ModbusRequest,
    ) -> (&'static crate::protocol::EventType, serde_json::Value) {
        let mut data = serde_json::json!({
            "unit_id": adu.unit_id,
            "function_code": request.function_code(),
            "function": request.function_name(),
            "start_address": request.start_address(),
            "quantity": request.quantity(),
        });

        match request {
            ModbusRequest::ReadCoils { .. } => {
                data["bit_type"] = serde_json::json!("coil");
                (&MODBUS_READ_BITS_EVENT, data)
            }
            ModbusRequest::ReadDiscreteInputs { .. } => {
                data["bit_type"] = serde_json::json!("discrete_input");
                (&MODBUS_READ_BITS_EVENT, data)
            }
            ModbusRequest::ReadHoldingRegisters { .. } => {
                data["register_type"] = serde_json::json!("holding");
                (&MODBUS_READ_REGISTERS_EVENT, data)
            }
            ModbusRequest::ReadInputRegisters { .. } => {
                data["register_type"] = serde_json::json!("input");
                (&MODBUS_READ_REGISTERS_EVENT, data)
            }
            ModbusRequest::WriteSingleCoil { value, .. } => {
                data["coil_values"] = serde_json::json!([value]);
                (&MODBUS_WRITE_REQUEST_EVENT, data)
            }
            ModbusRequest::WriteMultipleCoils { values, .. } => {
                data["coil_values"] = serde_json::json!(values);
                (&MODBUS_WRITE_REQUEST_EVENT, data)
            }
            ModbusRequest::WriteSingleRegister { value, .. } => {
                data["register_values"] = serde_json::json!([value]);
                (&MODBUS_WRITE_REQUEST_EVENT, data)
            }
            ModbusRequest::WriteMultipleRegisters { values, .. } => {
                data["register_values"] = serde_json::json!(values);
                (&MODBUS_WRITE_REQUEST_EVENT, data)
            }
        }
    }

    /// Turn the model's structured answer into the PDU to put on the wire.
    ///
    /// Fails closed: anything that is not a usable answer to *this* request becomes a
    /// `server device failure` exception rather than a plausible-looking default. A
    /// silent LLM and a model that answered the wrong question are both faults, and the
    /// client is told so.
    fn pdu_from_results(
        connection_id: ConnectionId,
        request: &ModbusRequest,
        results: &[ActionResult],
    ) -> (Decision, Vec<u8>) {
        let fc = request.function_code();

        for result in results {
            let ActionResult::Custom { name, data } = result else {
                continue;
            };

            match name.as_str() {
                RESULT_EXCEPTION => {
                    // `execute_action` has already checked this against the exception
                    // codes the specification defines, so the fallback is unreachable
                    // today. It is written as a fail-closed default rather than a cast so
                    // it stays correct if another producer of this result ever appears:
                    // `n as u8` would silently turn 0x104 into 0x04.
                    let code = data
                        .get("exception_code")
                        .and_then(|v| v.as_u64())
                        .filter(|n| *n <= u8::MAX as u64)
                        .map(|n| n as u8)
                        .unwrap_or(codec::EXC_SERVER_DEVICE_FAILURE);
                    return (Decision::ModelReject, codec::encode_exception(fc, code));
                }
                RESULT_BITS => {
                    if !request.is_bit_read() {
                        error!(
                            "Modbus {}: send_modbus_bits answered a {} request; bits only \
                             answer function codes 1 and 2",
                            connection_id,
                            request.function_name()
                        );
                        return (
                            Decision::FailClosedWrongShape,
                            codec::encode_exception(fc, codec::EXC_SERVER_DEVICE_FAILURE),
                        );
                    }
                    let values: Vec<bool> = data
                        .get("values")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_bool()).collect())
                        .unwrap_or_default();
                    if values.len() != request.quantity() as usize {
                        error!(
                            "Modbus {}: model returned {} bit value(s) for a request of {}; \
                             answering with a device failure rather than a truncated frame",
                            connection_id,
                            values.len(),
                            request.quantity()
                        );
                        return (
                            Decision::FailClosedWrongShape,
                            codec::encode_exception(fc, codec::EXC_SERVER_DEVICE_FAILURE),
                        );
                    }
                    return (
                        Decision::ModelAnswer,
                        codec::encode_bits_response(fc, &values),
                    );
                }
                RESULT_REGISTERS => {
                    if !request.is_register_read() {
                        error!(
                            "Modbus {}: send_modbus_registers answered a {} request; registers \
                             only answer function codes 3 and 4",
                            connection_id,
                            request.function_name()
                        );
                        return (
                            Decision::FailClosedWrongShape,
                            codec::encode_exception(fc, codec::EXC_SERVER_DEVICE_FAILURE),
                        );
                    }
                    // Filtered, never cast. `n as u16` would turn a model's 65736 into
                    // 200 and put it on the wire as a plant reading, which on this
                    // protocol is a fabricated measurement with physical consequences.
                    // Anything out of range is dropped here, which makes the count wrong,
                    // which the check below turns into an exception.
                    let values: Vec<u16> = data
                        .get("values")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_u64())
                                .filter(|n| *n <= u16::MAX as u64)
                                .map(|n| n as u16)
                                .collect()
                        })
                        .unwrap_or_default();
                    if values.len() != request.quantity() as usize {
                        error!(
                            "Modbus {}: model returned {} register value(s) for a request of \
                             {}; answering with a device failure rather than a truncated frame",
                            connection_id,
                            values.len(),
                            request.quantity()
                        );
                        return (
                            Decision::FailClosedWrongShape,
                            codec::encode_exception(fc, codec::EXC_SERVER_DEVICE_FAILURE),
                        );
                    }
                    return (
                        Decision::ModelAnswer,
                        codec::encode_registers_response(fc, &values),
                    );
                }
                RESULT_WRITE_ACK => {
                    if !request.is_write() {
                        error!(
                            "Modbus {}: send_modbus_write_ack answered a {} request, which is \
                             a read",
                            connection_id,
                            request.function_name()
                        );
                        return (
                            Decision::FailClosedWrongShape,
                            codec::encode_exception(fc, codec::EXC_SERVER_DEVICE_FAILURE),
                        );
                    }
                    return (Decision::ModelAnswer, codec::encode_write_ack(request));
                }
                _ => {}
            }
        }

        error!(
            "Modbus {}: no usable action returned for {}; answering with exception 0x04 \
             (server device failure)",
            connection_id,
            request.function_name()
        );
        (
            Decision::FailClosedNoAction,
            codec::encode_exception(fc, codec::EXC_SERVER_DEVICE_FAILURE),
        )
    }

    /// Frame a PDU and write it, updating counters and the dual logs.
    #[allow(clippy::too_many_arguments)]
    async fn send_pdu(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        transaction_id: u16,
        unit_id: u8,
        pdu: &[u8],
        app_state: &Arc<AppState>,
        connections: &Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let adu = codec::encode_adu(transaction_id, unit_id, pdu);

        let write_half = {
            let conns = connections.lock().await;
            conns.get(&connection_id).map(|c| c.write_half.clone())
        };
        let Some(write_half) = write_half else {
            return;
        };

        let mut write = write_half.lock().await;
        if let Err(e) = write.write_all(&adu).await {
            error!("Modbus write failed on {}: {}", connection_id, e);
            return;
        }
        if let Err(e) = write.flush().await {
            error!("Modbus flush failed on {}: {}", connection_id, e);
            return;
        }
        drop(write);

        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(adu.len() as u64),
                None,
                Some(1),
            )
            .await;

        // Summary + full payload FileOnly: the send_modbus_* action template already
        // reports the send to the TUI.
        let log = Log::new(Some(status_tx));
        log.debug(format!(
            "Modbus sent {} bytes to {} (txid={}, fc={:#04x})",
            adu.len(),
            connection_id,
            transaction_id,
            pdu.first().copied().unwrap_or(0)
        ));
        log.trace(format!("Modbus sent (hex): {}", hex::encode(&adu)));
    }

    /// Drop a connection from both the local map and `AppState`.
    async fn close(
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        app_state: &Arc<AppState>,
        connections: &Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let write_half = {
            let mut conns = connections.lock().await;
            conns.remove(&connection_id).map(|c| c.write_half)
        };
        if let Some(write_half) = write_half {
            let mut w = write_half.lock().await;
            let _ = w.shutdown().await;
        }
        app_state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        app_state
            .close_connection_on_server(server_id, connection_id)
            .await;
        Log::new(Some(status_tx)).info(format!("Modbus connection {connection_id} closed"));
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }
}
