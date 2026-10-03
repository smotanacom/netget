pub mod actions;
pub mod wire;
use crate::client::llm_budget::call_llm_for_client;
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::gearman::wire as gear;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::GearmanClientProtocol;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};
type HandlerEvent = (Event, u8);
type HandlerAction = (Value, u8);
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let role = wire::Role::parse(
        match &ctx.startup_params {
            Some(p) => p
                .get_optional_string("role")?
                .unwrap_or_else(|| wire::DEFAULT_ROLE.into()),
            None => wire::DEFAULT_ROLE.into(),
        }
        .as_str(),
    )?;
    let stream = tokio::time::timeout(wire::DEADLINE, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("Gearman connect deadline")??;
    let local = stream.local_addr()?;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (mut reader, writer) = tokio::io::split(stream);
    let (frame_tx, frame_rx) = mpsc::channel::<Result<Option<wire::Frame>>>(16);
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = wire::frame(&mut reader).await;
            let done = !matches!(&frame, Ok(Some(_)));
            if frame_tx.send(frame).await.is_err() || done {
                break;
            }
        }
    });
    let reader_abort = reader_task.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;
    let (action_tx, action_rx) = mpsc::channel::<HandlerAction>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<HandlerEvent>(16);
    let handler_ctx = ctx.clone();
    let handler = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            let instruction = handler_ctx
                .state
                .get_instruction_for_client(handler_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = handler_ctx
                .state
                .get_memory_for_client(handler_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &handler_ctx.llm_client,
                &handler_ctx.state,
                handler_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &GearmanClientProtocol,
                &handler_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        handler_ctx
                            .state
                            .set_memory_for_client(handler_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if depth >= wire::MAX_FOLLOWUPS && action["type"] != "disconnect" {
                            Log::new(Some(&handler_ctx.status_tx))
                                .warn("Gearman handler followup limit reached");
                            continue;
                        }
                        if action_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&handler_ctx.status_tx)).warn(format!("Gearman handler: {e}"))
                }
            }
        }
    });
    ctx.state.register_client_task(ctx.client_id, handler).await;
    let session_ctx = ctx.clone();
    let session_task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            role,
            writer,
            external,
            action_rx,
            frame_rx,
            event_tx,
        )
        .await;
        reader_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Gearman ended: {e}"));
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
    ctx.state
        .register_client_task(ctx.client_id, session_task)
        .await;
    Ok(local)
}
struct Pending {
    request: wire::Request,
    action: Value,
    depth: u8,
    deadline: tokio::time::Instant,
}
struct Job {
    action: Value,
    depth: u8,
}
struct Flow {
    role: wire::Role,
    pending: Option<Pending>,
    jobs: HashMap<String, Job>,
    abilities: HashSet<String>,
    sleeping: bool,
    may_sleep: bool,
    exceptions: bool,
}
impl Flow {
    fn check(&self, r: &wire::Request) -> Result<()> {
        anyhow::ensure!(
            !r.expects_reply() || self.pending.is_none(),
            "Gearman request pending; retry after its response"
        );
        if r.worker_only() {
            anyhow::ensure!(
                self.role == wire::Role::Worker,
                "Gearman worker action requires worker role"
            );
        } else if r.packet_type != gear::ECHO_REQ {
            anyhow::ensure!(
                self.role == wire::Role::Submitter,
                "Gearman submitter action requires submitter role"
            );
        }
        if r.submit() && !r.background() {
            anyhow::ensure!(
                self.jobs.len() < wire::MAX_JOBS,
                "Gearman foreground job limit reached"
            );
            if !r.args[1].is_empty() {
                anyhow::ensure!(
                    !self.jobs.values().any(|j| j.action["function_name"]
                        .as_str()
                        .is_some_and(|f| f.as_bytes() == r.args[0])
                        && j.action["unique_id"]
                            .as_str()
                            .is_some_and(|u| u.as_bytes() == r.args[1])),
                    "Gearman duplicate active unique job would make correlation ambiguous"
                );
            }
        }
        match r.packet_type {
            gear::CAN_DO => {
                let function = std::str::from_utf8(&r.args[0])?;
                anyhow::ensure!(
                    self.abilities.contains(function) || self.abilities.len() < wire::MAX_ABILITIES,
                    "Gearman ability limit reached"
                );
            }
            gear::GRAB_JOB | gear::GRAB_JOB_UNIQ => {
                anyhow::ensure!(!self.sleeping, "Wait for NOOP before grabbing from sleep");
                anyhow::ensure!(
                    !self.abilities.is_empty(),
                    "Register an ability before grabbing"
                );
                anyhow::ensure!(
                    self.jobs.len() < wire::MAX_JOBS,
                    "Gearman assigned job limit reached"
                );
            }
            gear::PRE_SLEEP => anyhow::ensure!(
                self.may_sleep && !self.sleeping && self.pending.is_none(),
                "Gearman sleep requires a completed no_job response"
            ),
            gear::WORK_STATUS
            | gear::WORK_DATA
            | gear::WORK_WARNING
            | gear::WORK_COMPLETE
            | gear::WORK_FAIL
            | gear::WORK_EXCEPTION => {
                let handle = std::str::from_utf8(&r.args[0])?;
                anyhow::ensure!(
                    self.jobs.contains_key(handle),
                    "Gearman work reply requires a handle assigned to this connection"
                );
            }
            _ => {}
        }
        Ok(())
    }
    fn sent(&mut self, r: &wire::Request) -> Result<()> {
        match r.packet_type {
            gear::CAN_DO => {
                self.abilities
                    .insert(std::str::from_utf8(&r.args[0])?.into());
            }
            gear::CANT_DO => {
                self.abilities.remove(std::str::from_utf8(&r.args[0])?);
            }
            gear::RESET_ABILITIES => self.abilities.clear(),
            gear::GRAB_JOB | gear::GRAB_JOB_UNIQ => {
                self.sleeping = false;
                self.may_sleep = false;
            }
            gear::PRE_SLEEP => {
                self.sleeping = true;
                self.may_sleep = false;
            }
            gear::WORK_COMPLETE | gear::WORK_FAIL | gear::WORK_EXCEPTION => {
                self.jobs.remove(std::str::from_utf8(&r.args[0])?);
            }
            _ => {}
        }
        Ok(())
    }
    fn incoming(&mut self, f: wire::Frame) -> Result<(Event, u8)> {
        let t = f.packet_type;
        if t == gear::ERROR {
            let a = wire::args(&f.data, 2)?;
            let code = std::str::from_utf8(a[0]).context("Gearman error code is not UTF-8")?;
            anyhow::ensure!(
                !code.is_empty()
                    && code.len() <= 64
                    && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "Invalid Gearman error code"
            );
            let description =
                std::str::from_utf8(a[1]).context("Gearman error text is not UTF-8")?;
            let p = self.pending.take();
            return Ok((
                Event::new(
                    &actions::ERROR_EVENT,
                    json!({"code":code,"description":description,"request":p.as_ref().map(|p|&p.action)}),
                ),
                p.map(|p| p.depth).unwrap_or(0),
            ));
        }
        if t == wire::NOOP {
            anyhow::ensure!(
                self.role == wire::Role::Worker && self.sleeping && f.data.is_empty(),
                "Unexpected Gearman NOOP"
            );
            self.sleeping = false;
            return Ok((Event::new(&actions::WAKE_EVENT, json!({})), 0));
        }
        if matches!(
            t,
            gear::WORK_STATUS
                | gear::WORK_DATA
                | gear::WORK_WARNING
                | gear::WORK_COMPLETE
                | gear::WORK_FAIL
                | gear::WORK_EXCEPTION
        ) {
            anyhow::ensure!(
                self.role == wire::Role::Submitter,
                "Worker received a submitter job update"
            );
            let a = wire::args(
                &f.data,
                match t {
                    gear::WORK_STATUS => 3,
                    gear::WORK_FAIL => 1,
                    _ => 2,
                },
            )?;
            let handle = wire::handle(a[0])?;
            let job = self
                .jobs
                .get(&handle)
                .context("Gearman update names an unknown foreground handle")?;
            let terminal = matches!(
                t,
                gear::WORK_COMPLETE | gear::WORK_FAIL | gear::WORK_EXCEPTION
            );
            let kind = match t {
                gear::WORK_STATUS => "progress",
                gear::WORK_DATA => "data",
                gear::WORK_WARNING => "warning",
                gear::WORK_COMPLETE => "complete",
                gear::WORK_FAIL => "fail",
                _ => "exception",
            };
            let mut data =
                json!({"request":job.action,"job_handle":handle,"kind":kind,"terminal":terminal});
            if t == gear::WORK_STATUS {
                let n = wire::number(a[1])?;
                let d = wire::number(a[2])?;
                anyhow::ensure!(n <= d, "Gearman progress numerator exceeds denominator");
                data["numerator"] = json!(n);
                data["denominator"] = json!(d);
            } else if t != gear::WORK_FAIL {
                data["payload"] = wire::body(a[1]);
            }
            if t == gear::WORK_EXCEPTION {
                anyhow::ensure!(self.exceptions, "Gearman exception option was not enabled");
            }
            let depth = job.depth;
            if terminal {
                self.jobs.remove(&handle);
            }
            return Ok((Event::new(&actions::UPDATE_EVENT, data), depth));
        }
        let p = self
            .pending
            .take()
            .context("Unsolicited Gearman response")?;
        let response = match t {
            gear::JOB_CREATED => {
                anyhow::ensure!(
                    p.request.submit(),
                    "JOB_CREATED does not match pending request"
                );
                let handle = wire::handle(&f.data)?;
                if !p.request.background() {
                    anyhow::ensure!(
                        !self.jobs.contains_key(&handle),
                        "Duplicate Gearman foreground handle"
                    );
                    self.jobs.insert(
                        handle.clone(),
                        Job {
                            action: p.action.clone(),
                            depth: p.depth,
                        },
                    );
                }
                json!({"kind":"job_created","job_handle":handle,"background":p.request.background()})
            }
            gear::STATUS_RES => {
                anyhow::ensure!(
                    p.request.packet_type == gear::GET_STATUS,
                    "STATUS_RES does not match pending request"
                );
                let a = wire::args(&f.data, 5)?;
                let handle = wire::handle(a[0])?;
                anyhow::ensure!(
                    a[0] == p.request.args[0],
                    "Gearman status handle does not match request"
                );
                anyhow::ensure!(
                    matches!(a[1], b"0" | b"1") && matches!(a[2], b"0" | b"1"),
                    "Invalid Gearman status flags"
                );
                let n = wire::number(a[3])?;
                let d = wire::number(a[4])?;
                anyhow::ensure!(
                    n <= d && !(a[1] == b"0" && a[2] == b"1"),
                    "Inconsistent Gearman status"
                );
                json!({"kind":"status","job_handle":handle,"known":a[1]==b"1","running":a[2]==b"1","numerator":n,"denominator":d})
            }
            gear::ECHO_RES => {
                anyhow::ensure!(
                    p.request.packet_type == gear::ECHO_REQ && f.data == p.request.args[0],
                    "Gearman echo does not match request"
                );
                json!({"kind":"echo","payload":wire::body(&f.data)})
            }
            gear::OPTION_RES => {
                anyhow::ensure!(
                    p.request.packet_type == gear::OPTION_REQ && f.data == b"exceptions",
                    "Gearman option response does not match request"
                );
                self.exceptions = true;
                json!({"kind":"option","option":"exceptions"})
            }
            wire::NO_JOB => {
                anyhow::ensure!(
                    matches!(p.request.packet_type, gear::GRAB_JOB | gear::GRAB_JOB_UNIQ)
                        && f.data.is_empty(),
                    "Gearman no_job does not match grab"
                );
                self.may_sleep = true;
                json!({"kind":"no_job"})
            }
            wire::JOB_ASSIGN | wire::JOB_ASSIGN_UNIQ => {
                let uniq = t == wire::JOB_ASSIGN_UNIQ;
                anyhow::ensure!(
                    p.request.packet_type
                        == if uniq {
                            gear::GRAB_JOB_UNIQ
                        } else {
                            gear::GRAB_JOB
                        },
                    "Gearman assignment type does not match grab"
                );
                let a = wire::args(&f.data, if uniq { 4 } else { 3 })?;
                let handle = wire::handle(a[0])?;
                let function =
                    std::str::from_utf8(a[1]).context("Gearman assigned function is not UTF-8")?;
                anyhow::ensure!(
                    self.abilities.contains(function),
                    "Gearman assignment names an unregistered ability"
                );
                let unique = if uniq {
                    let u = std::str::from_utf8(a[2])?;
                    anyhow::ensure!(
                        u.len() <= gear::MAX_UNIQUE,
                        "Assigned unique ID exceeds limit"
                    );
                    Some(u)
                } else {
                    None
                };
                anyhow::ensure!(
                    !self.jobs.contains_key(&handle) && self.jobs.len() < wire::MAX_JOBS,
                    "Duplicate or excessive Gearman assignment"
                );
                self.jobs.insert(
                    handle.clone(),
                    Job {
                        action: p.action.clone(),
                        depth: 0,
                    },
                );
                json!({"kind":"job_assigned","job_handle":handle,"function":function,"unique_id":unique,"workload":wire::body(a[if uniq{3}else{2}])})
            }
            _ => bail!("Unsupported Gearman response type {t}"),
        };
        Ok((
            Event::new(
                &actions::RESPONSE_EVENT,
                json!({"request":p.action,"response":response}),
            ),
            if matches!(t, wire::JOB_ASSIGN | wire::JOB_ASSIGN_UNIQ) {
                0
            } else {
                p.depth
            },
        ))
    }
}
fn emit(tx: &mpsc::Sender<HandlerEvent>, event: HandlerEvent) -> Result<()> {
    tx.try_send(event)
        .context("Gearman event queue full; handler stalled")
}
/// Whole-write deadline with responsive disconnect even if the peer stops reading.
pub async fn write_interruptible<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    external: &mut mpsc::Receiver<ClientCommand>,
) -> Result<bool> {
    let (complete, disconnect) = {
        let write = tokio::time::timeout(wire::DEADLINE, writer.write_all(bytes));
        tokio::pin!(write);
        loop {
            tokio::select! {biased;
                result=&mut write=>{result.context("Gearman write deadline")??;break(true,None);},
                command=external.recv()=>{let Some(command)=command else{break(false,None)};if command.action["type"]=="disconnect"{break(false,Some(command));}crate::client::command_support::reply(command,Ok(ClientSendOutcome::Rejected{error:"Gearman write pending; retry after it completes".into()}));}
            }
        }
    };
    if !complete {
        writer.shutdown().await?;
        if let Some(c) = disconnect {
            crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
        }
    }
    Ok(complete)
}
async fn session(
    ctx: &ConnectContext,
    role: wire::Role,
    mut writer: WriteHalf<TcpStream>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<HandlerAction>,
    mut frames: mpsc::Receiver<Result<Option<wire::Frame>>>,
    events: mpsc::Sender<HandlerEvent>,
) -> Result<()> {
    let mut flow = Flow {
        role,
        pending: None,
        jobs: HashMap::new(),
        abilities: HashSet::new(),
        sleeping: false,
        may_sleep: false,
        exceptions: false,
    };
    emit(
        &events,
        (
            Event::new(
                &actions::CONNECTED_EVENT,
                json!({"remote_addr":ctx.remote_addr,"role":role.name()}),
            ),
            0,
        ),
    )?;
    loop {
        let deadline = flow
            .pending
            .as_ref()
            .map(|p| p.deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(86400));
        let (action, depth, command) = tokio::select! {
            frame=frames.recv()=>{
                let Some(frame)=frame else{return Ok(())};let Some(frame)=frame? else{return Ok(())};
                let refusal=frame.packet_type==gear::ERROR;
                emit(&events,flow.incoming(frame)?)?;
                if refusal {bail!("Gearman peer refused a command; see gearman_error");}
                continue;
            },
            command=external.recv()=>match command{Some(c)=>(c.action.clone(),0,Some(c)),None=>return Ok(())},
            action=internal.recv(),if flow.pending.is_none()=>match action{Some((a,d))=>(a,d,None),None=>return Ok(())},
            _=tokio::time::sleep_until(deadline),if flow.pending.is_some()=>bail!("Gearman request reply deadline"),
        };
        if action["type"] == "disconnect" {
            writer.shutdown().await?;
            if let Some(c) = command {
                crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
            }
            return Ok(());
        }
        let request = wire::request(&action).and_then(|r| {
            flow.check(&r)?;
            Ok(r)
        });
        let request = match request {
            Ok(r) => r,
            Err(e) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                } else {
                    Log::new(Some(&ctx.status_tx)).warn(format!("Gearman action rejected: {e}"));
                }
                continue;
            }
        };
        let bytes = request.bytes();
        let result = write_interruptible(&mut writer, &bytes, &mut external).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Gearman",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![json!({"sent":matches!(&result,Ok(true))})],
                )
                .await;
        }
        if let Some(c) = command {
            crate::client::command_support::reply(
                c,
                match &result {
                    Ok(true) => Ok(ClientSendOutcome::Sent {
                        bytes_sent: bytes.len(),
                    }),
                    Ok(false) => Ok(ClientSendOutcome::Rejected {
                        error: "Gearman write cancelled by disconnect".into(),
                    }),
                    Err(e) => Err(anyhow::anyhow!(e.to_string())),
                },
            );
        }
        if !result? {
            return Ok(());
        }
        flow.sent(&request)?;
        if request.expects_reply() {
            flow.pending = Some(Pending {
                request,
                action,
                depth,
                deadline: tokio::time::Instant::now() + wire::DEADLINE,
            });
        }
    }
}
