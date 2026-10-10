//! Cap'n Proto RPC server. Rust owns the encoding, the RPC tables and every bound; one
//! capability (the bootstrap, typed by the startup schema's interface) is exported, and the
//! handler answers each call on it with results or an exception.
pub mod actions;
pub mod layout;
pub mod rpc;
pub mod schema;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, ensure, Context, Result};
use layout::{Message, Target};
use rpc::{CallTarget, Incoming};
use serde_json::{json, Value};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `capnp compile` may take.
pub const COMPILE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bootstrap questions one connection may hold open at once.
pub const MAX_OPEN_BOOTSTRAPS: usize = 64;

/// Longest inline schema accepted.
pub const MAX_INLINE_SCHEMA_BYTES: usize = 256 * 1024;

/// Schema source given inline rather than as a path: it has a declaration in it.
fn is_inline(schema: &str) -> bool {
    schema.contains('{') || schema.contains('\n')
}

/// Whether the configured schema is source that `capnp` must compile.
pub fn needs_compiler(params: Option<&Value>) -> bool {
    params
        .and_then(|p| p.get("schema"))
        .and_then(Value::as_str)
        .is_none_or(|s| is_inline(s) || s.trim().ends_with(".capnp"))
}

async fn compile(path: &std::path::Path) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("capnp");
    command
        .arg("compile")
        .arg("-o-")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        command.arg(format!("--src-prefix={}", dir.display()));
        command.arg(format!("--import-path={}", dir.display()));
    }
    let out = tokio::time::timeout(COMPILE_TIMEOUT, command.output())
        .await
        .context("capnp compile timed out")?
        .context("could not run capnp (install capnproto)")?;
    ensure!(
        out.status.success(),
        "capnp compile failed: {}",
        crate::utils::truncate::truncate_for_log(String::from_utf8_lossy(&out.stderr).trim(), 1024)
    );
    Ok(out.stdout)
}

/// Load a schema: inline Cap'n Proto source, a `.capnp` file (both compiled with
/// `capnp compile -o-`), or anything else as that command's output. Inline source without a
/// file id gets one derived from its text, so the same text always has the same ids.
pub async fn load_schema(schema: &str) -> Result<schema::Schema> {
    let schema = schema.trim();
    let bytes = if is_inline(schema) {
        ensure!(
            schema.len() <= MAX_INLINE_SCHEMA_BYTES,
            "inline schema longer than {MAX_INLINE_SCHEMA_BYTES} bytes"
        );
        let has_id = schema
            .lines()
            .map(str::trim)
            .any(|l| l.starts_with("@0x") && l.ends_with(';'));
        let source = if has_id {
            schema.to_string()
        } else {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            schema.hash(&mut h);
            format!("@{:#018x};\n{schema}\n", h.finish() | 1 << 63)
        };
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("inline.capnp");
        tokio::fs::write(&file, source).await?;
        compile(&file).await?
    } else if schema.ends_with(".capnp") {
        compile(std::path::Path::new(schema)).await?
    } else {
        let meta = tokio::fs::metadata(schema)
            .await
            .with_context(|| format!("cannot read schema {schema}"))?;
        ensure!(
            meta.len() <= schema::MAX_SCHEMA_BYTES as u64,
            "compiled schema too large"
        );
        tokio::fs::read(schema).await?
    };
    schema::Schema::load(&bytes)
}

pub struct Config {
    pub schema: schema::Schema,
    pub interface: u64,
    pub interface_name: String,
    pub idle: Duration,
}

async fn config(ctx: &SpawnContext) -> Result<Config> {
    let params = ctx
        .startup_params
        .as_ref()
        .context("schema and interface are required")?;
    let path = params.get_string("schema")?;
    let name = params.get_string("interface")?;
    let secs = params
        .get_optional_u64("idle_timeout_secs")?
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    let schema = load_schema(&path).await?;
    let iface = schema.interface(&name)?;
    let (interface, interface_name) = (iface.id, iface.name.clone());
    Ok(Config {
        schema,
        interface,
        interface_name,
        idle: Duration::from_secs(secs),
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let config = Arc::new(config(&ctx).await?);
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Cap'n Proto RPC listening on {addr}, exporting {}",
        config.interface_name
    ));
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"",
                "Cap'n Proto RPC",
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
                        local_addr: addr,
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
            let config = config.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, &config, id, socket, peer).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("Cap'n Proto RPC connection {id} ended: {e:#}"));
                    }
                    child
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(addr)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, method: &str, decision: &str) {
    let summary = format!("Cap'n Proto RPC connection {id} method={method} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

pub async fn send<W: AsyncWrite + Unpin>(writer: &mut W, words: &[u64]) -> Result<usize> {
    let bytes = layout::frame(words);
    tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(&bytes))
        .await
        .context("write deadline")??;
    Ok(bytes.len())
}

/// The handler's answer to one call, as a Return message.
async fn answer_call(
    ctx: &SpawnContext,
    config: &Config,
    id: ConnectionId,
    peer: SocketAddr,
    (question, interface, method): (u32, u64, u16),
    content: Target<'_>,
) -> Result<Vec<u64>> {
    let methods = config.schema.methods_of(config.interface);
    let Some((_, iface, m)) = methods
        .iter()
        .find(|(i, _, m)| *i == interface && m.id == method)
    else {
        return rpc::return_exception(
            question,
            &format!("method @{method} of interface {interface:#x} is not implemented"),
            rpc::EXC_UNIMPLEMENTED,
        );
    };
    let params = match content {
        Target::Struct(s) => config.schema.to_json(m.params, s)?,
        Target::Null => json!({}),
        _ => bail!("call parameters are not a struct"),
    };
    let event = Event::new(
        &actions::CALL_EVENT,
        json!({"interface": iface.name, "method": m.name, "params": params,
               "results_shape": config.schema.describe(m.results), "remote_addr": peer.to_string()}),
    );
    let failed = |e: Option<&anyhow::Error>| {
        let text = match e {
            Some(e) => crate::utils::wire_failure::prefixed_wire_failure_text(e),
            None => crate::utils::WireFailure::Unavailable.prefixed_text(),
        };
        let kind = if e.is_some_and(crate::llm::is_overload_error) {
            rpc::EXC_OVERLOADED
        } else {
            rpc::EXC_FAILED
        };
        rpc::return_exception(question, text, kind)
    };
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::CapnpRpcProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, &m.name, "fail_closed_llm_error");
            return failed(Some(&e));
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, &m.name, "fail_closed_invalid_reply");
        return failed(None);
    }
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("capnp_") => {
                answers.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match answers.as_slice() {
        [(name, data)] if name == "capnp_return" => {
            let (dw, pc) = config.schema.struct_size(m.results)?;
            let built = rpc::return_results(question, |b, payload| {
                let s = b.init_struct(payload, 0, dw, pc)?;
                config.schema.from_json(b, m.results, s, &data["results"])
            });
            match built {
                Ok(words) => {
                    outcome(ctx, id, &m.name, "model_answer");
                    Ok(words)
                }
                Err(e) => {
                    Log::new(Some(&ctx.status_tx)).error(format!(
                        "Cap'n Proto RPC results for {} do not fit the schema: {e:#}",
                        m.name
                    ));
                    outcome(ctx, id, &m.name, "fail_closed_invalid_reply");
                    failed(None)
                }
            }
        }
        [(name, data)] if name == "capnp_exception" => {
            let (reason, kind) = actions::check_exception(data)?;
            outcome(ctx, id, &m.name, "model_reject");
            rpc::return_exception(question, &reason, kind)
        }
        [] => {
            outcome(ctx, id, &m.name, "model_silent");
            failed(None)
        }
        _ => {
            outcome(ctx, id, &m.name, "fail_closed_invalid_reply");
            failed(None)
        }
    }
}

async fn session(
    ctx: &SpawnContext,
    config: &Config,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    // Bootstrap questions whose answer (the exported capability) may still be pipelined on.
    let mut bootstraps: HashSet<u32> = HashSet::new();
    while let Some(segments) = layout::read_message(&mut reader, config.idle).await? {
        ctx.state
            .update_connection_stats(ctx.server_id, id, None, None, Some(1), None)
            .await;
        let msg = Message::new(segments);
        let reply = match rpc::decode(&msg)? {
            Incoming::Bootstrap { question } => {
                ensure!(
                    bootstraps.len() < MAX_OPEN_BOOTSTRAPS,
                    "more than {MAX_OPEN_BOOTSTRAPS} open bootstrap questions"
                );
                bootstraps.insert(question);
                Some(rpc::return_capability(question, 0)?)
            }
            Incoming::Call {
                question,
                target,
                interface,
                method,
                content,
            } => {
                let on_bootstrap = match target {
                    CallTarget::Imported(0) => true,
                    CallTarget::Promised { question, steps: 0 } => bootstraps.contains(&question),
                    _ => false,
                };
                Some(if on_bootstrap {
                    answer_call(
                        ctx,
                        config,
                        id,
                        peer,
                        (question, interface, method),
                        content,
                    )
                    .await?
                } else {
                    rpc::return_exception(
                        question,
                        "no capability at that target: only the bootstrap capability is exported",
                        rpc::EXC_FAILED,
                    )?
                })
            }
            Incoming::Finish { question } => {
                bootstraps.remove(&question);
                None
            }
            // NetGet asks nothing and holds no imports, so there is nothing to act on.
            Incoming::Return { .. }
            | Incoming::Release
            | Incoming::Resolve
            | Incoming::Unimplemented => None,
            Incoming::Abort { reason } => {
                Log::new(Some(&ctx.status_tx)).info(format!(
                    "Cap'n Proto RPC connection {id} aborted by the peer: {}",
                    crate::utils::truncate::truncate_for_log(&reason, 256)
                ));
                return Ok(());
            }
            Incoming::Other(_) => Some(rpc::unimplemented(&msg)?),
        };
        if let Some(words) = reply {
            let n = send(&mut writer, &words).await?;
            ctx.state
                .update_connection_stats(ctx.server_id, id, None, Some(n as u64), None, Some(1))
                .await;
        }
    }
    Ok(())
}
