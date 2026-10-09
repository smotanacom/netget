//! LPD (RFC 1179) print server. Rust owns the one-command-per-connection framing, the
//! receive-job subcommands and their acknowledgements, and every bound; handlers decide
//! whether each complete job is queued, what lpq is told and what lprm removed. No spool:
//! nothing received is kept after the handler has seen it.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::HashMap, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

/// Deadline for each command line and each file transfer.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 3600;
/// Job-list operands accepted on a queue or removal command.
pub const MAX_LIST_OPERANDS: usize = 64;

#[derive(Clone)]
struct Config {
    queues: Option<Vec<String>>,
    max_job: u64,
    idle: Duration,
}

fn config(ctx: &SpawnContext) -> Result<Config> {
    let params = ctx.startup_params.as_ref();
    let queues = match params
        .map(|p| p.get_optional_array("queues"))
        .transpose()?
        .flatten()
    {
        None => None,
        Some(list) => {
            let mut queues = Vec::new();
            for queue in list {
                let queue = queue
                    .as_str()
                    .filter(|q| wire::valid_token(q))
                    .ok_or_else(|| {
                        anyhow::anyhow!("queues must be printable names without spaces")
                    })?;
                queues.push(queue.to_string());
            }
            anyhow::ensure!(
                !queues.is_empty() && queues.len() <= 256,
                "queues must name 1 to 256 queues"
            );
            Some(queues)
        }
    };
    let max_job = params
        .map(|p| p.get_optional_u64("max_job_bytes"))
        .transpose()?
        .flatten()
        .unwrap_or(wire::DEFAULT_MAX_JOB_BYTES);
    anyhow::ensure!(
        (1..=wire::MAX_JOB_BYTES_LIMIT).contains(&max_job),
        "max_job_bytes must be between 1 and {}",
        wire::MAX_JOB_BYTES_LIMIT
    );
    let idle = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&idle),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Config {
        queues,
        max_job,
        idle: Duration::from_secs(idle),
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let cfg = config(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("LPD listening on {addr}"));
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            // LPD has no greeting or error line; a refused connection is simply closed.
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"",
                "LPD",
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
            let cfg = cfg.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, id, socket, &cfg).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("LPD connection {id} ended: {e}"));
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

async fn write<W: tokio::io::AsyncWrite + Unpin>(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut W,
    bytes: &[u8],
) -> Result<()> {
    tokio::time::timeout(wire::IO_TIMEOUT, w.write_all(bytes))
        .await
        .context("LPD write deadline")??;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            None,
            Some(bytes.len() as u64),
            None,
            Some(1),
        )
        .await;
    Ok(())
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("LPD connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn decision(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    expected: &str,
) -> Result<Value> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::LpdProtocol,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(error);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        anyhow::bail!("LPD handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name == expected => {
                if found.is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    anyhow::bail!("Multiple LPD answers to one event");
                }
                found = Some(data);
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    if found.is_none() {
        outcome(ctx, id, event.id(), "model_silent");
    }
    found.context("Handler did not answer the LPD event")
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    cfg: &Config,
) -> Result<()> {
    let (read, mut out) = tokio::io::split(socket);
    let mut reader = BufReader::new(read);
    let Some(line) = wire::read_line(&mut reader, cfg.idle).await? else {
        return Ok(());
    };
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(line.len() as u64 + 1),
            None,
            Some(1),
            None,
        )
        .await;
    let Some((&code, rest)) = line.split_first() else {
        anyhow::bail!("Empty LPD command");
    };
    let operands = wire::operands(rest);
    let Some(queue) = operands.first().filter(|q| wire::valid_token(q)).cloned() else {
        anyhow::bail!("LPD command without a valid queue name");
    };
    let list: Vec<String> = operands.iter().skip(1).cloned().collect();
    anyhow::ensure!(
        list.len() <= MAX_LIST_OPERANDS + 1 && list.iter().all(|t| wire::valid_token(t)),
        "LPD command operands out of bounds"
    );
    match code {
        wire::CMD_PRINT_WAITING => {
            // RFC 1179 5.1 defines no reply; the request is logged and the connection closed.
            Log::new(Some(&ctx.status_tx))
                .info(format!("LPD print-waiting-jobs for queue {queue}"));
            Ok(())
        }
        wire::CMD_RECEIVE_JOB => receive_job(ctx, id, &mut reader, &mut out, cfg, &queue).await,
        wire::CMD_QUEUE_SHORT | wire::CMD_QUEUE_LONG => {
            let long = code == wire::CMD_QUEUE_LONG;
            let event = Event::new(
                &actions::QUEUE_EVENT,
                json!({"queue": queue, "long": long, "list": list}),
            );
            let text = match decision(ctx, id, event, "lpd_queue_status").await {
                Ok(answer) => {
                    outcome(ctx, id, "lpd_queue_query", "model_answer");
                    let jobs = answer["jobs"].as_array().cloned().unwrap_or_default();
                    wire::render_queue(&queue, long, answer["status"].as_str(), &jobs)
                }
                Err(_) => format!("{queue}: queue status unavailable\n"),
            };
            write(ctx, id, &mut out, text.as_bytes()).await?;
            let _ = out.shutdown().await;
            Ok(())
        }
        wire::CMD_REMOVE_JOBS => {
            let Some((agent, jobs)) = list.split_first() else {
                anyhow::bail!("LPD remove command without an agent");
            };
            let event = Event::new(
                &actions::REMOVE_EVENT,
                json!({"queue": queue, "agent": agent, "list": jobs}),
            );
            // On failure nothing was removed, and nothing is reported as removed.
            let text = match decision(ctx, id, event, "lpd_remove_result").await {
                Ok(answer) => {
                    let removed = answer["removed"].as_array().cloned().unwrap_or_default();
                    outcome(
                        ctx,
                        id,
                        "lpd_remove_request",
                        if removed.is_empty() {
                            "model_reject"
                        } else {
                            "model_answer"
                        },
                    );
                    removed
                        .iter()
                        .map(|job| match job {
                            Value::Number(n) => match n.as_u64() {
                                Some(job) => format!("job {job:03} dequeued\n"),
                                None => format!("job {n} dequeued\n"),
                            },
                            other => format!(
                                "job {} dequeued\n",
                                wire::clean(other.as_str().unwrap_or(""), 64)
                            ),
                        })
                        .collect::<String>()
                }
                Err(_) => String::new(),
            };
            write(ctx, id, &mut out, text.as_bytes()).await?;
            let _ = out.shutdown().await;
            Ok(())
        }
        other => anyhow::bail!("Unknown LPD command code {other:#04x}"),
    }
}

async fn receive_job<R, W>(
    ctx: &SpawnContext,
    id: ConnectionId,
    reader: &mut R,
    out: &mut W,
    cfg: &Config,
    queue: &str,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    if cfg
        .queues
        .as_ref()
        .is_some_and(|q| !q.iter().any(|name| name == queue))
    {
        Log::new(Some(&ctx.status_tx)).info(format!("LPD refused job for unknown queue {queue}"));
        write(ctx, id, out, &[1]).await?;
        return Ok(());
    }
    write(ctx, id, out, &[0]).await?;
    let mut data: HashMap<String, Vec<u8>> = HashMap::new();
    let mut control: Option<(String, wire::ControlFile)> = None;
    let mut job_bytes: u64 = 0;
    loop {
        let Some(line) = wire::read_line(reader, cfg.idle).await? else {
            if control.is_some() || !data.is_empty() {
                Log::new(Some(&ctx.status_tx)).warn(format!(
                    "LPD connection {id} closed with an incomplete job; discarded"
                ));
            }
            return Ok(());
        };
        let Some((&sub, rest)) = line.split_first() else {
            anyhow::bail!("Empty LPD subcommand");
        };
        if sub == wire::SUB_ABORT {
            data.clear();
            control = None;
            job_bytes = 0;
            continue;
        }
        anyhow::ensure!(
            sub == wire::SUB_CONTROL_FILE || sub == wire::SUB_DATA_FILE,
            "Unknown LPD receive-job subcommand {sub:#04x}"
        );
        let operands = wire::operands(rest);
        let count = operands.first().and_then(|c| c.parse::<u64>().ok());
        let name = operands.get(1).filter(|n| wire::valid_token(n)).cloned();
        let is_control = sub == wire::SUB_CONTROL_FILE;
        let admissible = match (count, &name) {
            (Some(count), Some(name)) if is_control => {
                name.starts_with("cf") && count <= wire::MAX_CONTROL_FILE && control.is_none()
            }
            (Some(count), Some(name)) => {
                name.starts_with("df")
                    && job_bytes.saturating_add(count) <= cfg.max_job
                    && data.len() < wire::MAX_FILES_PER_JOB
                    && !data.contains_key(name)
            }
            _ => false,
        };
        let (Some(count), Some(name), true) = (count, name, admissible) else {
            // Refused before a byte of the file is read; the client will not send it.
            write(ctx, id, out, &[1]).await?;
            return Ok(());
        };
        write(ctx, id, out, &[0]).await?;
        let body = wire::read_file(reader, count, cfg.idle).await?;
        ctx.state
            .update_connection_stats(ctx.server_id, id, Some(count + 1), None, Some(1), None)
            .await;
        if is_control {
            match wire::ControlFile::parse(&body) {
                Ok(parsed) => control = Some((name, parsed)),
                Err(e) => {
                    Log::new(Some(&ctx.status_tx))
                        .warn(format!("LPD connection {id} refused a control file: {e}"));
                    write(ctx, id, out, &[1]).await?;
                    return Ok(());
                }
            }
        } else {
            job_bytes += count;
            data.insert(name, body);
        }
        let complete = control
            .as_ref()
            .is_some_and(|(_, c)| c.data_files().iter().all(|n| data.contains_key(n)));
        if !complete {
            write(ctx, id, out, &[0]).await?;
            continue;
        }
        let (control_name, parsed) = control.take().expect("complete implies a control file");
        let accepted = decide_job(ctx, id, queue, &control_name, &parsed, &data).await;
        write(ctx, id, out, &[if accepted { 0 } else { 1 }]).await?;
        data.clear();
        job_bytes = 0;
    }
}

async fn decide_job(
    ctx: &SpawnContext,
    id: ConnectionId,
    queue: &str,
    control_name: &str,
    control: &wire::ControlFile,
    data: &HashMap<String, Vec<u8>>,
) -> bool {
    let (job_id, origin) = wire::job_number(control_name).unwrap_or_default();
    let files: Vec<Value> = control
        .prints
        .iter()
        .map(|(format, name, source)| {
            let bytes = data.get(name).map(Vec::as_slice).unwrap_or_default();
            let (text, _binary) = wire::preview(bytes);
            json!({
                "name": name,
                "source_name": source,
                "format": format.to_string(),
                "size": bytes.len(),
                "text": text,
            })
        })
        .collect();
    let event = Event::new(
        &actions::JOB_EVENT,
        json!({
            "queue": queue,
            "job_id": job_id,
            "host": control.host.clone().or(Some(origin).filter(|o| !o.is_empty())),
            "user": control.user,
            "job_name": control.job_name,
            "title": control.title,
            "class": control.class,
            "mail": control.mail,
            "files": files,
        }),
    );
    match decision(ctx, id, event, "lpd_job_reply").await {
        Ok(answer) if answer["accept"] == true => {
            outcome(ctx, id, "lpd_print_job", "model_answer");
            true
        }
        Ok(_) => {
            outcome(ctx, id, "lpd_print_job", "model_reject");
            false
        }
        Err(_) => false,
    }
}
