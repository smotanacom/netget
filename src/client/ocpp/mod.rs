//! Simulated OCPP-J charge point: one outstanding CALL each way, ids correlated by Rust.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::ocpp::frame::{self, Frame, Version};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::OcppClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_VERSION: &str = "1.6";
pub const DEFAULT_PREFIX: &str = "/";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Percent-encode a URL path segment (the charge point id).
fn encode_segment(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx
        .startup_params
        .as_ref()
        .context("OCPP client needs charge_point_id")?;
    let cp = p.get_string("charge_point_id")?;
    ensure!(
        !cp.is_empty() && cp.len() <= 48 && cp.bytes().all(|b| b.is_ascii_graphic() && b != b'/'),
        "charge_point_id must be 1..=48 printable ASCII characters without '/'"
    );
    let version_label = p
        .get_optional_string("ocpp_version")?
        .unwrap_or_else(|| DEFAULT_VERSION.into());
    let version =
        Version::from_label(&version_label).context("ocpp_version must be \"1.6\" or \"2.0.1\"")?;
    let prefix = p
        .get_optional_string("path_prefix")?
        .unwrap_or_else(|| DEFAULT_PREFIX.into());
    ensure!(
        prefix.starts_with('/')
            && prefix.ends_with('/')
            && prefix.len() <= 128
            && !prefix.contains(['?', '#', ' ']),
        "path_prefix must start and end with '/'"
    );
    let url = format!("ws://{}{prefix}{}", ctx.remote_addr, encode_segment(&cp));
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", version.subprotocol().parse()?);
    let (ws, response) = tokio::time::timeout(
        Duration::from_secs(30),
        tokio_tungstenite::connect_async_with_config(
            request,
            Some(crate::server::ocpp::ws_config()),
            false,
        ),
    )
    .await
    .context("OCPP connect deadline")??;
    let agreed = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ensure!(
        agreed == version.subprotocol(),
        "central system agreed subprotocol '{agreed}', not {}",
        version.subprotocol()
    );
    let local = match ws.get_ref() {
        tokio_tungstenite::MaybeTlsStream::Plain(s) => s.local_addr()?,
        _ => "0.0.0.0:0".parse()?,
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"charge_point_id": cp, "ocpp_version": version.label()}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = OcppClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("OCPP client handler: {e}"))
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
        let result = session(&session_ctx, version, ws, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("OCPP client ended: {e}"));
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

struct State {
    version: Version,
    next: u64,
    outbound: Option<(String, String, tokio::time::Instant)>,
    inbound: Option<(String, String)>,
}

impl State {
    /// Turn an action into the frame to send (`Ok(None)` for disconnect).
    fn frame_for(&mut self, action: &Value) -> Result<Option<String>> {
        match OcppClientProtocol.execute_action(action.clone())? {
            ClientActionResult::Disconnect => Ok(None),
            ClientActionResult::Custom { name, data } => match name.as_str() {
                "ocpp_call" => {
                    ensure!(
                        self.outbound.is_none(),
                        "a CALL is already outstanding; wait for its answer"
                    );
                    let act = data["action"].as_str().unwrap_or_default().to_owned();
                    frame::check_request(self.version, &act, &data["payload"])?;
                    self.next += 1;
                    let mid = format!("cp-{}", self.next);
                    let text = frame::encode(&Frame::Call {
                        id: mid.clone(),
                        action: act.clone(),
                        payload: data["payload"].clone(),
                    })?;
                    self.outbound = Some((mid, act, tokio::time::Instant::now()));
                    Ok(Some(text))
                }
                "ocpp_call_result" => {
                    let (mid, act) = self
                        .inbound
                        .clone()
                        .context("no central-system CALL is waiting for an answer")?;
                    frame::check_response(self.version, &act, &data["payload"])?;
                    let text = frame::encode(&Frame::Result {
                        id: mid,
                        payload: data["payload"].clone(),
                    })?;
                    self.inbound = None;
                    Ok(Some(text))
                }
                _ => {
                    let (mid, _) = self
                        .inbound
                        .clone()
                        .context("no central-system CALL is waiting for an answer")?;
                    let code = data["code"].as_str().unwrap_or_default();
                    frame::validate_error_code(self.version, code)?;
                    let details = if data["details"].is_object() {
                        data["details"].clone()
                    } else {
                        json!({})
                    };
                    let text = frame::encode(&Frame::Error {
                        id: mid,
                        code: code.into(),
                        description: data["description"].as_str().unwrap_or("").into(),
                        details,
                    })?;
                    self.inbound = None;
                    Ok(Some(text))
                }
            },
            _ => bail!("unsupported action"),
        }
    }
}

async fn session(
    ctx: &ConnectContext,
    version: Version,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    let mut st = State {
        version,
        next: 0,
        outbound: None,
        inbound: None,
    };
    loop {
        let deadline = st.outbound.as_ref().map(|(_, _, at)| *at + CALL_TIMEOUT);
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending::<()>().await } } => {
                let (mid, act, _) = st.outbound.take().expect("deadline implies outbound");
                bail!("central system did not answer {act} ({mid}) within {}s", CALL_TIMEOUT.as_secs());
            }
            m = stream.next() => {
                let text = match m {
                    None => return Ok(()),
                    Some(m) => match m? {
                        Message::Text(t) => t,
                        Message::Close(_) => return Ok(()),
                        Message::Binary(_) => bail!("OCPP-J is text only; binary frame received"),
                        _ => continue,
                    },
                };
                match frame::parse(&text) {
                    Err((mid, why)) => {
                        let _ = sink.send(Message::Text(frame::error_frame(mid.as_deref().unwrap_or("-1"), version.formation_code(), &why))).await;
                    }
                    Ok(Frame::Call { id, action, payload }) => {
                        if st.inbound.is_some() {
                            sink.send(Message::Text(frame::error_frame(&id, "GenericError", "a central-system CALL is already being answered"))).await?;
                            continue;
                        }
                        if let Err(e) = frame::check_request(version, &action, &payload) {
                            sink.send(Message::Text(frame::error_frame(&id, version.occurrence_code(), &e.to_string()))).await?;
                            continue;
                        }
                        st.inbound = Some((id.clone(), action.clone()));
                        events.try_send(Event::new(&actions::CSMS_CALL_EVENT, json!({"action": action, "message_id": id, "payload": payload}))).context("OCPP event queue full")?;
                    }
                    Ok(Frame::Result { id, payload }) => {
                        if let Some((mid, act, _)) = st.outbound.take_if(|(mid, _, _)| *mid == id) {
                            events.try_send(Event::new(&actions::RESPONSE_EVENT, json!({"action": act, "message_id": mid, "payload": payload}))).context("OCPP event queue full")?;
                        }
                    }
                    Ok(Frame::Error { id, code, description, details }) => {
                        if let Some((mid, act, _)) = st.outbound.take_if(|(mid, _, _)| *mid == id) {
                            events.try_send(Event::new(&actions::RESPONSE_EVENT, json!({"action": act, "message_id": mid, "error": {"code": code, "description": description, "details": details}}))).context("OCPP event queue full")?;
                        }
                    }
                }
                continue;
            }
        };
        let result = st.frame_for(&action);
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "OCPP",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "action": action["action"]}),
                    vec![json!({"ok": result.is_ok()})],
                )
                .await;
        }
        match result {
            Ok(None) => {
                let _ = sink.close().await;
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(Some(text)) => {
                let n = text.len();
                let sent =
                    tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Text(text)))
                        .await
                        .context("OCPP write deadline")
                        .and_then(|r| r.map_err(Into::into));
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        sent.as_ref()
                            .map(|_| ClientSendOutcome::Sent { bytes_sent: n })
                            .map_err(|e| anyhow::anyhow!(e.to_string())),
                    );
                }
                sent?;
            }
            Err(e) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                } else {
                    Log::new(Some(&ctx.status_tx)).warn(format!("OCPP client action refused: {e}"));
                }
            }
        }
    }
}
