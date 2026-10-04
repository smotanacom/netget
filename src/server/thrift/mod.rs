//! Apache Thrift RPC server for a service declared in IDL. Rust owns the transport (framed or
//! unframed, detected per connection), the protocol (binary or compact, detected per message),
//! method lookup, argument decoding and result encoding by IDL type; the handler answers calls.
pub mod actions;
pub mod codec;
pub mod idl;
pub mod value;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{bail, ensure, Context, Result};
use codec::{Message, Tv};
use idl::{Function, Idl, Service, Type};
use serde_json::{json, Value as Json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long an unframed peer may pause before a partial message is decoded again.
const QUIET: Duration = Duration::from_millis(20);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

struct Shared {
    ctx: SpawnContext,
    idl: Idl,
    service: String,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx
        .startup_params
        .as_ref()
        .context("the thrift server needs the idl startup parameter")?;
    let src = p
        .get_optional_string("idl")?
        .context("the thrift server needs the idl startup parameter")?;
    let idl = idl::parse(&src).context("the IDL does not parse")?;
    let service = idl
        .service(p.get_optional_string("service")?.as_deref())?
        .name
        .clone();
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Thrift service {service} on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        idl,
        service,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                b"",
                "Thrift",
                Some(&shared.ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
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
                            .debug(format!("Thrift connection {id}: {e:#}"));
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
    let summary = format!("Thrift connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Json, String> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::ThriftProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(crate::utils::WireFailure::classify(&e).text().to_owned());
        }
    };
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match (result.failures.is_empty(), answers.len()) {
        (true, 1) => Ok(answers.remove(0)),
        (true, 0) => {
            outcome(ctx, id, operation, "model_silent");
            Err("the service could not answer this call".into())
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Err("the service could not answer this call".into())
        }
    }
}

fn type_name(t: &Type) -> String {
    match t {
        Type::List(e) => format!("list<{}>", type_name(e)),
        Type::Set(e) => format!("set<{}>", type_name(e)),
        Type::Map(k, v) => format!("map<{},{}>", type_name(k), type_name(v)),
        Type::Struct(n) | Type::Enum(n) => n.clone(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// The reply to a call: a REPLY with the result struct, or an EXCEPTION.
fn reply(idl: &Idl, f: &Function, call: &Message, answer: &Json) -> Message {
    let app = |text: &str, kind: i32| Message {
        name: call.name.clone(),
        kind: codec::EXCEPTION,
        seqid: call.seqid,
        body: codec::application_exception(text, kind),
    };
    let ok = |fields: Vec<(i16, Tv)>| Message {
        name: call.name.clone(),
        kind: codec::REPLY,
        seqid: call.seqid,
        body: Tv::Struct(fields),
    };
    match answer["type"].as_str() {
        Some("thrift_return") => match (&f.returns, answer.get("value").filter(|v| !v.is_null())) {
            (None, _) => ok(vec![]),
            (Some(t), Some(v)) => match value::from_json(idl, t, v, 0) {
                Ok(tv) => ok(vec![(0, tv)]),
                Err(e) => app(
                    &format!("the result does not match {}: {e:#}", type_name(t)),
                    codec::INTERNAL_ERROR,
                ),
            },
            (Some(t), None) => app(
                &format!(
                    "{} returns {}, and no value was given",
                    f.name,
                    type_name(t)
                ),
                codec::INTERNAL_ERROR,
            ),
        },
        Some("thrift_throw") => {
            let which = answer["exception"].as_str().unwrap_or_default();
            match f
                .throws
                .iter()
                .find(|t| t.name == which || matches!(&t.ty, Type::Struct(n) if n == which))
            {
                Some(field) => match value::from_json(idl, &field.ty, &answer["value"], 0) {
                    Ok(tv) => ok(vec![(field.id, tv)]),
                    Err(e) => app(
                        &format!("the exception does not match: {e:#}"),
                        codec::INTERNAL_ERROR,
                    ),
                },
                None => app(
                    &format!("{} does not declare {which}", f.name),
                    codec::INTERNAL_ERROR,
                ),
            }
        }
        _ => app(
            answer["message"].as_str().unwrap_or("error"),
            codec::INTERNAL_ERROR,
        ),
    }
}

async fn connection(shared: &Shared, id: ConnectionId, mut stream: TcpStream) -> Result<()> {
    let ctx = &shared.ctx;
    let service: &Service = shared.idl.service(Some(&shared.service))?;
    let mut buf: Vec<u8> = Vec::new();
    // Framed messages start with a 4-byte length; unframed ones with a protocol marker.
    let mut framed: Option<bool> = None;
    loop {
        // Gather one message. An unframed message's length is known only by decoding it, so a
        // failed attempt is retried once the buffer has grown by half or the peer goes quiet:
        // a peer trickling bytes buys a bounded number of decodes rather than one per read.
        let mut tried_at = 0usize;
        let mut quiet = false;
        let (bytes, consumed) = loop {
            if framed.is_none() && !buf.is_empty() {
                framed = Some(!(buf[0] == 0x80 || buf[0] == 0x82));
            }
            match framed {
                Some(true) if buf.len() >= 4 => {
                    let n = u32::from_be_bytes(buf[..4].try_into()?) as usize;
                    ensure!(
                        n <= codec::MAX_MESSAGE,
                        "frame of {n} bytes exceeds the bound"
                    );
                    if buf.len() >= 4 + n {
                        break (buf[4..4 + n].to_vec(), 4 + n);
                    }
                }
                Some(false) if quiet || buf.len() > tried_at + tried_at / 2 => {
                    match codec::decode(&buf) {
                        Ok((_, _, used)) => break (buf[..used].to_vec(), used),
                        Err(e) if codec::is_incomplete(&e) => {
                            ensure!(buf.len() < codec::MAX_MESSAGE, "message over the bound");
                            tried_at = buf.len();
                        }
                        Err(e) => return Err(e),
                    }
                }
                _ => {}
            }
            quiet = false;
            let untried = framed == Some(false) && buf.len() > tried_at;
            let wait = if untried { QUIET } else { IDLE_TIMEOUT };
            let mut chunk = [0u8; 16 * 1024];
            let n = match tokio::time::timeout(wait, stream.read(&mut chunk)).await {
                Ok(r) => r?,
                Err(_) if untried => {
                    quiet = true;
                    continue;
                }
                Err(_) => return Ok(()),
            };
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        buf.drain(..consumed);
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(consumed as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let (call, protocol, used) = match codec::decode(&bytes) {
            Ok(v) => v,
            Err(e) => bail!("undecodable message: {e:#}"),
        };
        ensure!(used == bytes.len(), "trailing bytes after the message");
        let response = handle(shared, id, service, &call).await;
        if let Some(m) = response {
            let mut out = codec::encode(&m, protocol);
            if framed == Some(true) {
                let mut f = (out.len() as u32).to_be_bytes().to_vec();
                f.extend(out);
                out = f;
            }
            stream.write_all(&out).await?;
            ctx.state
                .update_connection_stats(
                    ctx.server_id,
                    id,
                    None,
                    Some(out.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
    }
}

/// Answer one message; None for a oneway call.
async fn handle(
    shared: &Shared,
    id: ConnectionId,
    service: &Service,
    call: &Message,
) -> Option<Message> {
    let ctx = &shared.ctx;
    let idl = &shared.idl;
    let app = |text: String, kind: i32| {
        Some(Message {
            name: call.name.clone(),
            kind: codec::EXCEPTION,
            seqid: call.seqid,
            body: codec::application_exception(&text, kind),
        })
    };
    if call.kind != codec::CALL && call.kind != codec::ONEWAY {
        outcome(ctx, id, "call", "protocol_refusal");
        return app(
            format!("message type {} is not a call", call.kind),
            codec::PROTOCOL_ERROR,
        );
    }
    let Some(f) = idl.function(service, &call.name) else {
        outcome(ctx, id, "call", "protocol_refusal");
        return app(
            format!("Invalid method name: '{}'", call.name),
            codec::UNKNOWN_METHOD,
        );
    };
    let Tv::Struct(fields) = &call.body else {
        return app("arguments are not a struct".into(), codec::PROTOCOL_ERROR);
    };
    if let Some(missing) = f
        .args
        .iter()
        .find(|a| a.required && !fields.iter().any(|(i, _)| *i == a.id))
    {
        outcome(ctx, id, "call", "protocol_refusal");
        return app(
            format!("Required field '{}' was not present", missing.name),
            codec::PROTOCOL_ERROR,
        );
    }
    let args = value::struct_json(idl, &f.args, fields);
    let throws: Vec<Json> = f
        .throws
        .iter()
        .map(|t| json!({"name": t.name, "type": type_name(&t.ty)}))
        .collect();
    let oneway = f.oneway || call.kind == codec::ONEWAY;
    let event = Event::new(
        &actions::CALL_EVENT,
        json!({"service": service.name, "method": f.name, "oneway": oneway, "args": args, "returns": f.returns.as_ref().map(type_name).unwrap_or_else(|| "void".into()), "throws": throws}),
    );
    let answer = ask(shared, id, event, "call").await;
    if oneway {
        return None;
    }
    match answer {
        Ok(a) => {
            outcome(
                ctx,
                id,
                "call",
                if a["type"] == "thrift_return" {
                    "model_answer"
                } else {
                    "model_reject"
                },
            );
            Some(reply(idl, f, call, &a))
        }
        Err(text) => app(text, codec::INTERNAL_ERROR),
    }
}
