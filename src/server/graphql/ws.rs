//! GraphQL over WebSocket, `graphql-transport-ws` (the graphql-ws protocol): connection
//! init/ack, ping/pong, subscribe/next/error/complete, with the protocol's close codes. Rust
//! owns the lifecycle, ids and execution; the handler supplies each subscription's events,
//! either in its first answer or later through the connection's peer handle.
use super::{actions, engine, outcome, Shared};
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use apollo_compiler::executable::OperationType;
use futures::{SinkExt, StreamExt};
use hyper::{header, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{
    tungstenite::{
        handshake::derive_accept_key,
        protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};

pub const SUBPROTOCOL: &str = "graphql-transport-ws";
/// graphql-ws closes a socket that has not sent `connection_init` in time with 4408; the
/// server's `connection_init_timeout_secs` defaults to this.
pub const CONNECTION_INIT_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_ID_LEN: usize = 128;

type Ws = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;
type Sink = Arc<Mutex<futures::stream::SplitSink<Ws, Message>>>;
type Active = Arc<Mutex<HashMap<String, Arc<engine::Prepared>>>>;

pub fn ws_config() -> WebSocketConfig {
    let mut c = WebSocketConfig::default();
    c.max_message_size = Some(engine::MAX_BODY_BYTES);
    c.max_frame_size = Some(engine::MAX_BODY_BYTES);
    c
}

pub fn is_upgrade<B>(req: &Request<B>) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Answer a WebSocket upgrade request. `Err` is the refusal to send instead.
pub fn handshake<B>(req: &Request<B>) -> Result<Response<()>, (u16, &'static str)> {
    let h = req.headers();
    let get = |n: header::HeaderName| h.get(n).and_then(|v| v.to_str().ok()).unwrap_or("");
    if req.method() != hyper::Method::GET {
        return Err((405, "WebSocket upgrades use GET"));
    }
    if !get(header::CONNECTION)
        .split(',')
        .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    {
        return Err((400, "Connection: Upgrade is required"));
    }
    if get(header::SEC_WEBSOCKET_VERSION) != "13" {
        return Err((426, "Sec-WebSocket-Version 13 is required"));
    }
    let key = get(header::SEC_WEBSOCKET_KEY);
    if key.is_empty() || key.len() > 64 {
        return Err((400, "Sec-WebSocket-Key is required"));
    }
    if !h
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == SUBPROTOCOL)
    {
        return Err((400, "the graphql-transport-ws subprotocol is required"));
    }
    let mut r = Response::new(());
    *r.status_mut() = hyper::StatusCode::SWITCHING_PROTOCOLS;
    let hs = r.headers_mut();
    hs.insert(
        header::UPGRADE,
        header::HeaderValue::from_static("websocket"),
    );
    hs.insert(
        header::CONNECTION,
        header::HeaderValue::from_static("Upgrade"),
    );
    hs.insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        header::HeaderValue::from_static(SUBPROTOCOL),
    );
    hs.insert(
        header::SEC_WEBSOCKET_ACCEPT,
        header::HeaderValue::from_str(&derive_accept_key(key.as_bytes()))
            .map_err(|_| (400, "bad key"))?,
    );
    Ok(r)
}

async fn send(sink: &Sink, v: Value) -> anyhow::Result<usize> {
    let text = v.to_string();
    let n = text.len();
    sink.lock().await.send(Message::Text(text)).await?;
    Ok(n)
}

async fn close(sink: &Sink, code: u16, reason: &str) {
    let mut s = sink.lock().await;
    let _ = s
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: reason.to_owned().into(),
        })))
        .await;
    let _ = s.close().await;
}

fn valid_id(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= MAX_ID_LEN && !s.chars().any(char::is_control))
}

/// Run one upgraded connection until either side closes.
pub(super) async fn session(shared: Arc<Shared>, id: ConnectionId, ws: Ws) {
    let ctx = &shared.ctx;
    let (sink, mut stream) = ws.split();
    let sink: Sink = Arc::new(Mutex::new(sink));
    let active: Active = Arc::default();
    Log::new(Some(&ctx.status_tx))
        .info(format!("GraphQL connection {id} upgraded to {SUBPROTOCOL}"));
    let peer_rx =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    let commands = ctx
        .state
        .spawn_server_task(
            ctx.server_id,
            peer_commands(shared.clone(), id, peer_rx, sink.clone(), active.clone()),
        )
        .await;
    let result = run(&shared, id, &sink, &mut stream, &active).await;
    commands.abort();
    ctx.state
        .remove_peer_handle(ctx.server_id, id.as_u32())
        .await;
    if let Err((code, reason)) = result {
        Log::new(Some(&ctx.status_tx))
            .warn(format!("GraphQL connection {id}: closing {code} {reason}"));
        close(&sink, code, &reason).await;
    } else {
        let _ = sink.lock().await.close().await;
    }
}

async fn run(
    shared: &Arc<Shared>,
    id: ConnectionId,
    sink: &Sink,
    stream: &mut futures::stream::SplitStream<Ws>,
    active: &Active,
) -> Result<(), (u16, String)> {
    let ctx = &shared.ctx;
    let deadline = tokio::time::Instant::now() + shared.init_timeout;
    let mut acked = false;
    loop {
        let next = if acked {
            stream.next().await
        } else {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(n) => n,
                Err(_) => return Err((4408, "Connection initialisation timeout".into())),
            }
        };
        let text = match next {
            None | Some(Err(_)) => return Ok(()),
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Close(_))) => return Ok(()),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Binary(_))) => return Err((4400, "Invalid message received".into())),
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(text.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let msg: Value = match serde_json::from_str(&text) {
            Ok(v @ Value::Object(_)) if engine::budget_ok(&v) => v,
            _ => return Err((4400, "Invalid message received".into())),
        };
        let io = |_| (1006u16, String::from("write failed"));
        match msg["type"].as_str() {
            Some("connection_init") => {
                if acked {
                    return Err((4429, "Too many initialisation requests".into()));
                }
                if msg
                    .get("payload")
                    .is_some_and(|p| !p.is_null() && !p.is_object())
                {
                    return Err((4400, "connection_init payload must be an object".into()));
                }
                acked = true;
                send(sink, json!({"type": "connection_ack"}))
                    .await
                    .map_err(io)?;
            }
            Some("ping") => {
                send(sink, json!({"type": "pong"})).await.map_err(io)?;
            }
            Some("pong") => {}
            Some("subscribe") => {
                if !acked {
                    return Err((4401, "Unauthorized".into()));
                }
                let Some(sub) = valid_id(&msg["id"]).map(str::to_owned) else {
                    return Err((4400, "subscribe needs an id".into()));
                };
                if active.lock().await.contains_key(&sub) {
                    return Err((4409, format!("Subscriber for {sub} already exists")));
                }
                subscribe(shared, id, sink, active, sub, &msg["payload"])
                    .await
                    .map_err(io)?;
            }
            Some("complete") => {
                if let Some(sub) = valid_id(&msg["id"]) {
                    if active.lock().await.remove(sub).is_some() {
                        Log::new(Some(&ctx.status_tx)).info(format!(
                            "GraphQL connection {id}: subscription {sub} completed by the client"
                        ));
                    }
                }
            }
            _ => return Err((4400, "Invalid message received".into())),
        }
    }
}

async fn subscribe(
    shared: &Arc<Shared>,
    id: ConnectionId,
    sink: &Sink,
    active: &Active,
    sub: String,
    payload: &Value,
) -> anyhow::Result<()> {
    let ctx = &shared.ctx;
    let errors = |e: Value| json!({"type": "error", "id": sub, "payload": e});
    let request = match payload {
        Value::Object(m) => super::decode_params(
            m.get("query").cloned(),
            m.get("operationName").cloned(),
            m.get("variables").cloned(),
        ),
        _ => Err("payload must be an object"),
    };
    let request = match request {
        Ok(r) => r,
        Err(e) => {
            send(sink, errors(json!([{"message": e}]))).await?;
            return Ok(());
        }
    };
    let prepared = match engine::prepare(
        &shared.schema,
        &request.query,
        request.operation_name.as_deref(),
        &request.variables,
    ) {
        Ok(p) => p,
        Err(e) => {
            outcome(ctx, id, "subscribe", "protocol_refusal");
            send(sink, errors(e.body()["errors"].clone())).await?;
            return Ok(());
        }
    };
    if prepared.operation_type != OperationType::Subscription {
        // A query or mutation over the socket: one `next`, then `complete`.
        match super::answer_operation(shared, id, &prepared, &request.query, "WS").await {
            Ok(body) => {
                send(sink, json!({"type": "next", "id": sub, "payload": body})).await?;
                send(sink, json!({"type": "complete", "id": sub})).await?;
            }
            Err(e) => {
                let text = crate::utils::wire_failure::WireFailure::classify(&e).text();
                send(sink, errors(json!([{"message": text}]))).await?;
            }
        }
        return Ok(());
    }
    {
        let mut a = active.lock().await;
        if a.len() >= MAX_SUBSCRIPTIONS {
            drop(a);
            outcome(ctx, id, "subscribe", "protocol_refusal");
            send(
                sink,
                errors(json!([{"message": format!("at most {MAX_SUBSCRIPTIONS} subscriptions per connection")}])),
            )
            .await?;
            return Ok(());
        }
        a.insert(sub.clone(), Arc::new(prepared));
    }
    let prepared = active
        .lock()
        .await
        .get(&sub)
        .cloned()
        .expect("just inserted");
    let event = Event::new(
        &actions::SUBSCRIPTION_EVENT,
        json!({
            "subscription_id": sub,
            "operation_name": prepared.operation_name,
            "query": request.query,
            "variables": prepared.variables_json(),
            "root_fields": prepared.root_fields(),
            "shape": prepared.shape(&shared.schema),
        }),
    );
    let answers = match super::ask(
        shared,
        id,
        event,
        "subscription",
        &["graphql_event", "graphql_complete", "graphql_error"],
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            active.lock().await.remove(&sub);
            let text = crate::utils::wire_failure::WireFailure::classify(&e).text();
            send(sink, errors(json!([{"message": text}]))).await?;
            return Ok(());
        }
    };
    let mut decision = "model_answer";
    for answer in answers {
        match apply(shared, sink, active, &sub, &answer).await {
            Ok(_) => {
                if answer["type"] == "graphql_error" {
                    decision = "model_reject";
                }
            }
            Err(e) => {
                decision = "fail_closed_invalid_reply";
                Log::new(Some(&ctx.status_tx)).warn(format!("GraphQL subscription {sub}: {e}"));
                if active.lock().await.remove(&sub).is_some() {
                    let text = "the subscription could not be served";
                    send(sink, errors(json!([{"message": text}]))).await?;
                }
                break;
            }
        }
    }
    outcome(ctx, id, "subscription", decision);
    Ok(())
}

/// Apply one handler action to a subscription. `default` names the subscription a start answer
/// belongs to; a peer action must name its own.
async fn apply(
    shared: &Shared,
    sink: &Sink,
    active: &Active,
    default: &str,
    answer: &Value,
) -> anyhow::Result<usize> {
    let sub = answer
        .get("subscription_id")
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned();
    anyhow::ensure!(
        default.is_empty() || sub == default,
        "a start answer can only feed its own subscription"
    );
    let prepared = active
        .lock()
        .await
        .get(&sub)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no active subscription {sub}"))?;
    match answer["type"].as_str() {
        Some("graphql_event") => {
            let body = super::execute_answer(shared, &prepared, answer)?;
            send(sink, json!({"type": "next", "id": sub, "payload": body})).await
        }
        Some("graphql_complete") => {
            active.lock().await.remove(&sub);
            send(sink, json!({"type": "complete", "id": sub})).await
        }
        Some("graphql_error") => {
            active.lock().await.remove(&sub);
            send(
                sink,
                json!({"type": "error", "id": sub, "payload": [super::refusal(answer)]}),
            )
            .await
        }
        Some("disconnect") => {
            close(sink, 1000, "closed by the server").await;
            Ok(0)
        }
        other => anyhow::bail!("{other:?} does not apply to a subscription"),
    }
}

async fn peer_commands(
    shared: Arc<Shared>,
    id: ConnectionId,
    mut rx: mpsc::Receiver<ClientCommand>,
    sink: Sink,
    active: Active,
) {
    while let Some(command) = rx.recv().await {
        let action = &command.action;
        let checked = actions::GraphqlProtocol::check_peer_action(action);
        let outcome = match checked {
            Err(e) => ClientSendOutcome::Rejected {
                error: e.to_string(),
            },
            Ok(()) if action["type"] == "disconnect" => {
                close(&sink, 1000, "closed by the server").await;
                ClientSendOutcome::Disconnected
            }
            Ok(()) => match apply(&shared, &sink, &active, "", action).await {
                Ok(n) => ClientSendOutcome::Sent { bytes_sent: n },
                Err(e) => ClientSendOutcome::Rejected {
                    error: e.to_string(),
                },
            },
        };
        shared
            .ctx
            .state
            .record_access_log(
                crate::state::AccessLogOwner::Server(shared.ctx.server_id.as_u32()),
                "GraphQL",
                Some(id.as_u32()),
                "injected_action",
                json!({"type": action["type"], "subscription_id": action["subscription_id"]}),
                vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
            )
            .await;
        let _ = command.reply_tx.send(Ok(outcome));
    }
}
