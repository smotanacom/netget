//! Apache Thrift RPC client over the server's IDL parser and codecs.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::thrift::codec::{self, Message, Protocol, Tv};
use crate::server::thrift::idl::{self, Idl, Type};
use crate::server::thrift::value;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ThriftClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value as Json};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_PROTOCOL: &str = "binary";
pub const DEFAULT_TRANSPORT: &str = "framed";
const TIMEOUT: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_millis(20);

struct Session {
    stream: TcpStream,
    idl: Idl,
    service: String,
    protocol: Protocol,
    framed: bool,
    seqid: i32,
    buf: Vec<u8>,
}

impl Session {
    async fn call(&mut self, method: &str, args: &Map<String, Json>) -> Result<Option<Json>> {
        let svc = self.idl.service(Some(&self.service))?;
        let f = self
            .idl
            .function(svc, method)
            .with_context(|| format!("{} has no function {method}", self.service))?
            .clone();
        let body =
            value::struct_from_json(&self.idl, &format!("{method} arguments"), &f.args, args, 0)?;
        self.seqid = self.seqid.wrapping_add(1);
        let msg = Message {
            name: method.to_owned(),
            kind: if f.oneway { codec::ONEWAY } else { codec::CALL },
            seqid: self.seqid,
            body,
        };
        let mut out = codec::encode(&msg, self.protocol);
        if self.framed {
            let mut fr = (out.len() as u32).to_be_bytes().to_vec();
            fr.extend(out);
            out = fr;
        }
        self.stream.write_all(&out).await?;
        if f.oneway {
            return Ok(None);
        }
        let reply = tokio::time::timeout(TIMEOUT, self.read())
            .await
            .context("no reply in time")??;
        ensure!(
            reply.seqid == self.seqid,
            "reply for sequence {} while waiting for {}",
            reply.seqid,
            self.seqid
        );
        let mut out = json!({"method": method, "result": null});
        match reply.kind {
            codec::EXCEPTION => {
                let message = match reply.body.field(1) {
                    Some(Tv::Bin(b)) => String::from_utf8_lossy(b).into_owned(),
                    _ => String::new(),
                };
                let kind = match reply.body.field(2) {
                    Some(Tv::I32(k)) => *k,
                    _ => 0,
                };
                out["application_error"] = json!({"type": kind, "message": message});
            }
            codec::REPLY => {
                let Tv::Struct(fields) = &reply.body else {
                    bail!("the reply is not a struct")
                };
                match fields.first() {
                    Some((0, v)) => {
                        out["result"] = value::to_json(&self.idl, f.returns.as_ref(), v)
                    }
                    Some((id, v)) => match f.throws.iter().find(|t| t.id == *id) {
                        Some(t) => {
                            out["exception"] = json!({"name": t.name, "type": match &t.ty { Type::Struct(n) => n.clone(), _ => String::new() }, "value": value::to_json(&self.idl, Some(&t.ty), v)})
                        }
                        None => {
                            out["exception"] = json!({"name": format!("_{id}"), "value": value::to_json(&self.idl, None, v)})
                        }
                    },
                    None => ensure!(f.returns.is_none(), "{method} returned no value"),
                }
            }
            other => bail!("unexpected message type {other} in reply"),
        }
        Ok(Some(out))
    }

    async fn read(&mut self) -> Result<Message> {
        // As in the server: an unframed reply is decoded again only once the buffer has grown by
        // half or the server pauses, never once per read.
        let mut tried_at = 0usize;
        let mut quiet = false;
        loop {
            if self.framed && self.buf.len() >= 4 {
                let n = u32::from_be_bytes(self.buf[..4].try_into()?) as usize;
                ensure!(
                    n <= codec::MAX_MESSAGE,
                    "frame of {n} bytes exceeds the bound"
                );
                if self.buf.len() >= 4 + n {
                    let (m, _, _) = codec::decode(&self.buf[4..4 + n])?;
                    self.buf.drain(..4 + n);
                    return Ok(m);
                }
            } else if !self.framed && (quiet || self.buf.len() > tried_at + tried_at / 2) {
                match codec::decode(&self.buf) {
                    Ok((m, _, used)) => {
                        self.buf.drain(..used);
                        return Ok(m);
                    }
                    Err(e) if codec::is_incomplete(&e) => {
                        ensure!(self.buf.len() < codec::MAX_MESSAGE, "reply over the bound");
                        tried_at = self.buf.len();
                    }
                    Err(e) => return Err(e),
                }
            }
            quiet = false;
            let mut chunk = [0u8; 16 * 1024];
            let n = if !self.framed && self.buf.len() > tried_at {
                match tokio::time::timeout(QUIET, self.stream.read(&mut chunk)).await {
                    Ok(r) => r?,
                    Err(_) => {
                        quiet = true;
                        continue;
                    }
                }
            } else {
                self.stream.read(&mut chunk).await?
            };
            ensure!(n > 0, "the server closed the connection");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx
        .startup_params
        .as_ref()
        .context("the thrift client needs the idl startup parameter")?;
    let src = p
        .get_optional_string("idl")?
        .context("the thrift client needs the idl startup parameter")?;
    let idl = idl::parse(&src).context("the IDL does not parse")?;
    let service = idl
        .service(p.get_optional_string("service")?.as_deref())?
        .name
        .clone();
    let protocol = match p
        .get_optional_string("protocol")?
        .as_deref()
        .unwrap_or(DEFAULT_PROTOCOL)
    {
        "binary" => Protocol::Binary,
        "compact" => Protocol::Compact,
        other => bail!("protocol is binary or compact, not {other:?}"),
    };
    let framed = match p
        .get_optional_string("transport")?
        .as_deref()
        .unwrap_or(DEFAULT_TRANSPORT)
    {
        "framed" => true,
        "buffered" => false,
        other => bail!("transport is framed or buffered, not {other:?}"),
    };
    let stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("Thrift connect timed out")??;
    let local = stream.local_addr()?;
    let functions: Vec<Json> = idl
        .service(Some(&service))?
        .functions
        .iter()
        .map(|f| json!({"name": f.name, "args": f.args.iter().map(|a| a.name.clone()).collect::<Vec<_>>(), "returns": f.returns.as_ref().map(|t| format!("{t:?}")), "oneway": f.oneway}))
        .collect();
    let mut session = Session {
        stream,
        idl,
        service: service.clone(),
        protocol,
        framed,
        seqid: 0,
        buf: Vec::new(),
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Json>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"service": service, "functions": functions}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = ThriftClientProtocol;
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
                    .warn(format!("Thrift client handler: {e}")),
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &mut session, external, internal_rx, &event_tx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("Thrift client ended: {e:#}"));
        }
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
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

async fn run(
    ctx: &ConnectContext,
    s: &mut Session,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Json>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let outcome = match ThriftClientProtocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => {
                let args = action["args"].as_object().cloned().unwrap_or_default();
                match s
                    .call(action["method"].as_str().unwrap_or_default(), &args)
                    .await
                {
                    Ok(Some(result)) => {
                        events
                            .send(Event::new(&actions::RESULT_EVENT, result))
                            .await
                            .ok();
                        Ok(ClientSendOutcome::Sent { bytes_sent: 0 })
                    }
                    Ok(None) => Ok(ClientSendOutcome::Sent { bytes_sent: 0 }),
                    Err(e) => Err(e),
                }
            }
        };
        if let Some(c) = command {
            let logged = outcome
                .as_ref()
                .map(|o| serde_json::to_value(o).unwrap_or(Json::Null))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Thrift",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "method": action["method"]}),
                    vec![logged],
                )
                .await;
            crate::client::command_support::reply(c, outcome);
        } else if let Err(e) = outcome {
            Log::new(Some(&ctx.status_tx)).warn(format!("Thrift call failed: {e:#}"));
        }
    }
}
