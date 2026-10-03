pub mod actions;
pub mod api;
use crate::server::bolt::{
    messages as m,
    packstream::{self, Dechunker, Value as Wire},
};
use crate::{
    client::{command_support, llm_budget::call_llm_for_client},
    logging::emit::Log,
    protocol::{ConnectContext, Event, EventType},
    state::{
        client_handles::{ClientCommand, ClientSendOutcome},
        AccessLogOwner, ClientStatus,
    },
};
pub use actions::BoltClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
pub const QUEUE_CAPACITY: usize = 8;
pub const MAX_FOLLOWUPS: u8 = 4;
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Stream = Box<dyn Io>;
type HandlerEvent = (Event, u8);
type HandlerAction = (Value, u8);
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Authentication,
    Ready,
    Streaming,
    TxReady,
    TxStreaming,
    Failed,
}
impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Ready => "ready",
            Self::Streaming => "streaming",
            Self::TxReady => "tx_ready",
            Self::TxStreaming => "tx_streaming",
            Self::Failed => "failed",
        }
    }
}
struct Core {
    minor: u8,
    phase: Phase,
    secret: Option<String>,
    authenticated: bool,
    startup_auth_metadata: Option<Value>,
    in_transaction: bool,
    fields: Vec<Value>,
    qid: Option<i64>,
}
struct Prepared {
    wire: Vec<u8>,
    operation: &'static str,
    n: Option<usize>,
}
impl Core {
    fn prepare(&mut self, action: api::Action) -> Result<Prepared> {
        let (tag, fields, operation, n) = match action {
            api::Action::Login { username, password } => {
                ensure!(
                    self.minor >= 1 && self.phase == Phase::Authentication,
                    "Bolt login requires5.1+ authentication phase"
                );
                let auth = api::auth(username.as_deref(), password.as_deref());
                if password.is_some() {
                    self.secret = password;
                }
                (m::LOGON, vec![auth], "login", None)
            }
            api::Action::Logoff => {
                ensure!(
                    self.minor >= 1 && self.phase == Phase::Ready,
                    "Bolt logoff requires5.1+ ready phase"
                );
                (m::LOGOFF, vec![], "logoff", None)
            }
            api::Action::Run {
                query,
                parameters,
                extra,
            } => {
                ensure!(
                    matches!(self.phase, Phase::Ready | Phase::TxReady),
                    "Bolt RUN requires ready phase with no open result"
                );
                if self.in_transaction {
                    ensure!(
                        matches!(&extra,Wire::Map(v) if v.is_empty()),
                        "Bolt transaction overrides belong to BEGIN"
                    );
                }
                (
                    m::RUN,
                    vec![Wire::string(query), parameters, extra],
                    "run",
                    None,
                )
            }
            api::Action::Pull(n) => {
                ensure!(
                    matches!(self.phase, Phase::Streaming | Phase::TxStreaming),
                    "Bolt PULL requires an open result"
                );
                let mut extra = vec![("n".into(), Wire::Int(n as i64))];
                if let Some(qid) = self.qid {
                    extra.push(("qid".into(), Wire::Int(qid)));
                }
                (m::PULL, vec![Wire::Map(extra)], "pull", Some(n))
            }
            api::Action::Discard => {
                ensure!(
                    matches!(self.phase, Phase::Streaming | Phase::TxStreaming),
                    "Bolt DISCARD requires an open result"
                );
                let mut extra = vec![("n".into(), Wire::Int(-1))];
                if let Some(qid) = self.qid {
                    extra.push(("qid".into(), Wire::Int(qid)));
                }
                (m::DISCARD, vec![Wire::Map(extra)], "discard", None)
            }
            api::Action::Begin(extra) => {
                ensure!(
                    self.phase == Phase::Ready,
                    "Bolt BEGIN requires ready phase"
                );
                (m::BEGIN, vec![extra], "begin", None)
            }
            api::Action::Commit => {
                ensure!(
                    self.phase == Phase::TxReady,
                    "Bolt COMMIT requires transaction without open result"
                );
                (m::COMMIT, vec![], "commit", None)
            }
            api::Action::Rollback => {
                ensure!(
                    self.phase == Phase::TxReady,
                    "Bolt ROLLBACK requires transaction without open result"
                );
                (m::ROLLBACK, vec![], "rollback", None)
            }
            api::Action::Reset => {
                ensure!(self.authenticated, "Bolt RESET requires authentication");
                (m::RESET, vec![], "reset", None)
            }
            api::Action::Disconnect => bail!("disconnect handled by session"),
        };
        Ok(Prepared {
            wire: api::message(tag, fields)?,
            operation,
            n,
        })
    }
    fn finish(
        &mut self,
        prepared: &Prepared,
        metadata: Value,
        records: Vec<Value>,
    ) -> Result<(&'static EventType, Value)> {
        let operation = prepared.operation;
        let event = match operation {
            "login" => {
                self.authenticated = true;
                self.phase = Phase::Ready;
                (
                    &*actions::AUTH_EVENT,
                    json!({"operation":"login","authentication_verified":true,"metadata":metadata,"phase":self.phase.name()}),
                )
            }
            "logoff" => {
                self.authenticated = false;
                self.phase = Phase::Authentication;
                (
                    &*actions::AUTH_EVENT,
                    json!({"operation":"logoff","authentication_verified":false,"metadata":metadata,"phase":self.phase.name()}),
                )
            }
            "run" => {
                self.fields = metadata
                    .get("fields")
                    .and_then(Value::as_array)
                    .context("Bolt RUN SUCCESS requires fields")?
                    .clone();
                self.qid = metadata.get("qid").and_then(Value::as_i64);
                self.phase = if self.in_transaction {
                    Phase::TxStreaming
                } else {
                    Phase::Streaming
                };
                (
                    &*actions::RUN_EVENT,
                    json!({"fields":self.fields,"qid":self.qid,"metadata":metadata,"in_transaction":self.in_transaction}),
                )
            }
            "pull" | "discard" => {
                let has_more = metadata
                    .get("has_more")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                ensure!(
                    operation != "discard" || !has_more,
                    "Bolt DISCARD-all cannot leave more records"
                );
                ensure!(
                    !has_more || records.len() == prepared.n.unwrap_or(0),
                    "Bolt partial PULL must return requested count"
                );
                let fields = self.fields.clone();
                if !has_more {
                    self.fields.clear();
                    self.qid = None;
                    self.phase = if self.in_transaction {
                        Phase::TxReady
                    } else {
                        Phase::Ready
                    };
                }
                (
                    &*actions::PAGE_EVENT,
                    json!({"operation":operation,"fields":fields,"records":records,"has_more":has_more,"summary":metadata,"in_transaction":self.in_transaction}),
                )
            }
            "begin" | "commit" | "rollback" | "reset" => {
                self.in_transaction = operation == "begin";
                self.fields.clear();
                self.qid = None;
                self.phase = if self.in_transaction {
                    Phase::TxReady
                } else {
                    Phase::Ready
                };
                (
                    &*actions::TX_EVENT,
                    json!({"operation":operation,"metadata":metadata,"in_transaction":self.in_transaction,"phase":self.phase.name()}),
                )
            }
            _ => unreachable!(),
        };
        Ok(event)
    }
}
fn endpoint(address: &str) -> Result<url::Url> {
    ensure!(address.len() <= 4096, "Bolt endpoint length limit");
    let normalized = if address.contains("://") {
        address.to_owned()
    } else {
        format!("bolt://{address}")
    };
    let url = url::Url::parse(&normalized).map_err(|_| anyhow::anyhow!("invalid Bolt endpoint"))?;
    ensure!(
        matches!(url.scheme(), "bolt" | "bolt+s")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none(),
        "Bolt endpoint must be a direct bolt or bolt+s origin without URL credentials/path/query"
    );
    #[cfg(target_arch = "wasm32")]
    ensure!(url.scheme() == "bolt", "browser Bolt supports TCP only");
    Ok(url)
}
async fn transport(url: &url::Url) -> Result<(Stream, SocketAddr)> {
    let host = match url.host().unwrap() {
        url::Host::Domain(s) => s.to_owned(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    };
    let socket =
        tokio::net::TcpStream::connect((host.as_str(), url.port().unwrap_or(7687))).await?;
    let remote = socket.peer_addr()?;
    #[cfg(not(target_arch = "wasm32"))]
    if url.scheme() == "bolt+s" {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host)
            .map_err(|_| anyhow::anyhow!("invalid Bolt TLS server name"))?;
        let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
            .connect(name, socket)
            .await
            .map_err(|_| anyhow::anyhow!("Bolt TLS verification failed"))?;
        return Ok((Box::new(tls), remote));
    }
    Ok((Box::new(socket), remote))
}
async fn read_reply(
    io: &mut Stream,
    decoder: &mut Dechunker,
    minor: u8,
    secret: Option<&str>,
) -> Result<(api::Reply, usize)> {
    let mut buffer = [0u8; 16384];
    loop {
        if let Some(bytes) = decoder
            .next_message()
            .map_err(|_| anyhow::anyhow!("Bolt message byte limit"))?
        {
            let len = bytes.len();
            return Ok((api::reply_private(&bytes, minor, secret)?, len));
        }
        let count = io.read(&mut buffer).await?;
        ensure!(count > 0, "Bolt peer closed before response");
        decoder.push(&buffer[..count]);
    }
}
async fn startup(
    mut ctx: ConnectContext,
    url: &url::Url,
    username: Option<String>,
    password: Option<String>,
) -> Result<(ConnectContext, Stream, SocketAddr, Dechunker, Core, Value)> {
    ctx.startup_params = None;
    let (mut io, remote) = transport(url).await?;
    let mut handshake = Vec::from(m::MAGIC);
    handshake.extend(api::PROPOSALS);
    io.write_all(&handshake).await?;
    let mut selected = [0u8; 4];
    io.read_exact(&mut selected).await?;
    let minor = api::selected_version(selected)?;
    let mut decoder = Dechunker::new(packstream::MAX_MESSAGE_BYTES);
    io.write_all(&api::hello(
        minor,
        username.as_deref(),
        password.as_deref(),
    )?)
    .await?;
    io.flush().await?;
    let (reply, _) = read_reply(&mut io, &mut decoder, minor, password.as_deref()).await?;
    let api::Reply::Success(server) = reply else {
        bail!("Bolt HELLO refused")
    };
    ensure!(
        server.get("server").and_then(Value::as_str).is_some(),
        "Bolt HELLO requires server agent"
    );
    let mut authenticated = minor == 0;
    let mut startup_auth_metadata = None;
    if minor >= 1 && username.is_some() {
        io.write_all(&api::message(
            m::LOGON,
            vec![api::auth(username.as_deref(), password.as_deref())],
        )?)
        .await?;
        io.flush().await?;
        let api::Reply::Success(metadata) =
            read_reply(&mut io, &mut decoder, minor, password.as_deref())
                .await?
                .0
        else {
            bail!("Bolt startup authentication refused")
        };
        startup_auth_metadata = Some(metadata);
        authenticated = true;
    }
    let core = Core {
        minor,
        phase: if authenticated {
            Phase::Ready
        } else {
            Phase::Authentication
        },
        secret: password,
        authenticated,
        startup_auth_metadata,
        in_transaction: false,
        fields: vec![],
        qid: None,
    };
    Ok((ctx, io, remote, decoder, core, server))
}
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let url = endpoint(&ctx.remote_addr)?;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut username = None;
    let mut password = None;
    if let Some(params) = &ctx.startup_params {
        timeout_secs = params
            .get_optional_u64("request_timeout_secs")?
            .unwrap_or(timeout_secs);
        username = params
            .get_optional_string("username")
            .map_err(|_| anyhow::anyhow!("Bolt username must be string"))?;
        password = params
            .get_optional_string("password")
            .map_err(|_| anyhow::anyhow!("Bolt password must be string"))?;
    }
    ensure!(
        (1..=30).contains(&timeout_secs),
        "Bolt request_timeout_secs must be1..30"
    );
    ensure!(
        username.is_some() == password.is_some(),
        "Bolt startup basic authentication requires username and password"
    );
    ensure!(
        username
            .as_ref()
            .is_none_or(|v| !v.is_empty() && v.len() <= 256)
            && password
                .as_ref()
                .is_none_or(|v| !v.is_empty() && v.len() <= api::MAX_PASSWORD),
        "Bolt startup authentication length limit"
    );
    let timeout = Duration::from_secs(timeout_secs);
    let (ctx, io, remote, decoder, core, server) =
        tokio::time::timeout(timeout, startup(ctx, &url, username, password))
            .await
            .context("Bolt connect/handshake/HELLO deadline")??;
    let external = command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (actions_tx, actions_rx) = mpsc::channel::<HandlerAction>(QUEUE_CAPACITY);
    let (events_tx, mut events_rx) = mpsc::channel::<HandlerEvent>(QUEUE_CAPACITY);
    let handler_ctx = ctx.clone();
    let handler = tokio::spawn(async move {
        while let Some((event, depth)) = events_rx.recv().await {
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
                &BoltClientProtocol,
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
                    for value in result.actions {
                        if depth >= MAX_FOLLOWUPS && value["type"] != "disconnect" {
                            crate::utils::json_budget::drop_iteratively(value);
                            Log::new(Some(&handler_ctx.status_tx))
                                .warn("Bolt handler followup limit reached");
                            continue;
                        }
                        if actions_tx.send((value, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(_) => Log::new(Some(&handler_ctx.status_tx))
                    .warn("Bolt event handler failed; credential diagnostics hidden"),
            }
        }
    });
    let handler_abort = handler.abort_handle();
    ctx.state.register_client_task(ctx.client_id, handler).await;
    let task_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &task_ctx, io, decoder, core, server, url, timeout, external, actions_rx, events_tx,
        )
        .await;
        handler_abort.abort();
        let status = if result.is_ok() {
            ClientStatus::Disconnected
        } else {
            Log::new(Some(&task_ctx.status_tx))
                .warn("Bolt bounded session failure; backend outcome unknown");
            ClientStatus::Error("Bolt bounded session failure; backend outcome unknown".into())
        };
        task_ctx
            .state
            .update_client_status(task_ctx.client_id, status)
            .await;
        task_ctx
            .state
            .remove_client_handle(task_ctx.client_id)
            .await;
        let _ = task_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(remote)
}
fn emit(
    events: &mpsc::Sender<HandlerEvent>,
    event: &'static EventType,
    data: Value,
    depth: u8,
    _secret: Option<&str>,
) -> Result<()> {
    // Native data was redacted before typed wrappers were constructed.
    events
        .try_send((Event::new(event, data), depth))
        .context("Bolt event queue full")
}
async fn operation(
    io: &mut Stream,
    decoder: &mut Dechunker,
    core: &mut Core,
    prepared: &Prepared,
    command: Option<ClientCommand>,
    events: &mpsc::Sender<HandlerEvent>,
    depth: u8,
) -> Result<bool> {
    io.write_all(&prepared.wire).await?;
    io.flush().await?;
    if let Some(command) = command {
        command_support::reply(
            command,
            Ok(ClientSendOutcome::Sent {
                bytes_sent: prepared.wire.len(),
            }),
        );
    }
    let mut records = Vec::new();
    let mut page_bytes = 0usize;
    let mut retained = 0usize;
    loop {
        let (reply, bytes) = read_reply(io, decoder, core.minor, core.secret.as_deref()).await?;
        page_bytes = page_bytes.saturating_add(bytes);
        ensure!(
            page_bytes <= api::MAX_PAGE_BYTES,
            "Bolt result page byte limit"
        );
        match reply {
            api::Reply::Record(record) => {
                ensure!(prepared.operation == "pull", "Bolt RECORD requires PULL");
                ensure!(
                    record.len() == core.fields.len(),
                    "Bolt record field width mismatch"
                );
                ensure!(
                    records.len() < prepared.n.unwrap_or(0),
                    "Bolt record page count limit"
                );
                let row = Value::Array(record);
                retained = retained.saturating_add(
                    api::retained_size(&row).context("Bolt record retained-content limit")?,
                );
                ensure!(
                    retained <= api::MAX_PAGE_BYTES,
                    "Bolt page retained-content limit"
                );
                records.push(row);
            }
            api::Reply::Success(metadata) => {
                let (event, data) = core.finish(prepared, metadata, records)?;
                ensure!(
                    crate::utils::json_budget::within_budget(
                        &data,
                        api::MAX_RETAINED_BYTES,
                        api::MAX_NODES,
                        api::MAX_EVENT_DEPTH
                    ),
                    "Bolt result event budget"
                );
                emit(events, event, data, depth, core.secret.as_deref())?;
                return Ok(false);
            }
            api::Reply::Failure(failure) => {
                let fatal = matches!(prepared.operation, "login" | "logoff" | "reset");
                core.phase = Phase::Failed;
                core.fields.clear();
                core.qid = None;
                emit(
                    events,
                    &actions::FAILURE_EVENT,
                    json!({"operation":prepared.operation,"ignored":false,"failure":failure,"records_discarded":records.len(),"phase":if fatal{"defunct"}else{"failed"}}),
                    depth,
                    core.secret.as_deref(),
                )?;
                return Ok(fatal);
            }
            api::Reply::Ignored => {
                core.phase = Phase::Failed;
                core.fields.clear();
                core.qid = None;
                emit(
                    events,
                    &actions::FAILURE_EVENT,
                    json!({"operation":prepared.operation,"ignored":true,"failure":null,"records_discarded":records.len(),"phase":"failed"}),
                    depth,
                    core.secret.as_deref(),
                )?;
                return Ok(false);
            }
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn session(
    ctx: &ConnectContext,
    mut io: Stream,
    mut decoder: Dechunker,
    mut core: Core,
    server: Value,
    url: url::Url,
    timeout: Duration,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<HandlerAction>,
    events: mpsc::Sender<HandlerEvent>,
) -> Result<()> {
    emit(
        &events,
        &actions::CONNECTED_EVENT,
        json!({"endpoint":api::redact_text(url.as_str(),core.secret.as_deref()),"version":format!("5.{}",core.minor),"server":server,"authentication_verified":core.authenticated,"authentication_metadata":core.startup_auth_metadata.take(),"phase":core.phase.name()}),
        0,
        core.secret.as_deref(),
    )?;
    let mut buffer = [0u8; 16384];
    loop {
        // An unsolicited response cannot be attributed to a new command.
        ensure!(
            decoder
                .next_message()
                .map_err(|_| anyhow::anyhow!("Bolt message byte limit"))?
                .is_none(),
            "Bolt unsolicited response"
        );
        let (value, depth, command) = tokio::select! {
            command=external.recv()=>{let Some(mut c)=command else{return Ok(())};(std::mem::take(&mut c.action),0,Some(c))},
            action=internal.recv()=>{let Some((a,d))=action else{return Ok(())};(a,d,None)},
            read=io.read(&mut buffer)=>{let n=read?;if n==0{return Ok(())}decoder.push(&buffer[..n]);continue},
        };
        if !api::within_budget(&value) {
            crate::utils::json_budget::drop_iteratively(value);
            if let Some(command) = command {
                command_support::reply(
                    command,
                    Ok(ClientSendOutcome::Rejected {
                        error: "Bolt action depth/node/retained-content limit".into(),
                    }),
                );
            } else {
                emit(
                    &events,
                    &actions::ERROR_EVENT,
                    json!({"category":"action","error":"Bolt action depth/node/retained-content limit","backend_outcome":"not_sent"}),
                    depth,
                    None,
                )?;
            }
            continue;
        }
        let result = api::action(&value);
        // Private access-log copy records a known offered discriminant only.
        let shown = if result.is_ok() {
            json!({"type":value["type"]})
        } else {
            json!({})
        };
        crate::utils::json_budget::drop_iteratively(value);
        let action = match result {
            Ok(action) => action,
            Err(error) => {
                if let Some(command) = command {
                    command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: error.to_string(),
                        }),
                    );
                } else {
                    emit(
                        &events,
                        &actions::ERROR_EVENT,
                        json!({"category":"action","error":error.to_string(),"backend_outcome":"not_sent"}),
                        depth,
                        core.secret.as_deref(),
                    )?;
                }
                continue;
            }
        };
        if matches!(action, api::Action::Disconnect) {
            if let Some(command) = command {
                command_support::reply(command, Ok(ClientSendOutcome::Disconnected));
            }
            let _ = tokio::time::timeout(timeout, io.write_all(&api::message(m::GOODBYE, vec![])?))
                .await;
            return Ok(());
        }
        let prepared = match core.prepare(action) {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(command) = command {
                    command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: error.to_string(),
                        }),
                    );
                } else {
                    emit(
                        &events,
                        &actions::ERROR_EVENT,
                        json!({"category":"action","error":error.to_string(),"backend_outcome":"not_sent"}),
                        depth,
                        core.secret.as_deref(),
                    )?;
                }
                continue;
            }
        };
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "bolt",
                    None,
                    "injected_action",
                    shown,
                    vec![json!({"operation":prepared.operation,"pending":true})],
                )
                .await;
        }
        let operation_deadline = tokio::time::Instant::now() + timeout;
        let pending = tokio::time::timeout(
            timeout,
            operation(
                &mut io,
                &mut decoder,
                &mut core,
                &prepared,
                command,
                &events,
                depth,
            ),
        );
        tokio::pin!(pending);
        loop {
            ensure!(
                tokio::time::Instant::now() < operation_deadline,
                "Bolt whole operation deadline"
            );
            tokio::select! {biased;
                command=external.recv()=>{
                    let Some(mut command)=command else{return Ok(())};let value=std::mem::take(&mut command.action);let bounded=api::within_budget(&value);let disconnect=bounded&&value.get("type").and_then(Value::as_str)==Some("disconnect");crate::utils::json_budget::drop_iteratively(value);
                    if disconnect{command_support::reply(command,Ok(ClientSendOutcome::Disconnected));return Ok(())}
                    command_support::reply(command,Ok(ClientSendOutcome::Rejected{error:if bounded{"Bolt operation pending; retry after native receipt"}else{"Bolt action depth/node/retained-content limit"}.into()}));
                },
                result=&mut pending=>{if result.context("Bolt whole operation deadline")??{return Ok(())}break},
            }
        }
    }
}
