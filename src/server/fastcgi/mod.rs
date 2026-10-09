//! FastCGI 1.0 application in the Responder role. Rust owns records, request ids, streams,
//! management records and bounds; the handler owns every response.
pub mod actions;
pub mod record;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, Context, Result};
use record::Record;
use serde_json::{json, Map, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

struct Shared {
    ctx: SpawnContext,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let idle = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=3600).contains(&idle),
        "idle_timeout_secs must be 1..=3600"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("FastCGI responder on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        idle: Duration::from_secs(idle),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            // A web server reading a 503 on a FastCGI socket gets nothing it understands, so a
            // refused connection is simply closed.
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                b"",
                "FastCGI",
                Some(&shared.ctx.status_tx),
            )
            .await
            {
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
                    if let Err(e) = connection(&child, id, stream).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("FastCGI connection {id}: {e}"));
                    }
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

fn outcome(ctx: &SpawnContext, id: ConnectionId, request: u16, decision: &str) {
    let summary = format!("FastCGI connection {id} request={request} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

#[derive(Default)]
struct Pending {
    id: u16,
    keep_conn: bool,
    params: Vec<u8>,
    params_done: bool,
    stdin: Vec<u8>,
}

async fn write<W: AsyncWrite + Unpin>(
    shared: &Shared,
    id: ConnectionId,
    w: &mut W,
    bytes: &[u8],
) -> Result<()> {
    w.write_all(bytes).await?;
    w.flush().await?;
    shared
        .ctx
        .state
        .update_connection_stats(
            shared.ctx.server_id,
            id,
            None,
            Some(bytes.len() as u64),
            None,
            Some(1),
        )
        .await;
    Ok(())
}

fn end(request: u16, app_status: u32, protocol_status: u8) -> Vec<u8> {
    record::encode(
        record::END_REQUEST,
        request,
        &record::end_request(app_status, protocol_status),
    )
}

/// The records answering one request: STDOUT, optional STDERR, END_REQUEST.
fn reply(request: u16, stdout: &[u8], stderr: Option<&[u8]>, app_status: u32) -> Vec<u8> {
    let mut out = record::encode_stream(record::STDOUT, request, stdout);
    if let Some(e) = stderr.filter(|e| !e.is_empty()) {
        out.extend(record::encode_stream(record::STDERR, request, e));
    }
    out.extend(end(request, app_status, record::REQUEST_COMPLETE));
    out
}

fn error_reply(request: u16, status: u16, text: &str) -> Vec<u8> {
    let mut headers = vec![(
        "Content-Type".to_owned(),
        "text/plain; charset=utf-8".to_owned(),
    )];
    if status == 503 {
        headers.push(("Retry-After".to_owned(), "5".to_owned()));
    }
    reply(
        request,
        &record::build_cgi_response(status, &headers, text.as_bytes()),
        None,
        1,
    )
}

async fn connection(
    shared: &Shared,
    id: ConnectionId,
    stream: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut pending: Option<Pending> = None;
    loop {
        let rec: Record =
            match tokio::time::timeout(shared.idle, record::read_record(&mut reader)).await {
                Err(_) => bail!("idle for {}s", shared.idle.as_secs()),
                Ok(r) => match r? {
                    Some(r) => r,
                    None => return Ok(()),
                },
            };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some((record::HEADER_LEN + rec.content.len()) as u64),
                None,
                Some(1),
                None,
            )
            .await;
        if rec.request_id == 0 {
            match rec.kind {
                record::GET_VALUES => {
                    let asked = record::decode_pairs(&rec.content)?;
                    let max = DEFAULT_MAX_CONNECTIONS.to_string();
                    let known: Vec<(&str, &str)> = asked
                        .iter()
                        .filter_map(|(k, _)| match k.as_str() {
                            "FCGI_MAX_CONNS" => Some(("FCGI_MAX_CONNS", max.as_str())),
                            "FCGI_MAX_REQS" => Some(("FCGI_MAX_REQS", max.as_str())),
                            "FCGI_MPXS_CONNS" => Some(("FCGI_MPXS_CONNS", "0")),
                            _ => None,
                        })
                        .collect();
                    let body = record::encode_pairs(known);
                    write(
                        shared,
                        id,
                        &mut writer,
                        &record::encode(record::GET_VALUES_RESULT, 0, &body),
                    )
                    .await?;
                }
                kind => {
                    write(
                        shared,
                        id,
                        &mut writer,
                        &record::encode(record::UNKNOWN_TYPE, 0, &record::unknown_type(kind)),
                    )
                    .await?;
                }
            }
            continue;
        }
        match rec.kind {
            record::BEGIN_REQUEST => {
                anyhow::ensure!(rec.content.len() >= 8, "BEGIN_REQUEST body is 8 bytes");
                let role = u16::from_be_bytes([rec.content[0], rec.content[1]]);
                let keep_conn = rec.content[2] & record::KEEP_CONN != 0;
                if pending.is_some() {
                    write(
                        shared,
                        id,
                        &mut writer,
                        &end(rec.request_id, 0, record::CANT_MPX_CONN),
                    )
                    .await?;
                    continue;
                }
                if role != record::ROLE_RESPONDER {
                    outcome(ctx, id, rec.request_id, "protocol_refusal");
                    write(
                        shared,
                        id,
                        &mut writer,
                        &end(rec.request_id, 0, record::UNKNOWN_ROLE),
                    )
                    .await?;
                    if !keep_conn {
                        return Ok(());
                    }
                    continue;
                }
                pending = Some(Pending {
                    id: rec.request_id,
                    keep_conn,
                    ..Default::default()
                });
            }
            record::ABORT_REQUEST => {
                if let Some(p) = pending.take_if(|p| p.id == rec.request_id) {
                    outcome(ctx, id, p.id, "peer_abort");
                    write(
                        shared,
                        id,
                        &mut writer,
                        &end(p.id, 0, record::REQUEST_COMPLETE),
                    )
                    .await?;
                    if !p.keep_conn {
                        return Ok(());
                    }
                }
            }
            record::PARAMS => {
                let Some(p) = pending.as_mut().filter(|p| p.id == rec.request_id) else {
                    continue;
                };
                anyhow::ensure!(!p.params_done, "PARAMS after the end of the PARAMS stream");
                if rec.content.is_empty() {
                    p.params_done = true;
                } else if p.params.len() + rec.content.len() > record::MAX_PARAMS_BYTES {
                    let p = pending.take().expect("checked");
                    outcome(ctx, id, p.id, "protocol_refusal");
                    write(
                        shared,
                        id,
                        &mut writer,
                        &error_reply(p.id, 431, "request parameters too large"),
                    )
                    .await?;
                    return Ok(());
                } else {
                    p.params.extend_from_slice(&rec.content);
                }
            }
            record::STDIN => {
                let Some(p) = pending.as_mut().filter(|p| p.id == rec.request_id) else {
                    continue;
                };
                anyhow::ensure!(p.params_done, "STDIN before the end of the PARAMS stream");
                if !rec.content.is_empty() {
                    if p.stdin.len() + rec.content.len() > record::MAX_BODY_BYTES {
                        let p = pending.take().expect("checked");
                        outcome(ctx, id, p.id, "protocol_refusal");
                        write(
                            shared,
                            id,
                            &mut writer,
                            &error_reply(p.id, 413, "request body too large"),
                        )
                        .await?;
                        return Ok(());
                    }
                    p.stdin.extend_from_slice(&rec.content);
                    continue;
                }
                let p = pending.take().expect("checked");
                let answer = respond(shared, id, &p).await;
                write(shared, id, &mut writer, &answer).await?;
                if !p.keep_conn {
                    return Ok(());
                }
            }
            record::DATA => {}
            kind @ (record::STDOUT
            | record::STDERR
            | record::END_REQUEST
            | record::GET_VALUES_RESULT
            | record::UNKNOWN_TYPE) => bail!("record type {kind} only flows from an application"),
            kind => {
                write(
                    shared,
                    id,
                    &mut writer,
                    &record::encode(record::UNKNOWN_TYPE, 0, &record::unknown_type(kind)),
                )
                .await?;
            }
        }
    }
}

/// Ask the handler about one complete request and build the records that answer it. Every
/// failure is a 5xx response with a category message, never one the handler did not give.
async fn respond(shared: &Shared, id: ConnectionId, p: &Pending) -> Vec<u8> {
    let ctx = &shared.ctx;
    let pairs = match record::decode_pairs(&p.params) {
        Ok(pairs) => pairs,
        Err(_) => {
            outcome(ctx, id, p.id, "protocol_refusal");
            return error_reply(p.id, 400, "malformed request parameters");
        }
    };
    let mut params = Map::new();
    let mut headers = Map::new();
    for (k, v) in &pairs {
        if let Some(h) = k.strip_prefix("HTTP_") {
            headers.insert(h.to_ascii_lowercase().replace('_', "-"), json!(v));
        }
        params.insert(k.clone(), json!(v));
    }
    for (param, header) in [
        ("CONTENT_TYPE", "content-type"),
        ("CONTENT_LENGTH", "content-length"),
    ] {
        if let Some(v) = params.get(param).filter(|v| v.as_str() != Some("")) {
            headers.insert(header.into(), v.clone());
        }
    }
    let get = |k: &str| params.get(k).cloned().unwrap_or(Value::Null);
    let (body, encoding) = record::body_text(&p.stdin);
    let event = Event::new(
        &actions::REQUEST_EVENT,
        json!({
            "request_id": p.id,
            "method": params.get("REQUEST_METHOD").cloned().unwrap_or(json!("GET")),
            "request_uri": get("REQUEST_URI"),
            "script_name": get("SCRIPT_NAME"),
            "path_info": get("PATH_INFO"),
            "query_string": get("QUERY_STRING"),
            "content_type": get("CONTENT_TYPE"),
            "headers": headers,
            "params": params,
            "body": body,
            "body_encoding": encoding,
            "keep_conn": p.keep_conn,
        }),
    );
    let failed = |decision: &str, e: anyhow::Error| {
        outcome(ctx, id, p.id, decision);
        let failure = crate::utils::wire_failure::WireFailure::classify(&e);
        error_reply(
            p.id,
            if failure.is_overloaded() { 503 } else { 500 },
            failure.text(),
        )
    };
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::FastcgiProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return failed("fail_closed_llm_error", e),
    };
    if !result.failures.is_empty() {
        return failed(
            "fail_closed_invalid_reply",
            anyhow::anyhow!("invalid handler answer"),
        );
    }
    let answers: Vec<Value> = result
        .protocol_results
        .into_iter()
        .flat_map(|r| match r {
            ActionResult::Multiple(items) => items,
            other => vec![other],
        })
        .filter_map(|r| match r {
            ActionResult::Custom { name, data } if name == "fastcgi_respond" => Some(data),
            _ => None,
        })
        .collect();
    let answer = match answers.as_slice() {
        [one] => one,
        [] => return failed("model_silent", anyhow::anyhow!("no handler answer")),
        _ => {
            return failed(
                "fail_closed_invalid_reply",
                anyhow::anyhow!("more than one answer"),
            )
        }
    };
    let built = (|| -> Result<Vec<u8>> {
        actions::check_respond(answer)?;
        let status = answer["status"]
            .as_u64()
            .map(|s| s as u16)
            .context("fastcgi_respond requires an explicit status")?;
        let headers = record::check_headers(answer.get("headers"))?;
        let body = record::decode_body(answer)?;
        Ok(reply(
            p.id,
            &record::build_cgi_response(status, &headers, &body),
            answer["stderr"].as_str().map(str::as_bytes),
            0,
        ))
    })();
    match built {
        Ok(records) => {
            outcome(ctx, id, p.id, "model_answer");
            records
        }
        Err(e) => failed("fail_closed_invalid_reply", e),
    }
}
