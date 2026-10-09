//! OCPP-J central system (CSMS). Rust negotiates the subprotocol, frames every RPC message,
//! correlates ids and enforces one outstanding CALL per direction; the handler decides each
//! answer and any CSMS-initiated call.
pub mod actions;
pub mod frame;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, ensure, Context, Result};
use frame::{Frame, Version};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_VERSIONS: [&str; 2] = ["1.6", "2.0.1"];
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

struct Shared {
    ctx: SpawnContext,
    versions: Vec<Version>,
    call_timeout: Duration,
}

type Sink = Arc<
    Mutex<
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            Message,
        >,
    >,
>;

/// The CSMS-initiated CALL awaiting an answer on one connection.
#[derive(Default)]
struct Pending {
    call: Option<(String, String, tokio::time::Instant)>,
    next: u64,
}

pub fn ws_config() -> WebSocketConfig {
    let mut c = WebSocketConfig::default();
    c.max_message_size = Some(frame::MAX_MESSAGE_BYTES);
    c.max_frame_size = Some(frame::MAX_MESSAGE_BYTES);
    c
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let versions = match p
        .map(|p| p.get_optional_array("ocpp_versions"))
        .transpose()?
        .flatten()
    {
        None => DEFAULT_VERSIONS
            .iter()
            .map(|v| Version::from_label(v).expect("known"))
            .collect(),
        Some(list) => {
            let v: Vec<Version> = list
                .iter()
                .map(|v| {
                    v.as_str()
                        .and_then(Version::from_label)
                        .context("ocpp_versions entries must be \"1.6\" or \"2.0.1\"")
                })
                .collect::<Result<_>>()?;
            ensure!(!v.is_empty(), "ocpp_versions must not be empty");
            v
        }
    };
    let call_timeout = p
        .map(|p| p.get_optional_u64("call_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(CALL_TIMEOUT.as_secs());
    ensure!(
        (1..=300).contains(&call_timeout),
        "call_timeout_secs must be 1..=300"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "OCPP CSMS listening on ws://{local}/<charge point id> ({})",
        versions
            .iter()
            .map(|v| v.subprotocol())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        versions,
        call_timeout: Duration::from_secs(call_timeout),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (socket, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "OCPP", Some(&shared.ctx.status_tx)).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: local,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: now,
                        status: ConnectionStatus::Active,
                        status_changed_at: now,
                        protocol_info: ProtocolConnectionInfo::empty(),
                    },
                )
                .await;
            let child = shared.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = connection(&child, id, socket).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("OCPP connection {id} ended: {e}"));
                    }
                    child
                        .ctx
                        .state
                        .remove_peer_handle(server_id, id.as_u32())
                        .await;
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("OCPP connection {id} action={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn send(shared: &Shared, id: ConnectionId, sink: &Sink, text: String) -> Result<usize> {
    let n = text.len();
    tokio::time::timeout(
        Duration::from_secs(10),
        sink.lock().await.send(Message::Text(text)),
    )
    .await
    .context("OCPP write deadline")??;
    shared
        .ctx
        .state
        .update_connection_stats(
            shared.ctx.server_id,
            id,
            None,
            Some(n as u64),
            None,
            Some(1),
        )
        .await;
    Ok(n)
}

async fn decide(shared: &Shared, id: ConnectionId, event: Event, accept: &[&str]) -> Result<Value> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::OcppProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        bail!("OCPP handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, mut data } if accept.contains(&name.as_str()) => {
                data["type"] = json!(name);
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("OCPP handler supplied more than one answer");
                }
            }
            ActionResult::CloseConnection if accept.contains(&"disconnect") => {
                found = Some(json!({"type": "disconnect"}));
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    found
        .context("OCPP handler did not answer")
        .inspect_err(|_| outcome(ctx, id, event.id(), "model_silent"))
}

async fn connection(
    shared: &Arc<Shared>,
    id: ConnectionId,
    socket: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let chosen: Arc<std::sync::Mutex<Option<(Version, String)>>> = Arc::default();
    let slot = chosen.clone();
    let offered = shared.versions.clone();
    let callback = move |request: &Request,
                         mut response: Response|
          -> std::result::Result<Response, ErrorResponse> {
        let refuse = |code: u16, why: &str| {
            let mut r = ErrorResponse::new(Some(why.to_owned()));
            *r.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::from_u16(code)
                .unwrap_or(tokio_tungstenite::tungstenite::http::StatusCode::BAD_REQUEST);
            r
        };
        let cp = frame::charge_point_id(request.uri().path())
            .map_err(|e| refuse(404, &e.to_string()))?;
        let asked: Vec<&str> = request
            .headers()
            .get_all("Sec-WebSocket-Protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .collect();
        // The server's preference order wins among what the charge point offered.
        let Some(version) = offered
            .iter()
            .copied()
            .find(|v| asked.contains(&v.subprotocol()))
        else {
            return Err(refuse(
                400,
                "no supported OCPP subprotocol offered (ocpp1.6, ocpp2.0.1)",
            ));
        };
        response.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            version.subprotocol().parse().expect("static header"),
        );
        *slot.lock().expect("handshake slot") = Some((version, cp));
        Ok(response)
    };
    let ws = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_hdr_async_with_config(socket, callback, Some(ws_config())),
    )
    .await
    .context("OCPP handshake deadline")??;
    let (version, cp) = chosen
        .lock()
        .expect("handshake slot")
        .clone()
        .context("handshake completed without a negotiated version")?;
    ctx.state
        .with_server_mut(ctx.server_id, |s| {
            if let Some(c) = s.connections.get_mut(&id) {
                c.protocol_info = ProtocolConnectionInfo::new(
                    json!({"charge_point_id": cp, "ocpp_version": version.label()}),
                );
            }
        })
        .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "OCPP charge point {cp} connected (connection {id}, OCPP {})",
        version.label()
    ));
    let (sink, mut stream) = ws.split();
    let sink: Sink = Arc::new(Mutex::new(sink));
    let pending: Arc<Mutex<Pending>> = Arc::default();
    let peer_rx =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    let commands = ctx
        .state
        .spawn_server_task(
            ctx.server_id,
            peer_commands(
                shared.clone(),
                id,
                version,
                peer_rx,
                sink.clone(),
                pending.clone(),
            ),
        )
        .await;
    let result = async {
        loop {
            // A pending CSMS call that outlived its timeout is forgotten.
            {
                let mut p = pending.lock().await;
                if p.call.as_ref().is_some_and(|(_, _, at)| at.elapsed() > shared.call_timeout) {
                    let (mid, action, _) = p.call.take().expect("checked");
                    Log::new(Some(&ctx.status_tx)).warn(format!("OCPP {cp}: {action} ({mid}) unanswered after {}s", shared.call_timeout.as_secs()));
                }
            }
            let message = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Err(_) => bail!("charge point idle for {}s", IDLE_TIMEOUT.as_secs()),
                Ok(None) => return Ok(()),
                Ok(Some(m)) => m?,
            };
            let text = match message {
                Message::Text(t) => t,
                Message::Close(_) => return Ok(()),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Binary(_) => bail!("OCPP-J is text only; binary frame received"),
            };
            ctx.state.update_connection_stats(ctx.server_id, id, Some(text.len() as u64), None, Some(1), None).await;
            match frame::parse(&text) {
                Err((Some(mid), why)) => {
                    outcome(ctx, id, "frame", "protocol_refusal");
                    send(shared, id, &sink, frame::error_frame(&mid, version.formation_code(), &why)).await?;
                }
                Err((None, why)) => {
                    outcome(ctx, id, "frame", "protocol_refusal");
                    send(shared, id, &sink, frame::error_frame("-1", version.formation_code(), &why)).await?;
                }
                Ok(Frame::Call { id: mid, action, payload }) => {
                    let reply = answer_call(shared, id, version, &cp, &mid, &action, payload).await;
                    send(shared, id, &sink, reply).await?;
                }
                Ok(Frame::Result { id: mid, payload }) => {
                    if let Some(action) = take_pending(&pending, &mid).await {
                        let data = json!({"charge_point_id": cp, "action": action, "message_id": mid, "payload": payload});
                        if let Some(next) = react(shared, id, data).await {
                            if next["type"] == "disconnect" {
                                return Ok(());
                            }
                            start_call(shared, id, version, &sink, &pending, &next).await?;
                        }
                    }
                }
                Ok(Frame::Error { id: mid, code, description, details }) => {
                    if let Some(action) = take_pending(&pending, &mid).await {
                        let data = json!({"charge_point_id": cp, "action": action, "message_id": mid, "error": {"code": code, "description": description, "details": details}});
                        if let Some(next) = react(shared, id, data).await {
                            if next["type"] == "disconnect" {
                                return Ok(());
                            }
                            start_call(shared, id, version, &sink, &pending, &next).await?;
                        }
                    }
                }
            }
        }
    }
    .await;
    commands.abort();
    let _ = sink.lock().await.close().await;
    result
}

async fn take_pending(pending: &Mutex<Pending>, mid: &str) -> Option<String> {
    let mut p = pending.lock().await;
    match &p.call {
        Some((id, _, _)) if id == mid => p.call.take().map(|(_, action, _)| action),
        // An answer to nothing pending is ignored (OCPP-J §4.1.4).
        _ => None,
    }
}

async fn answer_call(
    shared: &Shared,
    id: ConnectionId,
    version: Version,
    cp: &str,
    mid: &str,
    action: &str,
    payload: Value,
) -> String {
    let ctx = &shared.ctx;
    if let Err(e) = frame::check_request(version, action, &payload) {
        outcome(ctx, id, action, "protocol_refusal");
        return frame::error_frame(mid, version.occurrence_code(), &e.to_string());
    }
    let event = Event::new(
        &actions::CALL_EVENT,
        json!({"charge_point_id": cp, "ocpp_version": version.label(), "action": action, "message_id": mid, "payload": payload}),
    );
    let failed = || {
        frame::error_frame(
            mid,
            "InternalError",
            "the central system cannot answer right now",
        )
    };
    match decide(shared, id, event, &["ocpp_call_result", "ocpp_call_error"]).await {
        Err(_) => failed(),
        Ok(v) if v["type"] == "ocpp_call_result" => {
            match frame::check_response(version, action, &v["payload"]).and_then(|_| {
                frame::encode(&Frame::Result {
                    id: mid.into(),
                    payload: v["payload"].clone(),
                })
            }) {
                Ok(text) => {
                    outcome(ctx, id, action, "model_answer");
                    text
                }
                Err(_) => {
                    outcome(ctx, id, action, "fail_closed_invalid_reply");
                    failed()
                }
            }
        }
        Ok(v) => {
            let code = v["code"].as_str().unwrap_or("");
            match frame::validate_error_code(version, code) {
                Ok(()) => {
                    outcome(ctx, id, action, "model_reject");
                    let details = if v["details"].is_object() {
                        v["details"].clone()
                    } else {
                        json!({})
                    };
                    frame::encode(&Frame::Error {
                        id: mid.into(),
                        code: code.into(),
                        description: v["description"].as_str().unwrap_or("").into(),
                        details,
                    })
                    .unwrap_or_else(|_| failed())
                }
                Err(_) => {
                    outcome(ctx, id, action, "fail_closed_invalid_reply");
                    failed()
                }
            }
        }
    }
}

/// Let the handler react to a charge point's answer; returns a follow-up call or disconnect.
async fn react(shared: &Shared, id: ConnectionId, data: Value) -> Option<Value> {
    let label = data["action"].as_str().unwrap_or("").to_owned();
    match decide(
        shared,
        id,
        Event::new(&actions::CALL_RESPONSE_EVENT, data),
        &["ocpp_send_call", "disconnect"],
    )
    .await
    {
        Ok(v) => {
            outcome(&shared.ctx, id, &label, "model_answer");
            Some(v)
        }
        // Silence after a response is fine: nothing more to send.
        Err(_) => None,
    }
}

async fn start_call(
    shared: &Shared,
    id: ConnectionId,
    version: Version,
    sink: &Sink,
    pending: &Mutex<Pending>,
    call: &Value,
) -> Result<usize> {
    actions::validate_answer(call)?;
    let action = call["action"].as_str().unwrap_or_default().to_owned();
    frame::check_request(version, &action, &call["payload"])?;
    let mut p = pending.lock().await;
    ensure!(
        p.call.is_none(),
        "a CSMS call is already outstanding on this connection; wait for its answer"
    );
    p.next += 1;
    let mid = format!("csms-{}", p.next);
    let text = frame::encode(&Frame::Call {
        id: mid.clone(),
        action: action.clone(),
        payload: call["payload"].clone(),
    })?;
    p.call = Some((mid, action, tokio::time::Instant::now()));
    drop(p);
    send(shared, id, sink, text).await
}

async fn peer_commands(
    shared: Arc<Shared>,
    id: ConnectionId,
    version: Version,
    mut rx: mpsc::Receiver<ClientCommand>,
    sink: Sink,
    pending: Arc<Mutex<Pending>>,
) {
    while let Some(command) = rx.recv().await {
        let outcome: Result<ClientSendOutcome> = match command.action["type"].as_str() {
            Some("ocpp_send_call") => {
                match start_call(&shared, id, version, &sink, &pending, &command.action).await {
                    Ok(n) => Ok(ClientSendOutcome::Sent { bytes_sent: n }),
                    Err(e) => Ok(ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    }),
                }
            }
            Some("disconnect") => {
                let _ = sink.lock().await.close().await;
                Ok(ClientSendOutcome::Disconnected)
            }
            _ => Ok(ClientSendOutcome::Rejected {
                error: "OCPP peers accept ocpp_send_call or disconnect".into(),
            }),
        };
        shared
            .ctx
            .state
            .record_access_log(
                crate::state::AccessLogOwner::Server(shared.ctx.server_id.as_u32()),
                "OCPP",
                Some(id.as_u32()),
                "injected_action",
                json!({"type": command.action["type"], "action": command.action["action"]}),
                vec![outcome
                    .as_ref()
                    .map(|o| serde_json::to_value(o).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null)],
            )
            .await;
        let _ = command.reply_tx.send(outcome);
    }
}
