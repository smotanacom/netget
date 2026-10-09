//! ICAP client: one request at a time on a persistent connection, preview included.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::icap::wire::{self, HttpHead};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::IcapClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let stream = tokio::time::timeout(
        wire::IO_TIMEOUT,
        tokio::net::TcpStream::connect(&ctx.remote_addr),
    )
    .await
    .context("ICAP connect deadline")??;
    let local = stream.local_addr()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"remote_addr": ctx.remote_addr}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = IcapClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("ICAP client handler: {e}"))
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
        let result = session(&session_ctx, stream, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("ICAP client ended: {e}"));
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

fn reject(command: Option<ClientCommand>, error: String) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, Ok(ClientSendOutcome::Rejected { error }));
    }
}

async fn session(
    ctx: &ConnectContext,
    stream: tokio::net::TcpStream,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let (read, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let data = match IcapClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Custom { data, .. }) => data,
            Ok(ClientActionResult::Disconnect) => {
                let _ = writer.shutdown().await;
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => {
                reject(command, "unsupported action".into());
                continue;
            }
            Err(e) => {
                reject(command, e.to_string());
                continue;
            }
        };
        let method = data["method"].as_str().unwrap_or("OPTIONS").to_owned();
        let service = data["service"].as_str().unwrap_or("").to_owned();
        if command.is_some() {
            // Method and service only: bodies may carry user content.
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "ICAP",
                    None,
                    "injected_action",
                    json!({"method": method, "service": service}),
                    vec![],
                )
                .await;
        }
        let outcome = exchange(ctx, &data, &mut reader, &mut writer).await;
        match outcome {
            Ok((sent, event)) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Sent { bytes_sent: sent }),
                    );
                }
                events
                    .try_send(Event::new(&actions::RESPONSE_EVENT, event))
                    .context("ICAP event queue full; consumer stalled")?;
            }
            Err(e) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Err(anyhow::anyhow!(e.to_string())));
                }
                return Err(e);
            }
        }
    }
}

async fn exchange<R, W>(
    ctx: &ConnectContext,
    data: &Value,
    reader: &mut R,
    writer: &mut W,
) -> Result<(usize, Value)>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let method = data["method"].as_str().unwrap_or("OPTIONS");
    let service = data["service"].as_str().unwrap_or("");
    let req = if data["http_request"].is_null() {
        None
    } else {
        Some(wire::request_head(&data["http_request"])?)
    };
    let res = if data["http_response"].is_null() {
        None
    } else {
        Some(wire::response_head(&data["http_response"])?)
    };
    let body = data["body_text"].as_str().map(|b| b.as_bytes().to_vec());
    let mut headers = vec![("Host".to_owned(), ctx.remote_addr.clone())];
    if data["allow_204"].as_bool().unwrap_or(true) && method != "OPTIONS" {
        headers.push(("Allow".into(), "204".into()));
    }
    let start = format!("{method} icap://{}/{service} ICAP/1.0", ctx.remote_addr);
    let body_kind = if method == "REQMOD" {
        "req-body"
    } else {
        "res-body"
    };
    let preview = data["preview"].as_u64().map(|p| p as usize);
    let (first, rest) = match (&body, preview) {
        (Some(b), Some(p)) => {
            headers.push(("Preview".into(), p.min(b.len()).to_string()));
            let (head, tail) = b.split_at(p.min(b.len()));
            (Some(head.to_vec()), Some(tail.to_vec()))
        }
        (b, _) => (b.clone(), None),
    };
    let mut bytes = wire::message(
        &start,
        headers,
        req.as_ref(),
        if method == "RESPMOD" {
            res.as_ref()
        } else {
            None
        },
        first.as_deref(),
        body_kind,
    )?;
    // A complete preview ends with `0; ieof` so the server knows not to ask for more.
    if let Some(tail) = &rest {
        if tail.is_empty() {
            let len = bytes.len();
            ensure!(
                bytes.ends_with(b"0\r\n\r\n"),
                "internal: preview terminator"
            );
            bytes.truncate(len - 5);
            bytes.extend_from_slice(b"0; ieof\r\n\r\n");
        }
    }
    wire::write_all(writer, &bytes).await?;
    let mut sent = bytes.len();
    let mut head = wire::read_head(reader, wire::MAX_HEAD_BYTES)
        .await?
        .context("ICAP server closed before answering")?;
    let mut continued = false;
    if head.starts_with("ICAP/1.0 100") {
        let tail = rest
            .filter(|t| !t.is_empty())
            .context("server sent 100 Continue with no preview pending")?;
        let more = wire::chunked(&tail);
        wire::write_all(writer, &more).await?;
        sent += more.len();
        continued = true;
        head = wire::read_head(reader, wire::MAX_HEAD_BYTES)
            .await?
            .context("ICAP server closed before answering")?;
    }
    let parsed = wire::parse_head(&head)?;
    let [version, status, reason] = &parsed.start;
    ensure!(
        version == "ICAP/1.0",
        "response version '{version}' is not ICAP/1.0"
    );
    let status: u16 = status.parse().context("ICAP status is not a number")?;
    let mut event = json!({
        "method": method, "service": service, "status": status, "reason": reason, "continued": continued,
        "icap_headers": parsed.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
    });
    if let Some(enc) = wire::header(&parsed.headers, "Encapsulated") {
        let entities = wire::parse_encapsulated(enc)?;
        let mut adapted_req: Option<HttpHead> = None;
        for (i, (entity, offset)) in entities.iter().enumerate() {
            let next = entities.get(i + 1).map(|(_, o)| *o);
            match entity.as_str() {
                "req-hdr" => {
                    adapted_req = Some(
                        wire::read_exact_head(
                            reader,
                            next.context("req-hdr must be followed")? - offset,
                        )
                        .await?,
                    )
                }
                "res-hdr" => {
                    event["http_response"] = wire::head_json(
                        &wire::read_exact_head(
                            reader,
                            next.context("res-hdr must be followed")? - offset,
                        )
                        .await?,
                        false,
                    )
                }
                "req-body" | "res-body" | "opt-body" => {
                    let chunks = wire::read_chunks(reader, 0).await?;
                    for (k, v) in wire::body_json(&chunks.data).as_object().expect("object") {
                        event[k] = v.clone();
                    }
                }
                _ => {}
            }
        }
        if let Some(h) = adapted_req {
            event["http_request"] = wire::head_json(&h, true);
        }
    } else if status != 100 && (200..300).contains(&status) && status != 204 {
        bail!("ICAP {status} response without an Encapsulated header");
    }
    Ok((sent, event))
}
