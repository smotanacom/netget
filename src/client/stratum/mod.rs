//! Stratum V1 miner (client role): subscribes and authorizes on connect, tracks the pool's
//! work and difficulty, and mines and submits as the handler directs. Shares are built and
//! hashed with the same code the pool role verifies them with.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::stratum::wire::{self, Job, Message};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::StratumClientProtocol;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::{
    io::{AsyncWriteExt, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// Requests awaiting the pool's answer at once.
pub const MAX_PENDING: usize = 64;
/// Jobs remembered for hand submission.
pub const MAX_JOBS: usize = 8;
/// A pool may stay quiet this long between messages (new work comes every few minutes at most).
pub const READ_IDLE: Duration = Duration::from_secs(3600);

fn param(ctx: &ConnectContext, key: &str, default: &str) -> Result<String> {
    Ok(ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string(key))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| default.to_string()))
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let user = param(&ctx, "user", actions::DEFAULT_USER)?;
    let password = param(&ctx, "password", actions::DEFAULT_PASSWORD)?;
    let target = ctx
        .remote_addr
        .trim_start_matches("stratum+tcp://")
        .trim_end_matches('/')
        .to_string();
    let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(&target))
        .await
        .context("Stratum connect deadline")??;
    let local = stream.local_addr()?;
    let (reader, writer) = tokio::io::split(stream);
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (msg_tx, msg_rx) = mpsc::channel::<Result<Value>>(64);
    let reader_task = tokio::spawn(async move {
        let mut lines = wire::Lines::new(reader);
        loop {
            let item = lines.next(READ_IDLE).await;
            let end = !matches!(item, Ok(Some(_)));
            let sent = match item {
                Ok(Some(v)) => msg_tx.send(Ok(v)).await,
                Ok(None) => {
                    msg_tx
                        .send(Err(anyhow::anyhow!("pool closed the connection")))
                        .await
                }
                Err(e) => msg_tx.send(Err(e)).await,
            };
            if end || sent.is_err() {
                return;
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;

    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "Stratum",
                    None,
                    event.id(),
                    event.data.clone(),
                    vec![],
                )
                .await;
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
                &StratumClientProtocol,
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
                    .warn(format!("Stratum client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let mut miner = Miner {
            writer,
            next_id: 0,
            pending: HashMap::new(),
            extranonce1: Vec::new(),
            extranonce2_size: wire::EXTRANONCE2_SIZE,
            difficulty: 1.0,
            jobs: VecDeque::new(),
            extranonce2: 0,
            worker: user.clone(),
            events: event_tx,
        };
        let result = async {
            miner
                .request(
                    "mining.subscribe",
                    json!([concat!("NetGet/", env!("CARGO_PKG_VERSION"))]),
                    Pending::Subscribe,
                    None,
                )
                .await?;
            miner
                .request(
                    "mining.authorize",
                    json!([user, password]),
                    Pending::Authorize {
                        worker: user.clone(),
                    },
                    None,
                )
                .await?;
            session(&session_ctx, &mut miner, msg_rx, external, internal_rx).await
        }
        .await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Stratum client ended: {e:#}"));
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

enum Pending {
    Subscribe,
    Authorize { worker: String },
    Share { depth: usize, info: Value },
    Suggest,
}

struct Miner {
    writer: WriteHalf<TcpStream>,
    next_id: u64,
    pending: HashMap<u64, (Pending, Option<ClientCommand>)>,
    extranonce1: Vec<u8>,
    extranonce2_size: usize,
    difficulty: f64,
    jobs: VecDeque<Job>,
    extranonce2: u32,
    /// The worker shares are submitted as: the one authorized on connect.
    worker: String,
    events: mpsc::Sender<(Event, usize)>,
}

impl Miner {
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        what: Pending,
        caller: Option<ClientCommand>,
    ) -> Result<()> {
        if self.pending.len() >= MAX_PENDING {
            bail!("too many requests awaiting the pool");
        }
        self.next_id += 1;
        let line = wire::line(&json!({"id": self.next_id, "method": method, "params": params}));
        tokio::time::timeout(wire::IO_TIMEOUT, self.writer.write_all(&line))
            .await
            .context("Stratum write deadline")??;
        self.pending.insert(self.next_id, (what, caller));
        Ok(())
    }

    fn emit(
        &self,
        event: &'static crate::protocol::EventType,
        data: Value,
        depth: usize,
    ) -> Result<()> {
        self.events
            .try_send((Event::new(event, data), depth))
            .context("Stratum event queue full; consumer stalled")
    }

    fn extranonce2_bytes(&self, n: u32) -> Vec<u8> {
        let mut b = n.to_be_bytes().to_vec();
        b.resize(self.extranonce2_size, 0);
        b
    }

    /// Submit (job, extranonce2, ntime, nonce): the pool's answer arrives as an event.
    async fn submit(
        &mut self,
        job: &Job,
        en2: u32,
        ntime: u32,
        nonce: u32,
        depth: usize,
        caller: Option<ClientCommand>,
    ) -> Result<()> {
        let en2_bytes = self.extranonce2_bytes(en2);
        let hash = job.share_hash(&self.extranonce1, &en2_bytes, ntime, nonce);
        let info = json!({"job_id": job.job_id, "nonce": nonce, "extranonce2": en2,
                          "hash": wire::display_hex(&hash), "share_difficulty": wire::difficulty(&hash)});
        let params = json!([
            self.worker,
            job.job_id,
            hex::encode(&en2_bytes),
            wire::u32_hex(ntime),
            wire::u32_hex(nonce)
        ]);
        self.request(
            "mining.submit",
            params,
            Pending::Share { depth, info },
            caller,
        )
        .await
    }
}

/// Hash up to `max` nonces over one extranonce2: the first meeting `target`, else the best.
fn mine(
    job: &Job,
    extranonce1: &[u8],
    extranonce2: &[u8],
    target: f64,
    max: u64,
) -> (u32, f64, u64) {
    let root = job.merkle_root(extranonce1, extranonce2);
    let mut header = job.header(&root, job.ntime);
    let (mut best_nonce, mut best) = (0u32, 0f64);
    let mut tried = 0u64;
    for nonce in 0..=u32::MAX {
        if tried >= max {
            break;
        }
        tried += 1;
        header[76..80].copy_from_slice(&nonce.to_le_bytes());
        let d = wire::difficulty(&wire::sha256d(&header));
        if d > best {
            best = d;
            best_nonce = nonce;
            if d >= target {
                break;
            }
        }
    }
    (best_nonce, best, tried)
}

async fn session(
    ctx: &ConnectContext,
    miner: &mut Miner,
    mut messages: mpsc::Receiver<Result<Value>>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
) -> Result<()> {
    let log = Log::new(Some(&ctx.status_tx));
    loop {
        let (action, depth, mut injected) = tokio::select! {
            item = messages.recv() => {
                let Some(item) = item else { return Ok(()) };
                if let Some(answer) = on_message(miner, item?)? {
                    tokio::time::timeout(wire::IO_TIMEOUT, miner.writer.write_all(&wire::line(&answer)))
                        .await
                        .context("Stratum write deadline")??;
                }
                continue;
            }
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some((a, depth)) => (a, depth, None),
                None => return Ok(()),
            },
        };
        let refuse = |injected: &mut Option<ClientCommand>, error: String| {
            log.warn(format!("Stratum client action refused: {error}"));
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(
                    command,
                    Ok(ClientSendOutcome::Rejected { error }),
                );
            }
        };
        match StratumClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                refuse(&mut injected, e.to_string());
                continue;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "Stratum client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Stratum",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        match action["type"].as_str().unwrap_or_default() {
            actions::MINE => {
                let Some(job) = miner.jobs.back().cloned() else {
                    let data = json!({"hashes": 0, "best_difficulty": 0, "difficulty": miner.difficulty, "reason": "no job yet"});
                    if let Some(c) = injected.take() {
                        crate::client::command_support::reply(
                            c,
                            Ok(ClientSendOutcome::Executed {
                                detail: data.to_string(),
                            }),
                        );
                    }
                    miner.emit(&actions::MINED_EVENT, data, depth)?;
                    continue;
                };
                let max = action["max_hashes"]
                    .as_u64()
                    .unwrap_or(actions::DEFAULT_MINE_HASHES);
                let submit_best = action["submit_best"].as_bool().unwrap_or(false);
                miner.extranonce2 = miner.extranonce2.wrapping_add(1);
                let en2 = miner.extranonce2;
                let (en1, en2_bytes, target) = (
                    miner.extranonce1.clone(),
                    miner.extranonce2_bytes(en2),
                    miner.difficulty,
                );
                let mined = job.clone();
                let (nonce, best, tried) = tokio::task::spawn_blocking(move || {
                    mine(&mined, &en1, &en2_bytes, target, max)
                })
                .await
                .context("mining stopped")?;
                log.info(format!("Stratum client mined {tried} hashes on job {}: best difficulty {best:.6} (pool asks {target})", job.job_id));
                if best >= target || submit_best {
                    miner
                        .submit(&job, en2, job.ntime, nonce, depth, injected.take())
                        .await?;
                } else {
                    let data = json!({"hashes": tried, "best_difficulty": best, "difficulty": target, "reason": "nothing met the difficulty"});
                    if let Some(c) = injected.take() {
                        crate::client::command_support::reply(
                            c,
                            Ok(ClientSendOutcome::Executed {
                                detail: data.to_string(),
                            }),
                        );
                    }
                    miner.emit(&actions::MINED_EVENT, data, depth)?;
                }
            }
            actions::SUBMIT => {
                let job = match action["job_id"].as_str() {
                    Some(id) => miner.jobs.iter().find(|j| j.job_id == id).cloned(),
                    None => miner.jobs.back().cloned(),
                };
                let Some(job) = job else {
                    refuse(&mut injected, "no such job".into());
                    continue;
                };
                let field = |k: &str| action[k].as_u64().map(|n| n as u32);
                let ntime = field("ntime").unwrap_or(job.ntime);
                miner
                    .submit(
                        &job,
                        field("extranonce2").unwrap_or(0),
                        ntime,
                        field("nonce").unwrap_or(0),
                        depth,
                        injected.take(),
                    )
                    .await?;
            }
            actions::SUGGEST => {
                miner
                    .request(
                        "mining.suggest_difficulty",
                        json!([action["difficulty"]]),
                        Pending::Suggest,
                        injected.take(),
                    )
                    .await?;
            }
            actions::AUTHORIZE => {
                let user = action["user"].as_str().unwrap_or_default().to_string();
                let pass = action["password"]
                    .as_str()
                    .unwrap_or(actions::DEFAULT_PASSWORD)
                    .to_string();
                miner
                    .request(
                        "mining.authorize",
                        json!([user, pass]),
                        Pending::Authorize {
                            worker: user.clone(),
                        },
                        injected.take(),
                    )
                    .await?;
            }
            _ => {}
        }
    }
}

/// Track one message from the pool; what to answer it with, if it asked something.
fn on_message(miner: &mut Miner, value: Value) -> Result<Option<Value>> {
    match wire::parse(value)? {
        Message::Response { id, result, error } => {
            let Some((what, caller)) = id.as_u64().and_then(|id| miner.pending.remove(&id)) else {
                return Ok(None);
            };
            let ok = result.as_bool() == Some(true);
            let error = (!error.is_null()).then_some(error);
            let (event, data, depth) = match what {
                Pending::Subscribe => {
                    if let Some(e) = error {
                        bail!("the pool refused mining.subscribe: {e}");
                    }
                    let r = result
                        .as_array()
                        .context("mining.subscribe answered no array")?;
                    miner.extranonce1 =
                        hex::decode(r.get(1).and_then(Value::as_str).unwrap_or_default())
                            .context("extranonce1 is not hex")?;
                    miner.extranonce2_size =
                        r.get(2).and_then(Value::as_u64).unwrap_or(4).clamp(1, 16) as usize;
                    return Ok(None);
                }
                Pending::Authorize { worker: w } => (
                    &*actions::AUTHORIZED_EVENT,
                    json!({"worker": w, "ok": ok, "error": error}),
                    0,
                ),
                Pending::Share { depth, mut info } => {
                    info["accepted"] = json!(ok);
                    info["error"] = json!(error);
                    (&*actions::SHARE_RESULT_EVENT, info, depth)
                }
                Pending::Suggest => (
                    &*actions::MESSAGE_EVENT,
                    json!({"method": "mining.suggest_difficulty", "params": [result, error]}),
                    0,
                ),
            };
            if let Some(c) = caller {
                crate::client::command_support::reply(
                    c,
                    Ok(ClientSendOutcome::Executed {
                        detail: data.to_string(),
                    }),
                );
            }
            miner.emit(event, data, depth).map(|_| None)
        }
        Message::Notification { method, params } => match method.as_str() {
            "mining.set_difficulty" => {
                if let Some(d) = params.get(0).and_then(Value::as_f64).filter(|d| *d > 0.0) {
                    miner.difficulty = d;
                }
                Ok(None)
            }
            "mining.notify" => {
                let job = Job::from_notify(&params)?;
                if job.clean {
                    miner.jobs.clear();
                }
                if miner.jobs.len() >= MAX_JOBS {
                    miner.jobs.pop_front();
                }
                let data = json!({"job_id": job.job_id, "clean_jobs": job.clean, "prev_hash": job.prev_display(),
                                  "ntime": job.ntime, "nbits": wire::u32_hex(job.nbits),
                                  "merkle_branches": job.branches.len(), "difficulty": miner.difficulty});
                miner.jobs.push_back(job);
                miner.emit(&actions::JOB_EVENT, data, 0).map(|_| None)
            }
            "mining.set_extranonce" => {
                if let Some(en1) = params
                    .get(0)
                    .and_then(Value::as_str)
                    .and_then(|s| hex::decode(s).ok())
                {
                    miner.extranonce1 = en1;
                }
                if let Some(n) = params.get(1).and_then(Value::as_u64) {
                    miner.extranonce2_size = n.clamp(1, 16) as usize;
                }
                miner
                    .emit(
                        &actions::MESSAGE_EVENT,
                        json!({"method": method, "params": params}),
                        0,
                    )
                    .map(|_| None)
            }
            _ => miner
                .emit(
                    &actions::MESSAGE_EVENT,
                    json!({"method": method, "params": params}),
                    0,
                )
                .map(|_| None),
        },
        // The pool asking us something (client.get_version and the like) is answered by Rust.
        Message::Request { id, method, .. } => Ok(Some(if method == "client.get_version" {
            json!({"id": id, "result": concat!("NetGet/", env!("CARGO_PKG_VERSION")), "error": null})
        } else {
            json!({"id": id, "result": null, "error": wire::error(20, "Unknown method")})
        })),
    }
}
