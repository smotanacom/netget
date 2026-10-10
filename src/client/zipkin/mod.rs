//! Zipkin reporter: reports spans to a collector and queries its read API, one HTTP/1.1
//! connection per request.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::zipkin::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ZipkinClientProtocol;
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    client::conn::http1,
    header::{CONNECTION, CONTENT_ENCODING, CONTENT_TYPE, HOST},
    Request, Uri,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// One request, connect to last byte.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;

#[derive(Clone)]
struct Origin {
    authority: String,
    connect_addr: String,
}

fn origin(value: &str) -> Result<Origin> {
    let value = if value.starts_with("http://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let uri: Uri = value.parse().context("invalid Zipkin collector address")?;
    ensure!(
        matches!(uri.path(), "" | "/") && uri.query().is_none(),
        "give the collector as host:port or http://host:port, without a path"
    );
    let authority = uri.authority().context("collector host required")?;
    Ok(Origin {
        authority: authority.as_str().to_string(),
        connect_addr: format!(
            "{}:{}",
            authority.host(),
            authority.port_u16().unwrap_or(9411)
        ),
    })
}

struct Answer {
    status: u16,
    content_type: String,
    body: Bytes,
}

async fn exchange(
    origin: &Origin,
    method: &str,
    path: &str,
    body: Option<(Vec<u8>, bool)>,
) -> Result<Answer> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let socket = tokio::net::TcpStream::connect(&origin.connect_addr).await?;
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(64)
            .max_buf_size(32 * 1024)
            .handshake(TokioIo::new(socket))
            .await?;
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, &origin.authority)
            .header(CONNECTION, "close");
        let payload = match body {
            Some((bytes, gzip)) => {
                request = request.header(CONTENT_TYPE, "application/json");
                if gzip {
                    request = request.header(CONTENT_ENCODING, "gzip");
                    wire::gzip(&bytes)?
                } else {
                    bytes
                }
            }
            None => Vec::new(),
        };
        let request = request.body(Full::new(Bytes::from(payload)))?;
        let exchange = async {
            let response = sender.send_request(request).await?;
            let (parts, body) = response.into_parts();
            let encoding = parts
                .headers
                .get(CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("identity")
                .to_ascii_lowercase();
            let raw = Limited::new(body, wire::MAX_BODY_BYTES)
                .collect()
                .await
                .map_err(|_| anyhow::anyhow!("response body exceeds 1 MiB or is incomplete"))?
                .to_bytes();
            let body = Bytes::from(wire::decode_body(&raw, &encoding)?);
            Ok::<_, anyhow::Error>(Answer {
                status: parts.status.as_u16(),
                content_type: parts
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
                body,
            })
        };
        tokio::pin!(connection);
        tokio::pin!(exchange);
        tokio::select! {
            r = &mut exchange => r,
            r = &mut connection => {
                r.context("HTTP connection failed")?;
                exchange.await
            }
        }
    })
    .await
    .context("Zipkin request deadline exceeded")?
}

fn message(answer: &Answer) -> String {
    let text = String::from_utf8_lossy(&answer.body);
    crate::utils::truncate::truncate_for_log(text.trim(), 1024).to_string()
}

/// Perform one validated action and describe the outcome as the event it raises.
async fn perform(origin: &Origin, action: &Value) -> Result<Event> {
    if action["type"] == "zipkin_report" {
        let spans = wire::spans(&action["spans"])?;
        let body = serde_json::to_vec(&spans)?;
        let gzip = action["gzip"].as_bool().unwrap_or(false);
        let answer = exchange(origin, "POST", "/api/v2/spans", Some((body, gzip))).await?;
        let ok = (200..300).contains(&answer.status);
        return Ok(Event::new(
            &actions::REPORT_EVENT,
            json!({"status": answer.status, "accepted": ok, "span_count": spans.len(),
                   "message": if ok { String::new() } else { message(&answer) }}),
        ));
    }
    let (path, shape) = actions::query_path(action)?;
    let answer = exchange(origin, "GET", &path, None).await?;
    let endpoint = action["endpoint"].as_str().unwrap_or_default();
    let (result, msg) = if answer.status == 200 {
        let is_json = answer
            .content_type
            .split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
        match serde_json::from_slice::<Value>(&answer.body)
            .ok()
            .filter(|_| is_json)
            .map(|v| wire::result(shape, &v))
        {
            Some(Ok(v)) => (v, String::new()),
            Some(Err(e)) => (Value::Null, format!("invalid {endpoint} answer: {e:#}")),
            None => (Value::Null, "the answer was not JSON".to_string()),
        }
    } else {
        (Value::Null, message(&answer))
    };
    Ok(Event::new(
        &actions::QUERY_EVENT,
        json!({"endpoint": endpoint, "status": answer.status, "result": result, "message": msg}),
    ))
}

pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let origin = origin(&ctx.remote_addr)?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(32);
    event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({"remote_addr": format!("http://{}", origin.authority)}),
        ),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
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
                &ZipkinClientProtocol,
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
                        if internal_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("Zipkin client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &origin, external, internal_rx, event_tx).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Zipkin client ended: {e}"));
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
    Ok("0.0.0.0:0".parse()?)
}

async fn session(
    ctx: &ConnectContext,
    origin: &Origin,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) -> Result<()> {
    let log = Log::new(Some(&ctx.status_tx));
    loop {
        let (action, depth, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some((a, depth)) => (a, depth, None),
                None => return Ok(()),
            },
        };
        let reply = |injected: &mut Option<ClientCommand>, outcome: ClientSendOutcome| {
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(command, Ok(outcome));
            }
        };
        match ZipkinClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("Zipkin client action refused: {e:#}"));
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                continue;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "Zipkin client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Zipkin",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        match perform(origin, &action).await {
            Ok(event) => {
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "Zipkin",
                        None,
                        event.id(),
                        event.data.clone(),
                        vec![],
                    )
                    .await;
                reply(
                    &mut injected,
                    ClientSendOutcome::Executed {
                        detail: event.data.to_string(),
                    },
                );
                events
                    .try_send((event, depth))
                    .context("Zipkin event queue full; consumer stalled")?;
            }
            Err(e) => {
                log.warn(format!("Zipkin request failed: {e:#}"));
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: format!("{e:#}"),
                    },
                );
            }
        }
    }
}
