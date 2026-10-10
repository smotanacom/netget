//! Stratum V1 pool (server role). Rust owns each session — subscription, extranonce1, jobs,
//! the share arithmetic and every refusal the arithmetic decides — and asks the model only
//! who may mine and whether a share proven valid is credited.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use wire::{error, Job, Message};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(900);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
/// Jobs a connection keeps; shares for older ones are stale.
pub const MAX_JOBS: usize = 16;
/// Shares remembered per connection to refuse duplicates.
pub const MAX_SEEN_SHARES: usize = 4096;
/// Workers one connection may authorize.
pub const MAX_WORKERS: usize = 32;
/// nbits of the pool's own jobs: the easiest target there is, as on regtest.
pub const POOL_NBITS: u32 = 0x207f_ffff;

/// What every connection of one pool shares.
struct Pool {
    next_extranonce1: AtomicU32,
    next_job: AtomicU32,
    difficulty: f64,
}

struct Session {
    extranonce1: [u8; wire::EXTRANONCE1_SIZE],
    subscribed: bool,
    user_agent: Option<String>,
    workers: HashSet<String>,
    difficulty: f64,
    jobs: VecDeque<Job>,
    seen: HashSet<(String, String, String, String)>,
    height: u32,
}

fn params(ctx: &SpawnContext) -> Result<(f64, Duration)> {
    let p = ctx.startup_params.as_ref();
    let difficulty = p
        .map(|p| p.get_optional_f64("difficulty"))
        .transpose()?
        .flatten()
        .unwrap_or(actions::DEFAULT_DIFFICULTY);
    anyhow::ensure!(
        difficulty > 0.0 && difficulty <= actions::MAX_DIFFICULTY,
        "difficulty must be above 0 and at most {}",
        actions::MAX_DIFFICULTY
    );
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&idle),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok((difficulty, Duration::from_secs(idle)))
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let (difficulty, idle) = params(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Stratum pool listening on {addr}, difficulty {difficulty}"
    ));
    let pool = Arc::new(Pool {
        next_extranonce1: AtomicU32::new(rand::random()),
        next_job: AtomicU32::new(1),
        difficulty,
    });
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (socket, peer, permit) =
                match accept_bounded(&listener, &limiter, b"", "Stratum", Some(&ctx.status_tx))
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
            let pool = pool.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, &pool, id, socket, peer, idle).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("Stratum connection {id} ended: {e}"));
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

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("Stratum connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

fn reply(id: &Value, result: Value, err: Value) -> Value {
    json!({"id": id, "result": result, "error": err})
}

fn notify(method: &str, params: Value) -> Value {
    json!({"id": null, "method": method, "params": params})
}

fn now_secs() -> u32 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

impl Session {
    fn new_job(&mut self, pool: &Pool, message: Option<&str>, prev: [u8; 32], clean: bool) -> Job {
        self.height = self.height.wrapping_add(1).max(1);
        let job = wire::pool_job(
            format!("{:x}", pool.next_job.fetch_add(1, Ordering::Relaxed)),
            &prev,
            self.height,
            message.unwrap_or(actions::DEFAULT_COINBASE_MESSAGE),
            POOL_NBITS,
            now_secs(),
            clean,
        );
        if clean {
            self.jobs.clear();
            self.seen.clear();
        }
        if self.jobs.len() >= MAX_JOBS {
            self.jobs.pop_front();
        }
        self.jobs.push_back(job.clone());
        job
    }
}

/// The model's answer, split into its verdict and what accompanies it.
struct Answer {
    verdict: Option<Result<(), String>>,
    extras: Vec<Value>,
    invalid: bool,
}

fn split(results: Vec<ActionResult>) -> Answer {
    let mut verdicts = Vec::new();
    let mut extras = Vec::new();
    let mut stack = results;
    let mut ordered = Vec::new();
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { data, .. } => ordered.push(data),
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    ordered.reverse();
    for a in ordered {
        match a["type"].as_str().unwrap_or_default() {
            actions::ACCEPT => verdicts.push(Ok(())),
            actions::REJECT => {
                verdicts.push(Err(a["message"].as_str().unwrap_or("rejected").to_string()))
            }
            _ => extras.push(a),
        }
    }
    Answer {
        invalid: verdicts.len() > 1,
        verdict: verdicts.into_iter().next(),
        extras,
    }
}

/// Lines for the work, difficulty and messages accompanying an answer.
fn apply_extras(
    session: &mut Session,
    pool: &Pool,
    extras: &[Value],
    send_work: bool,
    out: &mut Vec<Value>,
) {
    let mut sent_job = false;
    for a in extras {
        match a["type"].as_str().unwrap_or_default() {
            actions::SET_DIFFICULTY => {
                if let Some(d) = a["difficulty"].as_f64() {
                    session.difficulty = d;
                    out.push(notify("mining.set_difficulty", json!([d])));
                }
            }
            actions::NEW_JOB => {
                let prev = wire::parse_prev_display(a["prev_hash"].as_str()).unwrap_or([0u8; 32]);
                let job = session.new_job(
                    pool,
                    a["message"].as_str(),
                    prev,
                    a["clean_jobs"].as_bool().unwrap_or(true),
                );
                out.push(notify("mining.notify", job.notify_params()));
                sent_job = true;
            }
            actions::SHOW_MESSAGE => {
                out.push(notify("client.show_message", json!([a["message"]])));
            }
            _ => {}
        }
    }
    if send_work && !sent_job {
        let job = match session.jobs.back() {
            Some(j) => j.clone(),
            None => session.new_job(pool, None, [0u8; 32], true),
        };
        if !out.iter().any(|m| m["method"] == "mining.set_difficulty") {
            out.insert(
                0,
                notify("mining.set_difficulty", json!([session.difficulty])),
            );
        }
        out.push(notify("mining.notify", job.notify_params()));
    }
}

async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Answer, anyhow::Error> {
    match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::StratumProtocol,
    )
    .await
    {
        Ok(r) if r.failures.is_empty() => {
            let answer = split(r.protocol_results);
            let decision = match (&answer.verdict, answer.invalid) {
                (_, true) => "fail_closed_invalid_reply",
                (Some(Ok(())), _) => "model_answer",
                (Some(Err(_)), _) => "model_reject",
                (None, _) => "model_silent",
            };
            outcome(ctx, id, operation, decision);
            Ok(answer)
        }
        Ok(_) => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Err(anyhow::anyhow!("invalid reply"))
        }
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            Err(e)
        }
    }
}

fn failure_text(e: &anyhow::Error) -> &'static str {
    crate::utils::wire_failure::wire_failure_text(e)
}

/// Lines answering one request.
#[allow(clippy::too_many_arguments)]
async fn answer(
    ctx: &SpawnContext,
    pool: &Pool,
    session: &mut Session,
    id: ConnectionId,
    peer: SocketAddr,
    rid: &Value,
    method: &str,
    params: &Value,
) -> Vec<Value> {
    let log = Log::new(Some(&ctx.status_tx));
    let p = params.as_array().cloned().unwrap_or_default();
    match method {
        "mining.subscribe" => {
            session.subscribed = true;
            session.user_agent = p
                .first()
                .and_then(Value::as_str)
                .map(|s| s.chars().take(128).collect());
            let sub = hex::encode(session.extranonce1);
            vec![reply(
                rid,
                json!([
                    [["mining.set_difficulty", sub], ["mining.notify", sub]],
                    hex::encode(session.extranonce1),
                    wire::EXTRANONCE2_SIZE
                ]),
                Value::Null,
            )]
        }
        "mining.configure" => vec![reply(rid, json!({}), Value::Null)],
        "mining.extranonce.subscribe" => vec![reply(rid, json!(false), Value::Null)],
        "mining.suggest_difficulty" => {
            log.debug(format!(
                "Stratum connection {id} suggested difficulty {:?} (ignored)",
                p.first()
            ));
            vec![reply(rid, json!(false), Value::Null)]
        }
        "mining.authorize" => {
            if !session.subscribed {
                return vec![reply(rid, json!(false), error(25, "Not subscribed"))];
            }
            let Some(worker) = p
                .first()
                .and_then(Value::as_str)
                .filter(|w| !w.is_empty() && w.len() <= 128)
            else {
                return vec![reply(rid, json!(false), error(20, "worker name required"))];
            };
            if session.workers.len() >= MAX_WORKERS && !session.workers.contains(worker) {
                return vec![reply(
                    rid,
                    json!(false),
                    error(20, "too many workers on one connection"),
                )];
            }
            let password = p.get(1).and_then(Value::as_str).unwrap_or_default();
            let event = Event::new(
                &actions::AUTHORIZE_EVENT,
                json!({"worker": worker, "password_given": !(password.is_empty() || password == "x"),
                       "user_agent": session.user_agent, "remote_addr": peer.to_string()}),
            );
            match ask(ctx, id, event, "authorize").await {
                Ok(a) if !a.invalid && matches!(a.verdict, Some(Ok(()))) => {
                    session.workers.insert(worker.to_string());
                    let mut out = vec![reply(rid, json!(true), Value::Null)];
                    apply_extras(session, pool, &a.extras, true, &mut out);
                    out
                }
                Ok(a) if !a.invalid => {
                    let why = match a.verdict {
                        Some(Err(why)) => why,
                        _ => crate::utils::WireFailure::Unavailable.text().to_string(),
                    };
                    let mut out = vec![reply(rid, json!(false), error(24, &why))];
                    apply_extras(session, pool, &a.extras, false, &mut out);
                    out
                }
                Ok(_) => vec![reply(
                    rid,
                    json!(false),
                    error(20, crate::utils::WireFailure::Unavailable.text()),
                )],
                Err(e) => vec![reply(rid, json!(false), error(20, failure_text(&e)))],
            }
        }
        "mining.submit" => submit(ctx, pool, session, id, peer, rid, &p).await,
        _ => vec![reply(rid, Value::Null, error(20, "Unknown method"))],
    }
}

async fn submit(
    ctx: &SpawnContext,
    pool: &Pool,
    session: &mut Session,
    id: ConnectionId,
    peer: SocketAddr,
    rid: &Value,
    p: &[Value],
) -> Vec<Value> {
    let refuse = |code: u32, why: &str| {
        Log::new(Some(&ctx.status_tx))
            .info(format!("Stratum connection {id} share refused: {why}"));
        vec![reply(rid, json!(false), error(code, why))]
    };
    if !session.subscribed {
        return refuse(25, "Not subscribed");
    }
    if p.len() != 5 {
        return refuse(
            20,
            "submit takes worker, job_id, extranonce2, ntime and nonce (no version rolling)",
        );
    }
    let s = |i: usize| p[i].as_str().unwrap_or_default();
    let (worker, job_id, en2_hex, ntime_hex, nonce_hex) = (s(0), s(1), s(2), s(3), s(4));
    if !session.workers.contains(worker) {
        return refuse(24, "Unauthorized worker");
    }
    let Some(job) = session.jobs.iter().find(|j| j.job_id == job_id).cloned() else {
        return refuse(21, "Job not found");
    };
    let Ok(en2) = hex::decode(en2_hex) else {
        return refuse(20, "extranonce2 is not hex");
    };
    if en2.len() != wire::EXTRANONCE2_SIZE {
        return refuse(20, "Invalid extranonce2 size");
    }
    let (Ok(ntime), Ok(nonce)) = (
        wire::parse_u32_hex(ntime_hex, "ntime"),
        wire::parse_u32_hex(nonce_hex, "nonce"),
    ) else {
        return refuse(20, "ntime and nonce must be 8 hex digits");
    };
    if ntime < job.ntime || ntime > job.ntime.saturating_add(wire::NTIME_ROLL) {
        return refuse(20, "Ntime out of range");
    }
    let key = (
        job_id.to_string(),
        en2_hex.to_lowercase(),
        ntime_hex.to_lowercase(),
        nonce_hex.to_lowercase(),
    );
    if session.seen.contains(&key) {
        return refuse(22, "Duplicate share");
    }
    let hash = job.share_hash(&session.extranonce1, &en2, ntime, nonce);
    let sdiff = wire::difficulty(&hash);
    let hash_hex = wire::display_hex(&hash);
    if sdiff < session.difficulty {
        return refuse(
            23,
            &format!(
                "Low difficulty share ({sdiff:.6} < {}): {hash_hex}",
                session.difficulty
            ),
        );
    }
    if session.seen.len() >= MAX_SEEN_SHARES {
        session.seen.clear();
    }
    session.seen.insert(key);
    let event = Event::new(
        &actions::SHARE_EVENT,
        json!({"worker": worker, "job_id": job_id, "hash": hash_hex, "share_difficulty": sdiff,
               "difficulty": session.difficulty, "remote_addr": peer.to_string()}),
    );
    match ask(ctx, id, event, "share").await {
        Ok(a) if !a.invalid && matches!(a.verdict, Some(Ok(()))) => {
            let mut out = vec![reply(rid, json!(true), Value::Null)];
            apply_extras(session, pool, &a.extras, false, &mut out);
            out
        }
        Ok(a) if !a.invalid => {
            let why = match a.verdict {
                Some(Err(why)) => why,
                _ => crate::utils::WireFailure::Unavailable.text().to_string(),
            };
            let mut out = vec![reply(rid, json!(false), error(20, &why))];
            apply_extras(session, pool, &a.extras, false, &mut out);
            out
        }
        Ok(_) => vec![reply(
            rid,
            json!(false),
            error(20, crate::utils::WireFailure::Unavailable.text()),
        )],
        Err(e) => vec![reply(rid, json!(false), error(20, failure_text(&e)))],
    }
}

async fn session(
    ctx: &SpawnContext,
    pool: &Pool,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    idle: Duration,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(socket);
    let mut lines = wire::Lines::new(reader);
    let mut session = Session {
        extranonce1: pool
            .next_extranonce1
            .fetch_add(1, Ordering::Relaxed)
            .to_be_bytes(),
        subscribed: false,
        user_agent: None,
        workers: HashSet::new(),
        difficulty: pool.difficulty,
        jobs: VecDeque::new(),
        seen: HashSet::new(),
        height: 0,
    };
    while let Some(value) = lines.next(idle).await? {
        ctx.state
            .update_connection_stats(ctx.server_id, id, None, None, Some(1), None)
            .await;
        let out = match wire::parse(value)? {
            Message::Request {
                id: rid,
                method,
                params,
            } => answer(ctx, pool, &mut session, id, peer, &rid, &method, &params).await,
            Message::Notification { method, .. } => {
                Log::new(Some(&ctx.status_tx)).debug(format!(
                    "Stratum connection {id} notification {method} ignored"
                ));
                continue;
            }
            Message::Response { .. } => continue,
        };
        let bytes: Vec<u8> = out.iter().flat_map(wire::line).collect();
        tokio::time::timeout(wire::IO_TIMEOUT, writer.write_all(&bytes))
            .await
            .context("Stratum write deadline")??;
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(out.len() as u64),
            )
            .await;
    }
    Ok(())
}
