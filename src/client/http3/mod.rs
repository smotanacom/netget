//! Authenticated RFC 9114 client with one owned control driver and bounded request futures.
//! The registered owner polls all query and handler futures; no detached tasks
//! survive client removal and a parked handler never prevents command injection.
pub mod actions;
use crate::client::{command_support, llm_budget::call_llm_for_client};
use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::http3::wire::*;
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
use crate::utils::quic::*;
pub use actions::Http3ClientProtocol;
use actions::{
    HTTP3_CLIENT_CONNECTED_EVENT, HTTP3_CLIENT_ERROR_EVENT, HTTP3_CLIENT_RESPONSE_RECEIVED_EVENT,
};
use anyhow::{ensure, Context, Result};
use bytes::{Buf, Bytes};
type Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};

const MAX_FOLLOWUP_DEPTH: u8 = 4;
type HandlerFuture = BoxFuture<'static, (u8, Result<Vec<Value>>)>;
type QueryFuture = BoxFuture<'static, (u8, Event)>;

pub struct Http3Client;
impl Http3Client {
    pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
        let params = ctx.startup_params.as_ref();
        let exchange =
            Duration::from_secs(bounded_parameter(params, "exchange_timeout_secs", 30, 300)?);
        let (endpoint, connection) =
            crate::utils::quic::connect(&ctx.remote_addr, params, b"h3", 4).await?;
        let endpoint = EndpointOwner(endpoint);
        let connection = ConnectionOwner(connection);
        let (driver, sender) = h3::client::builder()
            .send_grease(false)
            .max_field_section_size(MAX_HEADERS as u64)
            .build(h3_quinn::Connection::new(connection.clone()))
            .await?;
        let local_addr = endpoint.local_addr()?;
        let now = crate::utils::clock::Instant::now();
        ctx.state
            .with_client_mut(ctx.client_id, |client| {
                client.connection = Some(crate::state::ClientConnectionState {
                    id: ctx.client_id,
                    remote_addr: ctx.remote_addr.clone(),
                    connected_addr: Some(connection.remote_address()),
                    local_addr: Some(local_addr),
                    bytes_sent: 0,
                    bytes_received: 0,
                    packets_sent: 0,
                    packets_received: 0,
                    last_activity: now,
                    status: ClientStatus::Connected,
                    status_changed_at: now,
                    protocol_info: crate::state::server::ProtocolConnectionInfo::new(
                        json!({"alpn":"h3"}),
                    ),
                });
            })
            .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        let command_rx = command_support::register_command_channel(&ctx.state, ctx.client_id).await;
        let state = ctx.state.clone();
        let id = ctx.client_id;
        let task = tokio::spawn(Self::run(
            ctx, endpoint, connection, driver, sender, command_rx, exchange,
        ));
        state.register_client_task(id, task).await;
        Ok(local_addr)
    }

    async fn run(
        ctx: ConnectContext,
        _endpoint: EndpointOwner,
        connection: ConnectionOwner,
        mut driver: h3::client::Connection<h3_quinn::Connection, Bytes>,
        sender: Sender,
        mut commands: tokio::sync::mpsc::Receiver<ClientCommand>,
        exchange: Duration,
    ) {
        let mut handlers: FuturesUnordered<HandlerFuture> = FuturesUnordered::new();
        let mut queries: FuturesUnordered<QueryFuture> = FuturesUnordered::new();
        handlers.push(handle_event(
            ctx.clone(),
            Event::new(
                &HTTP3_CLIENT_CONNECTED_EVENT,
                json!({"base_url":format!("https://{}",ctx.remote_addr),"remote_addr":ctx.remote_addr}),
            ),
            0,
        ));
        let mut disconnect = false;
        while !disconnect {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    disconnect = enqueue(&ctx,&sender,exchange,&mut queries,handlers.len(),command.action.clone(),Some(command),0).await;
                }
                Some((depth,event)) = queries.next(), if !queries.is_empty() => {
                    // The final response still reaches the handler; only subsequent actions
                    // are stopped at the depth boundary. A returned disconnect is honoured.
                    handlers.push(handle_event(ctx.clone(),event,depth));
                }
                Some((depth,result)) = handlers.next(), if !handlers.is_empty() => {
                    match result {
                        Ok(actions) if actions.len() <= MAX_STREAMS => {
                            for action in actions {
                                if depth >= MAX_FOLLOWUP_DEPTH && action["type"] != "disconnect" {
                                    if action["type"] == "send_http3_request" { Log::new(Some(&ctx.status_tx)).warn("HTTP3 follow-up limit reached"); }
                                    continue;
                                }
                                if enqueue(&ctx,&sender,exchange,&mut queries,handlers.len(),action,None,depth).await { disconnect = true; break; }
                            }
                        }
                        Ok(_) => Log::new(Some(&ctx.status_tx)).warn("HTTP3 handler returned more than 32 actions"),
                        Err(e) => Log::new(Some(&ctx.status_tx)).warn(format!("HTTP3 event handler failed: {e}")),
                    }
                }
                _ = std::future::poll_fn(|cx| driver.poll_close(cx)) => break,
                _ = connection.closed() => break,
            }
        }
        drop(queries);
        drop(handlers);
        connection.close(0x100u32.into(), b"client disconnected");
        ctx.state
            .with_client_mut(ctx.client_id, |client| {
                if let Some(connection) = &mut client.connection {
                    connection.status = ClientStatus::Disconnected;
                    connection.status_changed_at = crate::utils::clock::Instant::now();
                }
            })
            .await;
        ctx.state.remove_client_handle(ctx.client_id).await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Disconnected)
            .await;
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
}

fn handle_event(ctx: ConnectContext, event: Event, depth: u8) -> HandlerFuture {
    async move {
        let result = async {
            let instruction = ctx
                .state
                .get_instruction_for_client(ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = ctx
                .state
                .get_memory_for_client(ctx.client_id)
                .await
                .unwrap_or_default();
            let result = call_llm_for_client(
                &ctx.llm_client,
                &ctx.state,
                ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &Http3ClientProtocol::new(),
                &ctx.status_tx,
            )
            .await?;
            if let Some(memory) = result.memory_updates {
                ctx.state.set_memory_for_client(ctx.client_id, memory).await;
            }
            Ok(result.actions)
        }
        .await;
        (depth, result)
    }
    .boxed()
}

async fn enqueue(
    ctx: &ConnectContext,
    sender: &Sender,
    deadline: Duration,
    queries: &mut FuturesUnordered<QueryFuture>,
    handler_count: usize,
    action: Value,
    command: Option<ClientCommand>,
    depth: u8,
) -> bool {
    let outcome = match Http3ClientProtocol::new().execute_action(action.clone()) {
        Ok(ClientActionResult::Disconnect) => Ok(ClientSendOutcome::Disconnected),
        Ok(ClientActionResult::WaitForMore | ClientActionResult::NoAction) => {
            Ok(ClientSendOutcome::Executed {
                detail: "Waiting for more queries".into(),
            })
        }
        Ok(ClientActionResult::Custom { name, data }) if name == "http3_request" => {
            // Reserve a handler slot for every accepted exchange, so a parked
            // manual handler never causes a completed response event to be lost.
            if queries.len() + handler_count >= MAX_STREAMS {
                Err(anyhow::anyhow!(
                    "HTTP3 client busy: 32 active exchanges or handlers"
                ))
            } else {
                queries.push(query_future(
                    ctx.clone(),
                    sender.clone(),
                    deadline,
                    data,
                    action,
                    command,
                    depth + 1,
                ));
                return false;
            }
        }
        Ok(_) => Err(anyhow::anyhow!("Unsupported HTTP3 client action")),
        Err(e) => Ok(ClientSendOutcome::Rejected {
            error: e.to_string(),
        }),
    };
    let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
    record_action(ctx, &action, &outcome).await;
    if let Some(command) = command {
        command_support::reply(command, outcome);
    }
    disconnect
}

async fn record_action(ctx: &ConnectContext, action: &Value, outcome: &Result<ClientSendOutcome>) {
    let result = match outcome {
        Ok(v) => serde_json::to_value(v).unwrap_or(Value::Null),
        Err(e) => json!({"error":e.to_string()}),
    };
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.client_id.as_u32()),
            Http3ClientProtocol::new().protocol_name(),
            None,
            "injected_action",
            action.clone(),
            vec![result],
        )
        .await;
}

fn query_future(
    ctx: ConnectContext,
    sender: Sender,
    deadline: Duration,
    data: Value,
    action: Value,
    command: Option<ClientCommand>,
    depth: u8,
) -> QueryFuture {
    async move {
        let result = async {
            exchange(sender, &ctx.remote_addr, ctx.startup_params.as_ref(), data, deadline).await
        }
        .await;
        let (outcome, event) = match result {
            Ok(response) => {
                ctx.state.with_client_mut(ctx.client_id, |client| {
                    if let Some(connection) = &mut client.connection {
                        connection.bytes_sent += action["body"].as_str().unwrap_or("").len() as u64;
                        connection.bytes_received += response.body.len() as u64;
                        connection.packets_sent += 1;
                        connection.packets_received += 1;
                        connection.last_activity = crate::utils::clock::Instant::now();
                    }
                }).await;
                let event = Event::new(&HTTP3_CLIENT_RESPONSE_RECEIVED_EVENT, json!({
                    "status_code":response.status_code,"headers":response.headers,"body":response.body,
                    "trailers":response.trailers,"stream_id":response.stream_index,
                }));
                (Ok(ClientSendOutcome::Executed { detail: format!("http3_request {} {} -> {} ({} byte body)",
                    action["method"].as_str().unwrap_or("GET"),action["path"].as_str().unwrap_or("/"),response.status_code,response.body.len()) }),event)
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("HTTP3 stream failed: {e:#}"));
                let event = Event::new(&HTTP3_CLIENT_ERROR_EVENT, json!({"error":format!("{e:#}")}));
                (Err(e), event)
            }
        };
        record_action(&ctx, &action, &outcome).await;
        if let Some(command) = command {
            command_support::reply(command, outcome);
        }
        (depth, event)
    }
    .boxed()
}

/// One semantic response. HTTP bodies exposed to handlers are UTF-8 text.
#[derive(Debug)]
pub struct Http3Exchange {
    pub status_code: u16,
    pub headers: serde_json::Map<String, Value>,
    pub body: String,
    pub trailers: serde_json::Map<String, Value>,
    pub stream_index: u64,
}
/// Sends one request while the owner continues polling the control/QPACK driver.
pub async fn exchange(
    mut sender: Sender,
    remote: &str,
    params: Option<&crate::protocol::StartupParams>,
    data: Value,
    deadline: Duration,
) -> Result<Http3Exchange> {
    let method = data["method"].as_str().context("Missing method")?;
    let path = data["path"].as_str().context("Missing path")?;
    ensure!(
        path.starts_with('/') && !path.starts_with("//"),
        "HTTP3 path must be an origin-form path"
    );
    ensure!(path.len() <= MAX_HEADERS, "HTTP3 path too long");
    let body = data["body"].as_str().unwrap_or("");
    ensure!(body.len() <= MAX_BODY, "HTTP3 request body exceeds 8 MiB");
    let end = tokio::time::Instant::now() + deadline;
    let mut headers = params
        .map(|p| p.get_optional_object("default_headers"))
        .transpose()?
        .flatten()
        .cloned()
        .unwrap_or_default();
    headers = headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
    if let Some(h) = data["headers"].as_object() {
        for (key, value) in h {
            headers.insert(key.to_ascii_lowercase(), value.clone());
        }
    }
    if let Some(priority) = data["priority"].as_u64() {
        ensure!(priority <= 7, "priority must be 0..7");
        headers.insert("priority".into(), json!(format!("u={priority}")));
    }
    let mut request = http::Request::builder()
        .method(method)
        .uri(format!("https://{remote}{path}"))
        .body(())?;
    *request.headers_mut() = parse_request_headers(&Value::Object(headers))?;
    check_field_section(
        request.headers(),
        &[
            (":method", request.method().as_str()),
            (":scheme", "https"),
            (
                ":authority",
                request
                    .uri()
                    .authority()
                    .context("Missing authority")?
                    .as_str(),
            ),
            (":path", path),
        ],
    )?;
    validate_length(request.headers(), body.len())?;
    let stream = tokio::time::timeout_at(end, sender.send_request(request))
        .await
        .context("HTTP3 request deadline")??;
    let mut stream = ClientStreamGuard {
        stream,
        done: false,
    };
    let result = tokio::time::timeout_at(end, async {
        if !body.is_empty() {
            stream
                .stream
                .send_data(Bytes::copy_from_slice(body.as_bytes()))
                .await?;
        }
        let trailers = parse_headers(&data["trailers"])?;
        if !trailers.is_empty() {
            stream.stream.send_trailers(trailers).await?;
        }
        stream.stream.finish().await?;
        let mut informational = 0;
        let response = loop {
            let response = stream.stream.recv_response().await?;
            check_headers(response.headers())?;
            if response.status().is_informational() {
                ensure!(
                    response.status().as_u16() != 101,
                    "HTTP3 forbids switching protocols"
                );
                informational += 1;
                ensure!(
                    informational <= 16,
                    "HTTP3 too many informational responses"
                );
                continue;
            }
            break response;
        };
        let mut bytes = Vec::new();
        while let Some(mut chunk) = stream.stream.recv_data().await? {
            ensure!(
                chunk.remaining() <= MAX_BODY.saturating_sub(bytes.len()),
                "HTTP3 response body exceeds 8 MiB"
            );
            let n = chunk.remaining();
            bytes.extend_from_slice(&chunk.copy_to_bytes(n));
        }
        let trailers = stream.stream.recv_trailers().await?.unwrap_or_default();
        check_headers(&trailers)?;
        if method == "HEAD" || matches!(response.status().as_u16(), 204 | 205 | 304) {
            ensure!(bytes.is_empty(), "HTTP3 response must not contain a body");
        } else {
            validate_length(response.headers(), bytes.len())?;
        }
        Ok(Http3Exchange {
            status_code: response.status().as_u16(),
            headers: header_json(response.headers())?,
            body: String::from_utf8(bytes).context("HTTP3 body is not UTF-8 text")?,
            trailers: header_json(&trailers)?,
            stream_index: stream.stream.id().index(),
        })
    })
    .await
    .context("HTTP3 exchange deadline exceeded")?;
    if result.is_ok() {
        stream.done = true;
    }
    result
}
struct ClientStreamGuard {
    stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    done: bool,
}
impl Drop for ClientStreamGuard {
    fn drop(&mut self) {
        if !self.done {
            self.stream
                .stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
            self.stream
                .stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
        }
    }
}
