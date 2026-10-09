//! ICAP (RFC 3507) server. Rust owns framing, OPTIONS, preview continuation and response
//! assembly; the handler decides each REQMOD/RESPMOD.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use wire::HttpHead;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const DEFAULT_SERVICE: &str = "netget";
pub const DEFAULT_PREVIEW: u64 = 1024;
const ISTAG: &str = "\"NetGet-1\"";

struct Service {
    name: String,
    methods: Vec<String>,
}

struct Shared {
    ctx: SpawnContext,
    services: Vec<Service>,
    preview: u64,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let services = match p
        .map(|p| p.get_optional_array("services"))
        .transpose()?
        .flatten()
    {
        None => vec![Service {
            name: DEFAULT_SERVICE.into(),
            methods: vec!["REQMOD".into(), "RESPMOD".into()],
        }],
        Some(list) => {
            ensure!(
                !list.is_empty() && list.len() <= 32,
                "services must list 1..=32 services"
            );
            let mut out = Vec::new();
            for s in list {
                let name = s["name"].as_str().context("each service needs a name")?;
                ensure!(
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                    "service name '{name}' must be letters, digits, '-', '_' or '.'"
                );
                let methods: Vec<String> = s["methods"]
                    .as_array()
                    .context("each service needs methods")?
                    .iter()
                    .map(|m| m.as_str().unwrap_or("").to_owned())
                    .collect();
                ensure!(
                    !methods.is_empty() && methods.iter().all(|m| m == "REQMOD" || m == "RESPMOD"),
                    "methods must be REQMOD and/or RESPMOD"
                );
                out.push(Service {
                    name: name.into(),
                    methods,
                });
            }
            out
        }
    };
    let preview = p
        .map(|p| p.get_optional_u64("preview_bytes"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_PREVIEW);
    ensure!(preview <= 65536, "preview_bytes must be 0..=65536");
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    ensure!(
        (1..=86400).contains(&idle),
        "idle_timeout_secs must be 1..=86400"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ICAP listening on {local}; services: {}",
        services
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        services,
        preview,
        idle: Duration::from_secs(idle),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        let refusal = b"ICAP/1.0 503 Service Unavailable\r\nISTag: \"NetGet-1\"\r\nEncapsulated: null-body=0\r\n\r\n";
        loop {
            let (socket, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                refusal,
                "ICAP",
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
                    if let Err(e) = connection(&child, id, socket).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("ICAP connection {id} ended: {e}"));
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

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("ICAP connection {id} request={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

fn status_only(code: u16, reason: &str) -> Result<Vec<u8>> {
    wire::message(
        &format!("ICAP/1.0 {code} {reason}"),
        vec![("ISTag".into(), ISTAG.into())],
        None,
        None,
        None,
        "",
    )
}

async fn decide(shared: &Shared, id: ConnectionId, event: Event) -> Result<Value> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::IcapProtocol,
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
        bail!("ICAP handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == "icap_response" => {
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("ICAP handler supplied more than one response");
                }
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    found
        .context("ICAP handler did not answer")
        .inspect_err(|_| outcome(ctx, id, event.id(), "model_silent"))
}

struct Request {
    method: String,
    service: String,
    headers: Vec<(String, String)>,
    req: Option<HttpHead>,
    res: Option<HttpHead>,
    body: Option<Vec<u8>>,
}

async fn connection(
    shared: &Shared,
    id: ConnectionId,
    socket: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let (read, mut writer) = tokio::io::split(socket);
    let mut reader = BufReader::new(read);
    loop {
        let head = match tokio::time::timeout(
            shared.idle,
            wire::read_head(&mut reader, wire::MAX_HEAD_BYTES),
        )
        .await
        {
            Err(_) => return Ok(()),
            Ok(r) => r,
        };
        let Some(head) = (match head {
            Ok(h) => h,
            Err(e) => {
                let _ = wire::write_all(&mut writer, &status_only(400, "Bad Request")?).await;
                return Err(e);
            }
        }) else {
            return Ok(());
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(head.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let parsed = match wire::parse_head(&head) {
            Ok(p) => p,
            Err(e) => {
                let _ = wire::write_all(&mut writer, &status_only(400, "Bad Request")?).await;
                return Err(e);
            }
        };
        let close = wire::header(&parsed.headers, "Connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let reply = tokio::time::timeout(
            wire::IO_TIMEOUT * 4,
            handle(shared, id, parsed, &mut reader, &mut writer),
        )
        .await
        .context("ICAP request deadline")??;
        wire::write_all(&mut writer, &reply).await?;
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                None,
                Some(reply.len() as u64),
                None,
                Some(1),
            )
            .await;
        // After an error the request's body may still be unread; close rather than parse it
        // as the next request.
        let failed = reply.starts_with(b"ICAP/1.0 4") || reply.starts_with(b"ICAP/1.0 5");
        if close || failed {
            return Ok(());
        }
    }
}

async fn handle<R, W>(
    shared: &Shared,
    id: ConnectionId,
    head: HttpHead,
    reader: &mut R,
    writer: &mut W,
) -> Result<Vec<u8>>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let ctx = &shared.ctx;
    let [method, uri, version] = &head.start;
    if version != "ICAP/1.0" {
        return status_only(505, "ICAP Version Not Supported");
    }
    let Some(rest) = uri.strip_prefix("icap://") else {
        return status_only(400, "Bad Request");
    };
    let service = rest
        .split_once('/')
        .map(|(_, s)| s.split('?').next().unwrap_or(""))
        .unwrap_or("")
        .to_owned();
    let Some(svc) = shared.services.iter().find(|s| s.name == service) else {
        return status_only(404, "ICAP Service Not Found");
    };
    if method == "OPTIONS" {
        let headers = vec![
            ("Methods".into(), svc.methods.join(", ")),
            ("Service".into(), "NetGet ICAP".into()),
            ("ISTag".into(), ISTAG.into()),
            (
                "Max-Connections".into(),
                crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS.to_string(),
            ),
            ("Options-TTL".into(), "3600".into()),
            ("Allow".into(), "204".into()),
            ("Preview".into(), shared.preview.to_string()),
            ("Transfer-Preview".into(), "*".into()),
        ];
        outcome(ctx, id, "OPTIONS", "protocol_options");
        return wire::message("ICAP/1.0 200 OK", headers, None, None, None, "");
    }
    if method != "REQMOD" && method != "RESPMOD" {
        return status_only(501, "Method Not Implemented");
    }
    if !svc.methods.iter().any(|m| m == method) {
        return status_only(405, "Method Not Allowed For Service");
    }
    let encapsulated = wire::parse_encapsulated(
        wire::header(&head.headers, "Encapsulated").context("Encapsulated header is required")?,
    )?;
    let mut req = None;
    let mut res = None;
    let mut body = None;
    for (i, (entity, offset)) in encapsulated.iter().enumerate() {
        let next = encapsulated.get(i + 1).map(|(_, o)| *o);
        match entity.as_str() {
            "req-hdr" => {
                req = Some(
                    wire::read_exact_head(
                        reader,
                        next.context("req-hdr must be followed by another entity")? - offset,
                    )
                    .await?,
                )
            }
            "res-hdr" => {
                res = Some(
                    wire::read_exact_head(
                        reader,
                        next.context("res-hdr must be followed by another entity")? - offset,
                    )
                    .await?,
                )
            }
            "req-body" | "res-body" => {
                let first = wire::read_chunks(reader, 0).await?;
                let mut data = first.data;
                if wire::header(&head.headers, "Preview").is_some() && !first.ieof {
                    wire::write_all(writer, b"ICAP/1.0 100 Continue\r\n\r\n").await?;
                    let rest = wire::read_chunks(reader, data.len()).await?;
                    data.extend(rest.data);
                }
                body = Some(data);
            }
            _ => {}
        }
    }
    ensure!(
        method == "RESPMOD" || res.is_none(),
        "REQMOD carries no response head"
    );
    let request = Request {
        method: method.clone(),
        service,
        headers: head.headers.clone(),
        req,
        res,
        body,
    };
    let allow_204 = wire::header(&request.headers, "Allow")
        .is_some_and(|v| v.split(',').any(|t| t.trim() == "204"));
    let mut data = json!({
        "method": request.method,
        "service": request.service,
        "allow_204": allow_204,
        "icap_headers": request.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
    });
    if let Some(h) = &request.req {
        data["http_request"] = wire::head_json(h, true);
    }
    if let Some(h) = &request.res {
        data["http_response"] = wire::head_json(h, false);
    }
    if let Some(b) = &request.body {
        for (k, v) in wire::body_json(b).as_object().expect("object") {
            data[k] = v.clone();
        }
    }
    let label = request.method.clone();
    let decision = decide(shared, id, Event::new(&actions::REQUEST_EVENT, data)).await;
    let decision = match decision {
        Err(_) => return status_only(500, "Server Error"),
        Ok(v) => match actions::validate(&v) {
            Ok(()) => v,
            Err(_) => {
                outcome(ctx, id, &label, "fail_closed_invalid_reply");
                return status_only(500, "Server Error");
            }
        },
    };
    let verdict = decision["verdict"].as_str().unwrap_or("error");
    let istag = vec![("ISTag".to_owned(), ISTAG.to_owned())];
    let text_body = decision["body_text"]
        .as_str()
        .map(|s| s.as_bytes().to_vec());
    let reply = match verdict {
        "no_modification" => {
            outcome(ctx, id, &label, "model_pass");
            if allow_204 {
                status_only(204, "No Content")
            } else if request.method == "REQMOD" {
                wire::message(
                    "ICAP/1.0 200 OK",
                    istag,
                    request.req.as_ref(),
                    None,
                    request.body.as_deref(),
                    "req-body",
                )
            } else {
                wire::message(
                    "ICAP/1.0 200 OK",
                    istag,
                    None,
                    request.res.as_ref(),
                    request.body.as_deref(),
                    "res-body",
                )
            }
        }
        "block" => {
            outcome(ctx, id, &label, "model_block");
            let page = text_body.unwrap_or_else(|| b"Blocked by policy".to_vec());
            let mut head = HttpHead {
                start: ["HTTP/1.1".into(), "403".into(), "Forbidden".into()],
                headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
            };
            wire::set_length(&mut head, page.len());
            wire::message(
                "ICAP/1.0 200 OK",
                istag,
                None,
                Some(&head),
                Some(&page),
                "res-body",
            )
        }
        // RFC 3507 §4.4.1: a RESPMOD response carries only [res-hdr] res-body.
        "modify" if request.method == "RESPMOD" && decision["http_response"].is_null() => {
            outcome(ctx, id, &label, "fail_closed_invalid_reply");
            status_only(500, "Server Error")
        }
        "modify" => {
            outcome(ctx, id, &label, "model_modify");
            if !decision["http_response"].is_null() {
                let mut head = wire::response_head(&decision["http_response"])?;
                let body = text_body
                    .or_else(|| request.body.clone())
                    .unwrap_or_default();
                wire::set_length(&mut head, body.len());
                wire::message(
                    "ICAP/1.0 200 OK",
                    istag,
                    None,
                    Some(&head),
                    Some(&body),
                    "res-body",
                )
            } else {
                let mut head = wire::request_head(&decision["http_request"])?;
                let body = text_body.or_else(|| request.body.clone());
                if let Some(b) = &body {
                    wire::set_length(&mut head, b.len());
                }
                wire::message(
                    "ICAP/1.0 200 OK",
                    istag,
                    Some(&head),
                    None,
                    body.as_deref(),
                    "req-body",
                )
            }
        }
        _ => {
            outcome(ctx, id, &label, "model_error");
            let code = decision["status"].as_u64().unwrap_or(500) as u16;
            let reason = match code {
                400 => "Bad Request",
                403 => "Forbidden",
                404 => "ICAP Service Not Found",
                503 => "Service Unavailable",
                _ => "Server Error",
            };
            status_only(code, reason)
        }
    };
    reply
}
