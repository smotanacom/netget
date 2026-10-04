//! GraphQL over HTTP client.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::graphql::{engine, ws, GRAPHQL_RESPONSE};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::GraphqlClientProtocol;
use anyhow::{ensure, Context, Result};
use apollo_compiler::executable::OperationType;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

pub const DEFAULT_INTROSPECT: bool = true;
const TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT: &str = "application/graphql-response+json, application/json;q=0.9";

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let endpoint = p
        .map(|p| p.get_optional_string("endpoint"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| crate::server::graphql::DEFAULT_ENDPOINT.to_owned());
    ensure!(
        endpoint.starts_with('/') && endpoint.len() <= 256 && !endpoint.contains(['#', ' ']),
        "endpoint must be an absolute path"
    );
    let introspect = p
        .map(|p| p.get_optional_bool("introspect"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_INTROSPECT);
    let url = format!("http://{}{endpoint}", ctx.remote_addr);
    crate::client::http_fetch::check_url(&url)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none()),
            &url,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = FetchClient::transport(TIMEOUT);
    let fetch = fetch
        .with_max_body(engine::MAX_BODY_BYTES)
        .with_user_agent("netget-graphql");
    let mut connected = json!({"url": url});
    if introspect {
        match exchange(
            &fetch,
            &url,
            engine::CLIENT_INTROSPECTION,
            None,
            &json!({}),
            false,
        )
        .await
        {
            Ok(r) if r["data"].is_object() && r.get("errors").is_none() => {
                connected["root_fields"] = engine::root_signatures(&r["data"]);
            }
            Ok(r) => {
                connected["introspection_error"] = json!(r
                    .get("error")
                    .or_else(|| r["errors"].get(0).map(|e| &e["message"]))
                    .and_then(Value::as_str)
                    .unwrap_or("introspection gave no schema"))
            }
            Err(e) => bail_connect(&e)?,
        }
    }
    let local: SocketAddr = "0.0.0.0:0".parse()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, connected))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = GraphqlClientProtocol;
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
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("GraphQL client handler: {e}")),
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &fetch, &url, external, internal_rx, event_tx).await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("GraphQL client ended: {e}"));
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

/// A transport failure on the first request means there is no server to talk to.
fn bail_connect(e: &anyhow::Error) -> Result<()> {
    Err(anyhow::anyhow!("GraphQL endpoint unreachable: {e}"))
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The graphql-transport-ws socket, opened on the first subscription.
struct Socket {
    sink: futures::stream::SplitSink<WsStream, Message>,
    stream: futures::stream::SplitStream<WsStream>,
    /// Active subscriptions: id → operation name.
    active: HashMap<String, Option<String>>,
    next_id: u64,
}

async fn send_ws(sink: &mut futures::stream::SplitSink<WsStream, Message>, v: Value) -> Result<()> {
    sink.send(Message::Text(v.to_string())).await?;
    Ok(())
}

/// Connect with the graphql-transport-ws subprotocol and complete connection_init/ack.
async fn open_socket(url: &str) -> Result<Socket> {
    let ws_url = url.replacen("http://", "ws://", 1);
    let mut request = ws_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        tokio_tungstenite::tungstenite::http::HeaderValue::from_static(ws::SUBPROTOCOL),
    );
    let (socket, response) = tokio::time::timeout(
        TIMEOUT,
        tokio_tungstenite::connect_async_with_config(request, Some(ws::ws_config()), false),
    )
    .await
    .context("WebSocket connect timed out")??;
    ensure!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            == Some(ws::SUBPROTOCOL),
        "the server did not agree to graphql-transport-ws"
    );
    let (mut sink, mut stream) = socket.split();
    send_ws(&mut sink, json!({"type": "connection_init", "payload": {}})).await?;
    let deadline = tokio::time::Instant::now() + ws::CONNECTION_INIT_TIMEOUT;
    loop {
        let message = tokio::time::timeout_at(deadline, stream.next())
            .await
            .context("no connection_ack in time")?
            .context("socket closed before connection_ack")??;
        let Message::Text(text) = message else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).context("server sent a non-JSON message")?;
        match v["type"].as_str() {
            Some("connection_ack") => break,
            Some("ping") => send_ws(&mut sink, json!({"type": "pong"})).await?,
            Some("pong") => {}
            other => anyhow::bail!("expected connection_ack, got {other:?}"),
        }
    }
    Ok(Socket {
        sink,
        stream,
        active: HashMap::new(),
        next_id: 1,
    })
}

enum Step {
    Command(Option<ClientCommand>),
    Action(Option<Value>),
    Socket(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
}

fn reply(command: Option<ClientCommand>, outcome: Result<ClientSendOutcome>) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, outcome);
    }
}

fn rejected(error: impl ToString) -> Result<ClientSendOutcome> {
    Ok(ClientSendOutcome::Rejected {
        error: error.to_string(),
    })
}

async fn session(
    ctx: &ConnectContext,
    fetch: &FetchClient,
    url: &str,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut socket: Option<Socket> = None;
    loop {
        let step = {
            let incoming = async {
                match socket.as_mut() {
                    Some(s) => s.stream.next().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                c = external.recv() => Step::Command(c),
                a = internal.recv() => Step::Action(a),
                m = incoming => Step::Socket(m),
            }
        };
        let (action, command) = match step {
            Step::Command(Some(c)) => (c.action.clone(), Some(c)),
            Step::Action(Some(a)) => (a, None),
            Step::Command(None) | Step::Action(None) => return Ok(()),
            Step::Socket(message) => {
                if !socket_message(ctx, &mut socket, message, &events).await? {
                    socket = None;
                }
                continue;
            }
        };
        match GraphqlClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(mut s) = socket.take() {
                    let _ = s.sink.close().await;
                }
                reply(command, Ok(ClientSendOutcome::Disconnected));
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                reply(command, rejected(e));
                continue;
            }
        }
        match action["type"].as_str() {
            Some("graphql_subscribe") => {
                let outcome = subscribe(url, &mut socket, &action).await;
                reply(command, outcome);
            }
            Some("graphql_unsubscribe") => {
                let sub = action["subscription_id"].as_str().unwrap_or_default();
                let outcome = match socket.as_mut() {
                    Some(s) if s.active.contains_key(sub) => {
                        s.active.remove(sub);
                        send_ws(&mut s.sink, json!({"type": "complete", "id": sub}))
                            .await
                            .map(|()| ClientSendOutcome::Sent {
                                bytes_sent: sub.len(),
                            })
                    }
                    _ => rejected(format!("no active subscription {sub}")),
                };
                reply(command, outcome);
            }
            _ => query(ctx, fetch, url, &action, command, &events).await?,
        }
    }
}

async fn subscribe(
    url: &str,
    socket: &mut Option<Socket>,
    action: &Value,
) -> Result<ClientSendOutcome> {
    if socket.is_none() {
        match open_socket(url).await {
            Ok(s) => *socket = Some(s),
            Err(e) => return rejected(format!("graphql-transport-ws connection failed: {e}")),
        }
    }
    let s = socket.as_mut().expect("opened above");
    if s.active.len() >= ws::MAX_SUBSCRIPTIONS {
        return rejected(format!("at most {} subscriptions", ws::MAX_SUBSCRIPTIONS));
    }
    let sub = s.next_id.to_string();
    s.next_id += 1;
    let mut payload = json!({"query": action["query"]});
    if let Some(n) = action["operation_name"].as_str() {
        payload["operationName"] = json!(n);
    }
    if action["variables"]
        .as_object()
        .is_some_and(|v| !v.is_empty())
    {
        payload["variables"] = action["variables"].clone();
    }
    let message = json!({"type": "subscribe", "id": sub, "payload": payload});
    let n = message.to_string().len();
    send_ws(&mut s.sink, message).await?;
    let (_, name) = engine::parse_operation(
        action["query"].as_str().unwrap_or_default(),
        action["operation_name"].as_str(),
    )?;
    s.active.insert(sub, name);
    Ok(ClientSendOutcome::Sent { bytes_sent: n })
}

/// Handle one socket message. `Ok(false)` means the socket is gone; every subscription still
/// active hears about it as an error event.
async fn socket_message(
    ctx: &ConnectContext,
    socket: &mut Option<Socket>,
    message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    events: &mpsc::Sender<Event>,
) -> Result<bool> {
    let s = socket.as_mut().expect("only polled while open");
    let closed = |why: String| async move {
        Log::new(Some(&ctx.status_tx)).warn(format!("GraphQL WebSocket: {why}"));
        why
    };
    let text = match message {
        Some(Ok(Message::Text(t))) => t,
        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => return Ok(true),
        Some(Ok(Message::Close(frame))) => {
            let why = closed(match frame {
                Some(f) => format!("closed by the server ({} {})", u16::from(f.code), f.reason),
                None => "closed by the server".into(),
            })
            .await;
            return end_all(s, &why, events).await.map(|()| false);
        }
        Some(Ok(Message::Binary(_))) => {
            let why = closed("binary frame from the server".into()).await;
            return end_all(s, &why, events).await.map(|()| false);
        }
        None | Some(Err(_)) => {
            let why = closed("connection lost".into()).await;
            return end_all(s, &why, events).await.map(|()| false);
        }
    };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v @ Value::Object(_)) if engine::budget_ok(&v) => v,
        _ => {
            let why = closed("unparsable message from the server".into()).await;
            let _ = s.sink.close().await;
            return end_all(s, &why, events).await.map(|()| false);
        }
    };
    let sub = v["id"].as_str().unwrap_or_default().to_owned();
    let event = match v["type"].as_str() {
        Some("ping") => {
            send_ws(&mut s.sink, json!({"type": "pong"})).await?;
            return Ok(true);
        }
        Some("pong") => return Ok(true),
        Some(kind @ ("next" | "error" | "complete")) => {
            let Some(name) = s.active.get(&sub).cloned() else {
                // A message for a subscription we ended: allowed to race, ignored.
                return Ok(true);
            };
            match kind {
                "next" => {
                    if let Err(e) = engine::check_response(&v["payload"]) {
                        Log::new(Some(&ctx.status_tx))
                            .warn(format!("GraphQL subscription {sub}: bad next payload: {e}"));
                        return Ok(true);
                    }
                    let mut data = json!({"subscription_id": sub, "operation_name": name});
                    for k in ["data", "errors"] {
                        if let Some(x) = v["payload"].get(k) {
                            data[k] = x.clone();
                        }
                    }
                    Event::new(&actions::SUBSCRIPTION_EVENT, data)
                }
                "error" => {
                    s.active.remove(&sub);
                    let errors = v["payload"]
                        .as_array()
                        .filter(|e| !e.is_empty())
                        .cloned()
                        .unwrap_or_else(|| vec![json!({"message": "error without details"})]);
                    Event::new(
                        &actions::SUBSCRIPTION_ERROR_EVENT,
                        json!({"subscription_id": sub, "operation_name": name, "errors": errors}),
                    )
                }
                _ => {
                    s.active.remove(&sub);
                    Event::new(
                        &actions::SUBSCRIPTION_COMPLETE_EVENT,
                        json!({"subscription_id": sub, "operation_name": name}),
                    )
                }
            }
        }
        _ => {
            let why = closed(format!("unexpected message type {:?}", v["type"])).await;
            let _ = s.sink.close().await;
            return end_all(s, &why, events).await.map(|()| false);
        }
    };
    events
        .send(event)
        .await
        .context("GraphQL event consumer stopped")?;
    Ok(true)
}

async fn end_all(s: &mut Socket, why: &str, events: &mpsc::Sender<Event>) -> Result<()> {
    for (sub, name) in s.active.drain() {
        events
            .send(Event::new(
                &actions::SUBSCRIPTION_ERROR_EVENT,
                json!({"subscription_id": sub, "operation_name": name, "errors": [{"message": format!("WebSocket {why}")}]}),
            ))
            .await
            .context("GraphQL event consumer stopped")?;
    }
    Ok(())
}

async fn query(
    ctx: &ConnectContext,
    fetch: &FetchClient,
    url: &str,
    action: &Value,
    command: Option<ClientCommand>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let (op_type, op_name) = match engine::parse_operation(
        action["query"].as_str().unwrap_or_default(),
        action["operation_name"].as_str(),
    ) {
        Ok(v) => v,
        Err(e) => {
            reply(command, rejected(e));
            return Ok(());
        }
    };
    let use_get = action["use_get"].as_bool().unwrap_or(false);
    if use_get && op_type == OperationType::Mutation {
        reply(command, rejected("mutations cannot be sent with GET"));
        return Ok(());
    }
    let query = action["query"].as_str().unwrap_or_default();
    let variables = action.get("variables").cloned().unwrap_or(json!({}));
    let outcome = exchange(
        fetch,
        url,
        query,
        action["operation_name"].as_str(),
        &variables,
        use_get,
    )
    .await;
    if command.is_some() {
        ctx.state
            .record_access_log(
                AccessLogOwner::Client(ctx.client_id.as_u32()),
                "GraphQL",
                None,
                "injected_action",
                json!({"operation_type": engine::operation_type_name(op_type)}),
                vec![json!({"ok": outcome.is_ok()})],
            )
            .await;
    }
    match outcome {
        Ok(mut data) => {
            data["operation_type"] = json!(engine::operation_type_name(op_type));
            data["operation_name"] = json!(op_name);
            reply(
                command,
                Ok(ClientSendOutcome::Sent {
                    bytes_sent: query.len(),
                }),
            );
            events
                .send(Event::new(&actions::RESPONSE_EVENT, data))
                .await
                .context("GraphQL event consumer stopped")?;
        }
        Err(e) => {
            Log::new(Some(&ctx.status_tx)).warn(format!("GraphQL request failed: {e}"));
            reply(command, Err(e));
        }
    }
    Ok(())
}

/// One GraphQL-over-HTTP request. Transport failures are `Err`; any HTTP answer is `Ok` with
/// either the checked GraphQL body or an `error` saying why it was not one.
async fn exchange(
    fetch: &FetchClient,
    url: &str,
    query: &str,
    operation_name: Option<&str>,
    variables: &Value,
    use_get: bool,
) -> Result<Value> {
    let request = if use_get {
        let mut params = vec![("query", query.to_owned())];
        if let Some(n) = operation_name {
            params.push(("operationName", n.to_owned()));
        }
        if variables.as_object().is_some_and(|v| !v.is_empty()) {
            params.push(("variables", variables.to_string()));
        }
        fetch.get(url).query(&params)
    } else {
        let mut body = json!({"query": query});
        if let Some(n) = operation_name {
            body["operationName"] = json!(n);
        }
        if variables.as_object().is_some_and(|v| !v.is_empty()) {
            body["variables"] = variables.clone();
        }
        fetch
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
    };
    let response = request.header("Accept", ACCEPT).send().await?;
    let status = response.status().as_u16();
    let media = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let bytes = response.bytes().await?;
    let mut out = json!({"status": status});
    if media != GRAPHQL_RESPONSE && media != "application/json" {
        out["error"] = json!(format!(
            "HTTP {status} answered with {} rather than a GraphQL response",
            if media.is_empty() {
                "no content type"
            } else {
                &media
            }
        ));
        return Ok(out);
    }
    out["media_type"] = json!(media);
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(b) if engine::budget_ok(&b) => b,
        _ => {
            out["error"] = json!(format!(
                "HTTP {status}: body is not a bounded JSON document"
            ));
            return Ok(out);
        }
    };
    if let Err(e) = engine::check_response(&body) {
        out["error"] = json!(format!("HTTP {status}: not a GraphQL response: {e}"));
        return Ok(out);
    }
    // graphql-response+json: a body without data must not come with 2xx.
    if media == GRAPHQL_RESPONSE && body.get("data").is_none() && (200..300).contains(&status) {
        out["error"] = json!("a response without data was sent with a 2xx status");
        return Ok(out);
    }
    for k in ["data", "errors", "extensions"] {
        if let Some(v) = body.get(k) {
            out[k] = v.clone();
        }
    }
    Ok(out)
}
