//! TR-069 device (client role). A session is a run of HTTP POSTs to the ACS: Inform, then
//! whatever the ACS answers — an RPC the handler answers in the next POST, or 204 to end it.
//! A connection-request listener lets the ACS ask for a session.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::tr069::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::Tr069ClientProtocol;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{server::conn::http1, service::service_fn, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// How long an RPC waits for the handler's answer before it is refused with fault 9002.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
/// RPCs one session may carry; past it the session is abandoned.
pub const MAX_SESSION_RPCS: usize = 256;
/// Sessions waiting to start (informs asked for, connection requests) at most.
pub const MAX_PENDING_SESSIONS: usize = 16;
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

struct Device {
    id: wire::DeviceId,
    root: String,
    cr_url: String,
}

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
    let acs = if ctx.remote_addr.starts_with("http://") || ctx.remote_addr.starts_with("https://") {
        ctx.remote_addr.clone()
    } else {
        format!("http://{}/", ctx.remote_addr)
    };
    reqwest::Url::parse(&acs).context("remote_addr must be the ACS URL")?;
    let events: Vec<String> = match ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_array("events"))
        .transpose()?
        .flatten()
    {
        Some(list) => list
            .iter()
            .map(|v| v.as_str().map(str::to_string).context("events are strings"))
            .collect::<Result<_>>()?,
        None => actions::DEFAULT_EVENTS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let root = param(&ctx, "root", actions::DEFAULT_ROOT)?;
    anyhow::ensure!(
        matches!(root.as_str(), "Device" | "InternetGatewayDevice"),
        "root is Device or InternetGatewayDevice"
    );
    let listen = param(
        &ctx,
        "connection_request_listen",
        actions::DEFAULT_CR_LISTEN,
    )?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("cannot listen for connection requests on {listen}"))?;
    let cr_addr = listener.local_addr()?;
    let device = Device {
        id: wire::DeviceId {
            manufacturer: param(&ctx, "manufacturer", actions::DEFAULT_MANUFACTURER)?,
            oui: param(&ctx, "oui", actions::DEFAULT_OUI)?,
            product_class: param(&ctx, "product_class", actions::DEFAULT_PRODUCT_CLASS)?,
            serial_number: param(&ctx, "serial_number", actions::DEFAULT_SERIAL)?,
        },
        root,
        cr_url: format!("http://{cr_addr}/"),
    };

    // Connection requests: any GET asks for a session; the device opens it itself.
    let (cr_tx, cr_rx) = mpsc::channel::<()>(4);
    let cr_state = ctx.state.clone();
    let client_id = ctx.client_id;
    let cr_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = cr_tx.clone();
            let conn = async move {
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let tx = tx.clone();
                    async move {
                        let ok = req.method() == hyper::Method::GET;
                        if ok {
                            let _ = tx.try_send(());
                        }
                        let mut r = Response::new(Full::new(Bytes::new()));
                        if !ok {
                            *r.status_mut() = hyper::StatusCode::METHOD_NOT_ALLOWED;
                        }
                        Ok::<_, Infallible>(r)
                    }
                });
                let _ = tokio::time::timeout(
                    HTTP_TIMEOUT,
                    http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HTTP_TIMEOUT)
                        .serve_connection(TokioIo::new(stream), service),
                )
                .await;
            };
            cr_state
                .register_client_task(client_id, tokio::spawn(conn))
                .await;
        }
    });
    ctx.state.register_client_task(ctx.client_id, cr_task).await;

    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "TR-069",
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
                &Tr069ClientProtocol,
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
                    .warn(format!("TR-069 client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let mut cpe = Cpe {
            http: crate::llm::ollama_client::client_for_endpoint_with_timeout(&acs, HTTP_TIMEOUT),
            acs,
            device,
            events: event_tx,
            external,
            internal: internal_rx,
            cr: cr_rx,
            pending: VecDeque::from([(events, Vec::new(), 0usize, None)]),
        };
        let result = cpe.run(&session_ctx).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("TR-069 client ended: {e:#}"));
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
    Ok(cr_addr)
}

/// A session to open: its events, extra inform parameters, chain depth and who asked.
type PendingSession = (
    Vec<String>,
    Vec<(String, String, String)>,
    usize,
    Option<ClientCommand>,
);

struct Cpe {
    http: reqwest::Client,
    acs: String,
    device: Device,
    events: mpsc::Sender<(Event, usize)>,
    external: mpsc::Receiver<ClientCommand>,
    internal: mpsc::Receiver<(Value, usize)>,
    cr: mpsc::Receiver<()>,
    pending: VecDeque<PendingSession>,
}

fn reply(caller: Option<ClientCommand>, outcome: ClientSendOutcome) {
    if let Some(c) = caller {
        crate::client::command_support::reply(c, Ok(outcome));
    }
}

/// What came back from one POST: nothing (the session is over), or a message.
enum Answer {
    End,
    Message(wire::Message),
}

impl Cpe {
    fn emit(
        &self,
        event: &'static crate::protocol::EventType,
        data: Value,
        depth: usize,
    ) -> Result<()> {
        self.events
            .try_send((Event::new(event, data), depth))
            .context("TR-069 event queue full; consumer stalled")
    }

    async fn post(&self, body: String, cookies: &mut BTreeMap<String, String>) -> Result<Answer> {
        let mut req = self
            .http
            .post(&self.acs)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", "");
        if !cookies.is_empty() {
            req = req.header(
                "Cookie",
                cookies
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        let resp = req
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {}", self.acs))?;
        for c in resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
        {
            if let Some((k, v)) = c.split(';').next().and_then(|kv| kv.split_once('=')) {
                cookies.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let status = resp.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Answer::End);
        }
        let len = resp.content_length().unwrap_or(0) as usize;
        if len > wire::MAX_ENVELOPE {
            bail!(
                "the ACS answered with more than {} bytes",
                wire::MAX_ENVELOPE
            );
        }
        let body = resp.bytes().await?;
        if body.len() > wire::MAX_ENVELOPE {
            bail!(
                "the ACS answered with more than {} bytes",
                wire::MAX_ENVELOPE
            );
        }
        if !status.is_success() {
            bail!("the ACS answered HTTP {status}");
        }
        if body.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Answer::End);
        }
        Ok(Answer::Message(wire::parse(&body)?))
    }

    /// Handle an action that is not an answer to an RPC; true to stop the client.
    fn take(
        &mut self,
        ctx: &ConnectContext,
        action: Value,
        depth: usize,
        caller: Option<ClientCommand>,
    ) -> bool {
        let log = Log::new(Some(&ctx.status_tx));
        match Tr069ClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(caller, ClientSendOutcome::Disconnected);
                return true;
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("TR-069 client action refused: {e}"));
                reply(
                    caller,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                return false;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "TR-069 client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            return false;
        }
        if action["type"] != actions::INFORM {
            reply(
                caller,
                ClientSendOutcome::Rejected {
                    error: "no RPC from the ACS is waiting for an answer".into(),
                },
            );
            return false;
        }
        if self.pending.len() >= MAX_PENDING_SESSIONS {
            reply(
                caller,
                ClientSendOutcome::Rejected {
                    error: "too many sessions waiting".into(),
                },
            );
            return false;
        }
        let events = action["events"]
            .as_array()
            .map(|e| {
                e.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let extra = action
            .get("parameters")
            .filter(|p| !p.is_null())
            .map(|p| wire::triples(p, None).unwrap_or_default())
            .unwrap_or_default();
        self.pending.push_back((events, extra, depth, caller));
        false
    }

    async fn run(&mut self, ctx: &ConnectContext) -> Result<()> {
        loop {
            if let Some((events, extra, depth, caller)) = self.pending.pop_front() {
                if self.session(ctx, events, extra, depth, caller).await? {
                    return Ok(());
                }
                continue;
            }
            tokio::select! {
                cr = self.cr.recv() => {
                    if cr.is_some() {
                        Log::new(Some(&ctx.status_tx)).info("TR-069 client: connection request from the ACS".to_string());
                        self.pending.push_back((vec!["6 CONNECTION REQUEST".into()], Vec::new(), 0, None));
                    }
                }
                command = self.external.recv() => match command {
                    Some(c) => {
                        let action = c.action.clone();
                        ctx.state.record_access_log(AccessLogOwner::Client(ctx.client_id.as_u32()), "TR-069", None, "injected_action", action.clone(), vec![]).await;
                        if self.take(ctx, action, 0, Some(c)) {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                },
                action = self.internal.recv() => match action {
                    Some((a, depth)) => if self.take(ctx, a, depth, None) { return Ok(()) },
                    None => return Ok(()),
                },
            }
        }
    }

    /// One session; true when the client was told to stop during it.
    async fn session(
        &mut self,
        ctx: &ConnectContext,
        events: Vec<String>,
        extra: Vec<(String, String, String)>,
        depth: usize,
        caller: Option<ClientCommand>,
    ) -> Result<bool> {
        let log = Log::new(Some(&ctx.status_tx));
        let mut cookies = BTreeMap::new();
        let mut params = vec![(
            format!("{}.ManagementServer.ConnectionRequestURL", self.device.root),
            self.device.cr_url.clone(),
            "xsd:string".to_string(),
        )];
        params.extend(extra);
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let ev: Vec<(String, String)> = events.iter().map(|e| (e.clone(), String::new())).collect();
        let inform = wire::envelope("1", &wire::inform(&self.device.id, &ev, &params, 0, &now));
        let end = |me: &Self,
                   ok: bool,
                   rpcs: usize,
                   error: Option<String>,
                   caller: Option<ClientCommand>|
         -> Result<()> {
            let data = json!({"ok": ok, "events": events, "rpcs": rpcs, "error": error});
            reply(
                caller,
                ClientSendOutcome::Executed {
                    detail: data.to_string(),
                },
            );
            me.emit(&actions::SESSION_EVENT, data, depth)
        };
        match self.post(inform, &mut cookies).await {
            Ok(Answer::Message(m)) if m.method == "InformResponse" => {}
            Ok(Answer::Message(m)) if m.method == "Fault" => {
                end(
                    self,
                    false,
                    0,
                    Some(format!("the ACS refused the Inform: {}", m.content)),
                    caller,
                )?;
                return Ok(false);
            }
            Ok(_) => {
                end(
                    self,
                    false,
                    0,
                    Some("the ACS did not answer the Inform with InformResponse".into()),
                    caller,
                )?;
                return Ok(false);
            }
            Err(e) => {
                end(self, false, 0, Some(format!("{e:#}")), caller)?;
                return Ok(false);
            }
        }
        let mut outgoing = String::new();
        let mut rpcs = 0;
        loop {
            let m = match self.post(std::mem::take(&mut outgoing), &mut cookies).await {
                Ok(Answer::End) => break,
                Ok(Answer::Message(m)) => m,
                Err(e) => {
                    end(self, false, rpcs, Some(format!("{e:#}")), caller)?;
                    return Ok(false);
                }
            };
            rpcs += 1;
            if rpcs > MAX_SESSION_RPCS {
                log.warn(format!(
                    "TR-069 client: more than {MAX_SESSION_RPCS} RPCs in one session; abandoned"
                ));
                break;
            }
            let id = m.id.clone().unwrap_or_default();
            if m.method == "GetRPCMethods" {
                outgoing = wire::envelope(
                    &id,
                    &wire::get_rpc_methods_response(&[
                        "GetRPCMethods",
                        "GetParameterValues",
                        "SetParameterValues",
                        "GetParameterNames",
                        "AddObject",
                        "DeleteObject",
                        "Reboot",
                        "FactoryReset",
                    ]),
                );
                continue;
            }
            self.emit(
                &actions::RPC_EVENT,
                json!({"method": m.method, "arguments": m.content}),
                depth,
            )?;
            let (body, stop) = self.answer(ctx, &m.method).await;
            if stop {
                return Ok(true);
            }
            outgoing = wire::envelope(&id, &body);
        }
        end(self, true, rpcs, None, caller)?;
        Ok(false)
    }

    /// Wait for the handler's answer to an RPC; actions that are not answers wait their turn.
    async fn answer(&mut self, ctx: &ConnectContext, method: &str) -> (String, bool) {
        let log = Log::new(Some(&ctx.status_tx));
        let deadline = tokio::time::Instant::now() + ANSWER_TIMEOUT;
        loop {
            let (action, depth, caller) = tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    log.warn(format!("TR-069 client: no answer to {method}; refused with fault 9002"));
                    return (wire::fault(9002, "Internal error"), false);
                }
                command = self.external.recv() => match command {
                    Some(c) => (c.action.clone(), 0, Some(c)),
                    None => return (wire::fault(9002, "Internal error"), true),
                },
                action = self.internal.recv() => match action {
                    Some((a, d)) => (a, d, None),
                    None => return (wire::fault(9002, "Internal error"), true),
                },
                cr = self.cr.recv() => {
                    // Already in a session: a connection request now is satisfied by it.
                    let _ = cr;
                    continue;
                }
            };
            let kind = action["type"].as_str().unwrap_or_default().to_string();
            if !matches!(
                kind.as_str(),
                actions::VALUES | actions::NAMES | actions::DONE | actions::FAULT
            ) {
                if self.take(ctx, action, depth, caller) {
                    return (wire::fault(9002, "Internal error"), true);
                }
                continue;
            }
            if let Err(e) = actions::check(&action) {
                log.warn(format!("TR-069 client answer refused: {e}"));
                reply(
                    caller,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                continue;
            }
            let body = match (kind.as_str(), method) {
                (actions::FAULT, _) => wire::fault(
                    action["code"].as_u64().unwrap_or(9002) as u32,
                    action["message"].as_str().unwrap_or_default(),
                ),
                (actions::VALUES, "GetParameterValues") => wire::get_parameter_values_response(
                    &wire::triples(
                        &action["parameters"],
                        action.get("types").filter(|t| !t.is_null()),
                    )
                    .unwrap_or_default(),
                ),
                (actions::NAMES, "GetParameterNames") => {
                    let names: Vec<(String, bool)> = action["parameters"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|p| {
                                    (
                                        p["name"].as_str().unwrap_or_default().to_string(),
                                        p["writable"].as_bool().unwrap_or(false),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    wire::get_parameter_names_response(&names)
                }
                (actions::DONE, m) if !matches!(m, "GetParameterValues" | "GetParameterNames") => {
                    wire::status_response(
                        m,
                        action["status"].as_u64().unwrap_or(0) as u32,
                        action["instance_number"].as_u64(),
                    )
                }
                (k, m) => {
                    let error = format!("{k} does not answer {m}");
                    log.warn(format!("TR-069 client: {error}"));
                    reply(caller, ClientSendOutcome::Rejected { error });
                    continue;
                }
            };
            reply(
                caller,
                ClientSendOutcome::Executed {
                    detail: json!({"answered": method, "with": kind}).to_string(),
                },
            );
            return (body, false);
        }
    }
}
