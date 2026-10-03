use super::{
    actions,
    request::{self, Request},
};
use crate::{
    protocol::{ConnectContext, Event, StartupParams},
    server::gnmi::{
        codec::{BoundedProstCodec, WireName, MAX_MESSAGE_BYTES},
        proto::gnmi as pb,
        semantic::{self, Mode},
        value,
    },
    state::{
        client_handles::{ClientCommand, ClientSendOutcome},
        AccessLogOwner, ClientStatus,
    },
};
use anyhow::{ensure, Context, Result};
use futures::{
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use prost::Message;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tonic::{
    codec::CompressionEncoding,
    transport::{Channel, Endpoint},
    Status,
};
struct SocketGuard(std::net::TcpStream);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}
fn seconds(params: Option<&StartupParams>, key: &str, default: u64, max: u64) -> Result<Duration> {
    let value = params
        .map(|p| p.get_optional_u64(key))
        .transpose()?
        .flatten()
        .unwrap_or(default);
    ensure!((1..=max).contains(&value), "{key} must be 1..{max}");
    Ok(Duration::from_secs(value))
}
fn string(params: Option<&StartupParams>, key: &str) -> Result<Option<String>> {
    Ok(params
        .map(|p| p.get_optional_string(key))
        .transpose()?
        .flatten())
}
pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let tls = params
        .map(|p| p.get_optional_bool("use_tls"))
        .transpose()?
        .flatten()
        .unwrap_or(super::DEFAULT_TLS);
    let startup = seconds(
        params,
        "connect_timeout_secs",
        super::CONNECT_TIMEOUT_SECS,
        60,
    )?;
    let timeout = seconds(
        params,
        "rpc_timeout_secs",
        crate::server::gnmi::DEFAULT_RPC_TIMEOUT_SECS,
        3600,
    )?;
    let idle = seconds(
        params,
        "idle_timeout_secs",
        super::DEFAULT_IDLE_TIMEOUT_SECS,
        3600,
    )?;
    let name = string(params, "server_name")?;
    let ca = string(params, "ca_file")?;
    ensure!(
        tls || (name.is_none() && ca.is_none()),
        "server_name/ca_file require use_tls"
    );
    let uri: hyper::Uri = format!(
        "{}://{}",
        if tls { "https" } else { "http" },
        ctx.remote_addr
    )
    .parse()?;
    ensure!(
        uri.path() == "/" && uri.query().is_none() && uri.authority().is_some(),
        "remote address must be host:port"
    );
    let host = uri.host().context("endpoint has no hostname")?;
    // http::Uri preserves IPv6 brackets; DNS/socket lookup and certificate names
    // take the literal address without those authority delimiters.
    let host = host
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned();
    let port = uri.port_u16().unwrap_or(crate::server::gnmi::DEFAULT_PORT);
    let name = name.unwrap_or_else(|| host.clone());
    ensure!(
        !name.is_empty() && name.len() <= 253,
        "server_name must be 1..253 bytes"
    );
    let (channel, socket, local, remote) = tokio::time::timeout(startup, async move {
        let ca = if let Some(path) = ca {
            Some(crate::server::gnmi::tls::read_pem(path).await?)
        } else {
            None
        };
        let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
        let local = stream.local_addr()?;
        let remote = stream.peer_addr()?;
        let raw = stream.into_std()?;
        let socket = SocketGuard(raw.try_clone()?);
        let stream = tokio::net::TcpStream::from_std(raw)?;
        let mut endpoint = Endpoint::from(uri)
            .connect_timeout(startup)
            .concurrency_limit(16)
            .buffer_size(16)
            .initial_stream_window_size(65536)
            .initial_connection_window_size(1048576)
            .http2_max_header_list_size(32768);
        if tls {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut config = tonic::transport::ClientTlsConfig::new()
                .with_webpki_roots()
                .domain_name(name);
            if let Some(ca) = ca {
                config = config.ca_certificate(tonic::transport::Certificate::from_pem(ca));
            }
            endpoint = endpoint.tls_config(config)?;
        }
        let mut stream = Some(stream);
        let connector = tower::service_fn(move |_: hyper::Uri| {
            let stream = stream.take();
            async move {
                stream.map(hyper_util::rt::TokioIo::new).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "gNMI automatic reconnect is disabled",
                    )
                })
            }
        });
        let channel = endpoint.connect_with_connector(connector).await?;
        Ok::<_, anyhow::Error>((channel, socket, local, remote))
    })
    .await
    .context("gNMI startup deadline exceeded")??;
    let now = crate::utils::clock::Instant::now();
    ctx.state
        .with_client_mut(ctx.client_id, |client| {
            client.connection = Some(crate::state::ClientConnectionState {
                id: ctx.client_id,
                remote_addr: ctx.remote_addr.clone(),
                connected_addr: Some(remote),
                local_addr: Some(local),
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: now,
                status: ClientStatus::Connected,
                status_changed_at: now,
                protocol_info: crate::state::server::ProtocolConnectionInfo::new(
                    json!({"tls_verified":tls}),
                ),
            });
        })
        .await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let commands =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let connected = Event::new(
        &actions::CONNECTED,
        json!({"remote_addr":ctx.remote_addr,"tls_verified":tls}),
    );
    ctx.state
        .clone()
        .spawn_client_task(
            ctx.client_id,
            run(ctx, channel, socket, commands, connected, timeout, idle),
        )
        .await;
    Ok(local)
}
type Handler = BoxFuture<'static, (u8, Result<Vec<Value>>)>;
type Operation = BoxFuture<'static, (u32, u8, Event)>;
fn handler(ctx: ConnectContext, event: Event, depth: u8) -> Handler {
    async move {
        let Some(instruction) = ctx.state.get_instruction_for_client(ctx.client_id).await else {
            return (depth, Ok(vec![]));
        };
        let memory = ctx
            .state
            .get_memory_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        let result = crate::client::llm_budget::call_llm_for_client(
            &ctx.llm_client,
            &ctx.state,
            ctx.client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &actions::GnmiClientProtocol,
            &ctx.status_tx,
        )
        .await;
        (
            depth,
            match result {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        ctx.state.set_memory_for_client(ctx.client_id, memory).await;
                    }
                    Ok(result.actions)
                }
                Err(error) => Err(error),
            },
        )
    }
    .boxed()
}
struct Entry {
    abort: AbortHandle,
    poll: Option<mpsc::Sender<pb::SubscribeRequest>>,
    cycle: Arc<AtomicBool>,
    mode: Option<Mode>,
    inputs: usize,
}
struct Owner {
    ctx: ConnectContext,
    channel: Channel,
    timeout: Duration,
    events: mpsc::Sender<(u8, Event)>,
    operations: FuturesUnordered<Operation>,
    handlers: FuturesUnordered<Handler>,
    pending: VecDeque<(u8, Event)>,
    entries: HashMap<u32, Entry>,
    used: HashSet<u32>,
}
fn ended(id: u32, status: Status, count: usize) -> Event {
    Event::new(
        &actions::ENDED,
        json!({"call_id":id,"code":status.code() as i32,"message":crate::utils::truncate_for_llm(status.message(),512),"response_count":count}),
    )
}
async fn event(events: &mpsc::Sender<(u8, Event)>, depth: u8, event: Event) -> Result<(), Status> {
    events
        .send((depth, event))
        .await
        .map_err(|_| Status::cancelled("client owner removed"))
}
async fn unary<T, U>(
    channel: Channel,
    path: &'static str,
    input: T,
    gzip: bool,
    timeout: Duration,
) -> Result<U, Status>
where
    T: Message + WireName + Default + Send + 'static,
    U: Message + WireName + Default + Send + 'static,
{
    let mut client = tonic::client::Grpc::new(channel)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    if gzip {
        client = client.send_compressed(CompressionEncoding::Gzip);
    }
    client
        .ready()
        .await
        .map_err(|_| Status::unavailable("gNMI connection not ready"))?;
    let mut request = tonic::Request::new(input);
    request.set_timeout(timeout);
    // The streaming reader verifies the unary shape explicitly instead of silently
    // draining extra messages while waiting for trailers in tonic's unary helper.
    let mut response = client
        .server_streaming(
            request,
            hyper::http::uri::PathAndQuery::from_static(path),
            BoundedProstCodec::<T, U>::default(),
        )
        .await?
        .into_inner();
    let message = response
        .message()
        .await?
        .ok_or_else(|| Status::internal("unary response has no message"))?;
    if response.message().await?.is_some() {
        return Err(Status::invalid_argument(
            "unary response has multiple messages",
        ));
    }
    Ok(message)
}
#[allow(clippy::too_many_arguments)]
fn operation(
    channel: Channel,
    request: Request,
    gzip: bool,
    timeout: Duration,
    id: u32,
    depth: u8,
    events: mpsc::Sender<(u8, Event)>,
    input: Option<mpsc::Receiver<pb::SubscribeRequest>>,
    cycle: Arc<AtomicBool>,
    registration: futures::future::AbortRegistration,
) -> Operation {
    async move{
        let count=Arc::new(AtomicUsize::new(0));let seen=count.clone();
        let call=async move{
            let (method,response)=match request{
                Request::Capabilities(request)=>{let response:pb::CapabilityResponse=unary(channel,"/gnmi.gNMI/Capabilities",request,gzip,timeout).await?;( "Capabilities",semantic::capability_result(response)?)},
                Request::Get(request)=>{let encoding=request.encoding;let response:pb::GetResponse=unary(channel,"/gnmi.gNMI/Get",request,gzip,timeout).await?;("Get",semantic::get_result(response,encoding)?)},
                Request::Set(request)=>{let expected=request.clone();let response:pb::SetResponse=unary(channel,"/gnmi.gNMI/Set",request,gzip,timeout).await?;
                    semantic::check_set_ack(&expected,&response)?;
                    ("Set",semantic::set_result(response)?)},
                Request::Subscribe(list)=>{
                    let selected=semantic::subscription(&list)?;let mut client=pb::g_nmi_client::GNmiClient::new(channel).accept_compressed(CompressionEncoding::Gzip).max_decoding_message_size(MAX_MESSAGE_BYTES).max_encoding_message_size(MAX_MESSAGE_BYTES);
                    if gzip{client=client.send_compressed(CompressionEncoding::Gzip);}
                    let input=input.ok_or_else(||Status::internal("missing subscription input"))?;
                    let mut request=tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(input));request.set_timeout(timeout);
                    let mut response=client.subscribe(request).await?.into_inner();let mut synced=false;
                    while let Some(response)=response.message().await?{
                        let sequence=seen.load(Ordering::Relaxed)+1;if sequence>256{return Err(Status::resource_exhausted("subscription output exceeds 256 messages"));}
                        if !response.extension.is_empty(){return Err(Status::unimplemented("subscription extensions excluded"));}
                        let event_type=match response.response{
                            Some(pb::subscribe_response::Response::Update(notification))=>{
                                if selected.mode==Mode::Once&&synced||selected.mode==Mode::Poll&&!cycle.load(Ordering::Relaxed)||selected.updates_only&&(!synced||selected.mode==Mode::Poll){return Err(Status::invalid_argument("notification outside requested snapshot"));}
                                value::notification_encoding(&notification,selected.encoding)?;
                                Event::new(&actions::UPDATE,json!({"call_id":id,"sequence":sequence,"notification":value::Notification::from_proto(notification)?}))
                            },
                            Some(pb::subscribe_response::Response::SyncResponse(true))=>{
                                if synced&&selected.mode!=Mode::Poll||!cycle.swap(false,Ordering::Relaxed){return Err(Status::invalid_argument("unexpected duplicate sync"));}synced=true;
                                Event::new(&actions::SYNC,json!({"call_id":id,"sequence":sequence}))
                            },
                            _=>return Err(Status::invalid_argument("unsupported or false subscription response")),
                        };seen.store(sequence,Ordering::Relaxed);event(&events,depth,event_type).await?;
                    }
                    if !synced||cycle.load(Ordering::Relaxed){return Err(Status::invalid_argument("subscription ended before snapshot sync"));}
                    return Ok::<_,Status>(());
                },
                _=>return Err(Status::internal("unexpected operation control")),
            };
            seen.store(1,Ordering::Relaxed);event(&events,depth,Event::new(&actions::RESPONSE,json!({"call_id":id,"method":method,"response":response}))).await?;Ok(())
        };
        let result=Abortable::new(tokio::time::timeout(timeout,call),registration).await;
        let status=match result{Ok(Ok(Ok(())))=>Status::ok(""),Ok(Ok(Err(status)))=>status,Ok(Err(_))=>Status::deadline_exceeded("gNMI RPC deadline exceeded"),Err(_)=>Status::cancelled("gNMI RPC cancelled")};
        (id,depth,ended(id,status,count.load(Ordering::Relaxed)))
    }.boxed()
}
impl Owner {
    fn apply(&mut self, action: &Value, depth: u8) -> Result<(ClientSendOutcome, bool)> {
        let request = request::parse(action)?;
        match request {
            Request::Disconnect => return Ok((ClientSendOutcome::Disconnected, true)),
            Request::Wait => {
                return Ok((
                    ClientSendOutcome::Executed {
                        detail: "waiting".into(),
                    },
                    false,
                ))
            }
            _ => {}
        }
        let id = u32::try_from(action["call_id"].as_u64().context("call_id required")?)?;
        match request {
            Request::Cancel => {
                self.entries
                    .get(&id)
                    .context("call_id is not active")?
                    .abort
                    .abort();
            }
            Request::Poll => {
                let entry = self.entries.get_mut(&id).context("call_id is not active")?;
                ensure!(
                    entry.mode == Some(Mode::Poll) && entry.inputs < 256,
                    "not an active bounded POLL subscription"
                );
                ensure!(
                    !entry.cycle.swap(true, Ordering::Relaxed),
                    "previous POLL snapshot has not synced"
                );
                let poll = pb::SubscribeRequest {
                    request: Some(pb::subscribe_request::Request::Poll(pb::Poll {})),
                    extension: vec![],
                };
                if let Err(error) = entry
                    .poll
                    .as_ref()
                    .context("missing POLL sender")?
                    .try_send(poll)
                {
                    entry.cycle.store(false, Ordering::Relaxed);
                    return Err(error.into());
                }
                entry.inputs += 1;
            }
            request => {
                ensure!(depth <= 4, "gNMI automatic follow-up depth exceeds 4");
                ensure!(
                    self.entries.len() < 16 && self.handlers.len() + self.pending.len() < 16,
                    "gNMI operation/handler capacity reached"
                );
                ensure!(
                    self.used.len() < 256 && !self.used.contains(&id),
                    "call_id reused or exceeds 256 calls"
                );
                let cycle = Arc::new(AtomicBool::new(true));
                let (sender, input, mode) = if let Request::Subscribe(list) = &request {
                    let (sender, input) = mpsc::channel(1);
                    sender.try_send(pb::SubscribeRequest {
                        request: Some(pb::subscribe_request::Request::Subscribe(list.clone())),
                        extension: vec![],
                    })?;
                    let mode = semantic::subscription(list)?.mode;
                    (Some(sender), Some(input), Some(mode))
                } else {
                    (None, None, None)
                };
                let (abort, registration) = AbortHandle::new_pair();
                self.operations.push(operation(
                    self.channel.clone(),
                    request,
                    action["gzip"].as_bool().unwrap_or(false),
                    self.timeout,
                    id,
                    depth,
                    self.events.clone(),
                    input,
                    cycle.clone(),
                    registration,
                ));
                self.entries.insert(
                    id,
                    Entry {
                        abort,
                        poll: sender,
                        cycle,
                        mode,
                        inputs: 1,
                    },
                );
                self.used.insert(id);
            }
        }
        Ok((
            ClientSendOutcome::Executed {
                detail: "gNMI operation/control queued; wire outcome follows in events".into(),
            },
            false,
        ))
    }
    async fn action(&mut self, action: Value, command: Option<ClientCommand>, depth: u8) -> bool {
        let outcome = self.apply(&action, depth);
        let stop = outcome.as_ref().is_ok_and(|(_, stop)| *stop);
        if let Some(command) = command {
            let outcome = outcome.map(|(value, _)| value);
            self.ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(self.ctx.client_id.as_u32()),
                    "gNMI",
                    None,
                    "injected_action",
                    action,
                    vec![match &outcome {
                        Ok(v) => serde_json::to_value(v).unwrap_or(Value::Null),
                        Err(_) => json!({"error":"gNMI action refused"}),
                    }],
                )
                .await;
            crate::client::command_support::reply(command, outcome);
        } else if outcome.is_err() {
            crate::logging::emit::Log::new(Some(&self.ctx.status_tx))
                .warn("gNMI automatic action refused");
        }
        stop
    }
    async fn receive(&mut self, depth: u8, event: Event) -> bool {
        self.ctx
            .state
            .record_access_log(
                AccessLogOwner::Client(self.ctx.client_id.as_u32()),
                "gNMI",
                None,
                event.id(),
                event.data.clone(),
                vec![],
            )
            .await;
        if self.handlers.len() < 16 {
            self.handlers.push(handler(self.ctx.clone(), event, depth));
            true
        } else if self.pending.len() < 16 {
            self.pending.push_back((depth, event));
            true
        } else {
            false
        }
    }
}
async fn run(
    ctx: ConnectContext,
    channel: Channel,
    _socket: SocketGuard,
    mut commands: mpsc::Receiver<ClientCommand>,
    connected: Event,
    timeout: Duration,
    idle: Duration,
) {
    let (events, mut incoming) = mpsc::channel(16);
    let mut owner = Owner {
        ctx: ctx.clone(),
        channel,
        timeout,
        events,
        operations: FuturesUnordered::new(),
        handlers: FuturesUnordered::new(),
        pending: VecDeque::new(),
        entries: HashMap::new(),
        used: HashSet::new(),
    };
    owner.receive(0, connected).await;
    let mut last = tokio::time::Instant::now();
    loop {
        while owner.handlers.len() < 16 {
            if let Some((depth, event)) = owner.pending.pop_front() {
                owner.handlers.push(handler(ctx.clone(), event, depth));
            } else {
                break;
            }
        }
        tokio::select! {
            command=commands.recv()=>{let Some(command)=command else{break;};last=tokio::time::Instant::now();if owner.action(command.action.clone(),Some(command),0).await{break;}},
            Some((id,depth,event))=owner.operations.next(),if !owner.operations.is_empty()=>{owner.entries.remove(&id);last=tokio::time::Instant::now();if !owner.receive(depth,event).await{break;}},
            Some((depth,event))=incoming.recv()=>{last=tokio::time::Instant::now();if !owner.receive(depth,event).await{break;}},
            Some((depth,result))=owner.handlers.next(),if !owner.handlers.is_empty()=>{
                last=tokio::time::Instant::now();let mut stop=false;
                if let Ok(actions)=result{if actions.len()>16{break;}for action in actions{if owner.action(action,None,depth.saturating_add(1)).await{stop=true;break;}}}
                if stop{break;}
            },
            _=tokio::time::sleep_until(last+idle),if owner.operations.is_empty()&&owner.handlers.is_empty()&&owner.pending.is_empty()=>break,
        }
    }
    drop(owner);
    ctx.state.remove_client_handle(ctx.client_id).await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Disconnected)
        .await;
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}
