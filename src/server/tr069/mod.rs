//! TR-069 ACS (server role). A CWMP session is a run of HTTP POSTs from the device bound by a
//! cookie: Inform, then empty posts and RPC responses, each answered with the next queued RPC
//! or with 204 to end it. Rust owns sessions and envelopes; the model fills the queue.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, server::conn::http1, service::service_fn, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_SESSION_TIMEOUT_SECS: u64 = 3600;
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// Sessions open at once; past it a new Inform is answered 503.
pub const MAX_SESSIONS: usize = 1024;
/// RPCs one session may send in all, so a model that keeps asking cannot hold a device forever.
pub const MAX_SESSION_RPCS: usize = 256;
const COOKIE: &str = "netget_cwmp";

struct Session {
    device_id: Value,
    queue: VecDeque<Value>,
    /// The RPC sent and not yet answered: (cwmp ID, method name).
    in_flight: Option<(String, String)>,
    sent: usize,
    last: crate::utils::clock::Instant,
}

struct Acs {
    sessions: Mutex<HashMap<String, Session>>,
    next: AtomicU64,
    timeout: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let secs = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("session_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(SESSION_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_SESSION_TIMEOUT_SECS).contains(&secs),
        "session_timeout_secs must be between 1 and {MAX_SESSION_TIMEOUT_SECS}"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("TR-069 ACS listening on {local}"));
    let acs = Arc::new(Acs {
        sessions: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
        timeout: Duration::from_secs(secs),
    });
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "TR-069",
                Some(&ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
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
            let child = ctx.clone();
            let acs = acs.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let svc_ctx = child.clone();
                    let service = service_fn(move |request| {
                        let ctx = svc_ctx.clone();
                        let acs = acs.clone();
                        async move { Ok::<_, Infallible>(handle(&ctx, &acs, id, peer, request).await) }
                    });
                    let _ = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_buf_size(64 * 1024)
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                    child
                        .state
                        .update_connection_status(child.server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn soap(body: String, cookie: Option<&str>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/xml; charset=\"utf-8\""),
    );
    if let Some(c) = cookie
        .and_then(|c| hyper::header::HeaderValue::from_str(&format!("{COOKIE}={c}; Path=/")).ok())
    {
        r.headers_mut().insert(hyper::header::SET_COOKIE, c);
    }
    r
}

fn status(code: StatusCode) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = code;
    r
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("TR-069 connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The model's RPCs for an event, or None when there is no usable answer.
async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Option<Vec<Value>> {
    match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::Tr069Protocol,
    )
    .await
    {
        Ok(r) if r.failures.is_empty() => {
            let mut stack = r.protocol_results;
            let mut out = Vec::new();
            while let Some(r) = stack.pop() {
                match r {
                    ActionResult::Custom { data, .. } => out.push(data),
                    ActionResult::Multiple(items) => stack.extend(items),
                    _ => {}
                }
            }
            out.reverse();
            if out.len() > actions::MAX_QUEUED {
                outcome(ctx, id, operation, "fail_closed_invalid_reply");
                return None;
            }
            outcome(
                ctx,
                id,
                operation,
                if out.is_empty() {
                    "model_silent"
                } else {
                    "model_answer"
                },
            );
            Some(out)
        }
        Ok(_) => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            None
        }
        Err(_) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            None
        }
    }
}

/// An action as the RPC envelope body it sends, and the method's name.
fn rpc(a: &Value) -> Option<(String, &'static str)> {
    let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
    Some(match a["type"].as_str()? {
        actions::GPV => {
            let names: Vec<String> = a["names"]
                .as_array()?
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect();
            (wire::get_parameter_values(&names), "GetParameterValues")
        }
        actions::SPV => (
            wire::set_parameter_values(
                &wire::triples(&a["parameters"], a.get("types").filter(|t| !t.is_null())).ok()?,
                &s("parameter_key"),
            ),
            "SetParameterValues",
        ),
        actions::GPN => (
            wire::get_parameter_names(&s("path"), a["next_level"].as_bool().unwrap_or(true)),
            "GetParameterNames",
        ),
        actions::ADD => (
            wire::add_object(&s("object"), &s("parameter_key")),
            "AddObject",
        ),
        actions::DELETE => (
            wire::delete_object(&s("object"), &s("parameter_key")),
            "DeleteObject",
        ),
        actions::REBOOT => (wire::reboot(&s("command_key")), "Reboot"),
        actions::FACTORY_RESET => (wire::factory_reset(), "FactoryReset"),
        _ => return None,
    })
}

/// The next queued RPC for a session, or 204 to end it.
fn next(acs: &Acs, sid: &str) -> Response<Full<Bytes>> {
    let mut sessions = acs.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = sessions.get_mut(sid) else {
        return status(StatusCode::NO_CONTENT);
    };
    s.last = crate::utils::clock::Instant::now();
    while let Some(a) = s.queue.pop_front() {
        if s.sent >= MAX_SESSION_RPCS {
            break;
        }
        if let Some((body, method)) = rpc(&a) {
            let id = format!("netget-{}", acs.next.fetch_add(1, Ordering::Relaxed));
            s.in_flight = Some((id.clone(), method.to_string()));
            s.sent += 1;
            return soap(wire::envelope(&id, &body), None);
        }
    }
    sessions.remove(sid);
    status(StatusCode::NO_CONTENT)
}

fn session_cookie(req: &Request<Incoming>) -> Option<String> {
    req.headers()
        .get_all(hyper::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_string)
        })
}

async fn handle(
    ctx: &SpawnContext,
    acs: &Arc<Acs>,
    id: ConnectionId,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let log = Log::new(Some(&ctx.status_tx));
    if req.method() != hyper::Method::POST {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let sid = session_cookie(&req);
    let body = match Limited::new(req.into_body(), wire::MAX_ENVELOPE)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            log.warn(format!(
                "TR-069 connection {id}: envelope larger than {} bytes refused",
                wire::MAX_ENVELOPE
            ));
            return status(StatusCode::PAYLOAD_TOO_LARGE);
        }
    };
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(body.len() as u64),
            None,
            Some(1),
            None,
        )
        .await;
    {
        let now = crate::utils::clock::Instant::now();
        acs.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, s| now.duration_since(s.last) < acs.timeout);
    }
    let known = sid
        .as_ref()
        .filter(|s| {
            acs.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(*s)
        })
        .cloned();
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        // An empty post: the device has nothing more to say; send what is queued.
        return match known {
            Some(sid) => next(acs, &sid),
            None => status(StatusCode::NO_CONTENT),
        };
    }
    let msg = match wire::parse(&body) {
        Ok(m) => m,
        Err(e) => {
            log.warn(format!(
                "TR-069 connection {id}: unreadable envelope: {e:#}"
            ));
            return soap(
                wire::envelope("", &wire::fault(8003, "Invalid arguments")),
                None,
            );
        }
    };
    let rid = msg.id.clone().unwrap_or_default();
    match (msg.method.as_str(), known) {
        ("Inform", _) => {
            if acs.sessions.lock().unwrap_or_else(|e| e.into_inner()).len() >= MAX_SESSIONS {
                return status(StatusCode::SERVICE_UNAVAILABLE);
            }
            let device_id = msg.content["device_id"].clone();
            let mut data = msg.content.clone();
            data["remote_addr"] = json!(peer.to_string());
            if let Some(o) = data.as_object_mut() {
                o.remove("max_envelopes");
                o.remove("current_time");
            }
            let Some(queued) =
                ask(ctx, id, Event::new(&actions::INFORM_EVENT, data), "inform").await
            else {
                return soap(
                    wire::envelope(
                        &rid,
                        &wire::fault(8002, crate::utils::WireFailure::Unavailable.text()),
                    ),
                    None,
                );
            };
            if let Some(r) = queued.iter().find(|a| a["type"] == actions::REJECT) {
                outcome(ctx, id, "inform", "model_reject");
                return soap(
                    wire::envelope(
                        &rid,
                        &wire::fault(8001, r["message"].as_str().unwrap_or("Request denied")),
                    ),
                    None,
                );
            }
            let sid = format!("{:032x}", rand::random::<u128>());
            acs.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    sid.clone(),
                    Session {
                        device_id,
                        queue: queued.into(),
                        in_flight: None,
                        sent: 0,
                        last: crate::utils::clock::Instant::now(),
                    },
                );
            soap(wire::envelope(&rid, &wire::inform_response()), Some(&sid))
        }
        ("TransferComplete", Some(sid)) => soap(
            wire::envelope(
                &rid,
                "<cwmp:TransferCompleteResponse></cwmp:TransferCompleteResponse>",
            ),
            Some(&sid),
        ),
        ("GetRPCMethods", Some(_)) => soap(
            wire::envelope(
                &rid,
                &wire::get_rpc_methods_response(&["Inform", "GetRPCMethods", "TransferComplete"]),
            ),
            None,
        ),
        (_, None) => soap(
            wire::envelope(
                &rid,
                &wire::fault(8003, "No session: a session starts with Inform"),
            ),
            None,
        ),
        (method, Some(sid)) => {
            let (device_id, answered, pending) = {
                let mut sessions = acs.sessions.lock().unwrap_or_else(|e| e.into_inner());
                let Some(s) = sessions.get_mut(&sid) else {
                    return status(StatusCode::NO_CONTENT);
                };
                (s.device_id.clone(), s.in_flight.take(), s.queue.len())
            };
            let Some((_, asked)) = answered else {
                // A request we do not serve from a device.
                return soap(
                    wire::envelope(&rid, &wire::fault(8000, "Method not supported")),
                    None,
                );
            };
            let ok = method != "Fault";
            let mut data =
                json!({"device_id": device_id, "method": asked, "ok": ok, "pending": pending});
            if ok {
                data["result"] = msg.content;
            } else {
                data["fault"] = msg.content;
            }
            match ask(
                ctx,
                id,
                Event::new(&actions::RESPONSE_EVENT, data),
                "response",
            )
            .await
            {
                Some(more) => {
                    if let Some(s) = acs
                        .sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(&sid)
                    {
                        for a in more.into_iter().filter(|a| a["type"] != actions::REJECT) {
                            if s.queue.len() < actions::MAX_QUEUED {
                                s.queue.push_back(a);
                            }
                        }
                    }
                    next(acs, &sid)
                }
                None => {
                    // No usable answer mid-session: send nothing more and end it.
                    acs.sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&sid);
                    status(StatusCode::NO_CONTENT)
                }
            }
        }
    }
}
