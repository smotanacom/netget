//! Every HTTP/2 child and subscription future belongs to its TCP connection.
use super::{
    actions, codec,
    proto::gnmi as pb,
    semantic::{self, Answer, Mode},
    value,
};
use crate::{
    protocol::{Event, SpawnContext},
    server::{
        accept_bounded::{self, ConnectionActivity},
        connection::ConnectionId,
        grpc::streaming::{ConnectionTasks, OwnedExecutor},
    },
    state::ServerId,
};
use bytes::Bytes;
use futures::Stream;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    convert::Infallible,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use tonic::{codec::CompressionEncoding, Status};
use tower::Service;
pub const FIRST_BYTE_READ_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_BETWEEN_REQUESTS_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_CONNECTIONS: usize = 256;
pub const MAX_ACTIVE_RPCS: usize = 64;
const MAX_MESSAGES: usize = 256;
const MAX_HANDLERS: usize = 258;
const REFUSAL:&[u8]=b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: 5\r\nConnection: close\r\n\r\n";
#[derive(Clone)]
struct Context {
    llm: crate::llm::OllamaClient,
    state: Arc<crate::state::AppState>,
    server: ServerId,
    connection: ConnectionId,
    protocol: Arc<actions::GnmiProtocol>,
    id: u32,
    deadline: tokio::time::Instant,
}
impl Context {
    async fn decide(
        &self,
        event_type: &'static crate::protocol::EventType,
        request: Value,
    ) -> Result<Vec<Answer>, Status> {
        let event = Event::new(event_type, json!({"rpc_id":self.id,"request":request}));
        let result = crate::llm::action_helper::call_llm(
            &self.llm,
            &self.state,
            self.server,
            Some(self.connection),
            &event,
            self.protocol.as_ref(),
        )
        .await
        .map_err(|error| {
            if crate::llm::is_overload_error(&error) {
                Status::unavailable("netget: backend at capacity")
            } else {
                Status::internal("netget: handler unavailable")
            }
        })?;
        if result.has_failures() {
            return Err(Status::internal("netget: invalid handler response"));
        }
        let answers = result
            .protocol_results
            .iter()
            .filter_map(|result| {
                if let crate::llm::ActionResult::Custom { name, data } = result {
                    if name == "gnmi_answer" {
                        return Some(data);
                    }
                }
                None
            })
            .collect::<Vec<_>>();
        if answers.is_empty() || answers.len() > 16 {
            return Err(Status::internal("netget: no bounded gNMI answer"));
        }
        answers.into_iter().map(semantic::answer).collect()
    }
    async fn unary(
        &self,
        event: &'static crate::protocol::EventType,
        request: Value,
    ) -> Result<Answer, Status> {
        let mut answers = self.decide(event, request).await?;
        if answers.len() != 1 {
            return Err(Status::internal("netget: unary RPC requires one answer"));
        }
        let answer = answers.remove(0);
        if let Answer::Error(error) = answer {
            Err(error)
        } else {
            Ok(answer)
        }
    }
}
#[derive(Clone)]
struct Receiver(Context);
#[tonic::async_trait]
impl pb::g_nmi_server::GNmi for Receiver {
    async fn capabilities(
        &self,
        request: tonic::Request<pb::CapabilityRequest>,
    ) -> Result<tonic::Response<pb::CapabilityResponse>, Status> {
        if !request.get_ref().extension.is_empty() {
            return Err(Status::unimplemented("extensions excluded"));
        }
        match self.0.unary(&actions::CAPABILITIES, json!({})).await? {
            Answer::Capabilities(v) => Ok(tonic::Response::new(v)),
            _ => Err(Status::internal("netget: wrong Capabilities answer")),
        }
    }
    async fn get(
        &self,
        request: tonic::Request<pb::GetRequest>,
    ) -> Result<tonic::Response<pb::GetResponse>, Status> {
        let request = request.into_inner();
        let event = semantic::get_request(&request)?;
        match self.0.unary(&actions::GET, event).await? {
            Answer::Get(notification) => {
                for n in &notification {
                    value::notification_encoding(n, request.encoding)?;
                }
                Ok(tonic::Response::new(value::checked(pb::GetResponse {
                    notification,
                    error: None,
                    extension: vec![],
                })?))
            }
            _ => Err(Status::internal("netget: wrong Get answer")),
        }
    }
    async fn set(
        &self,
        request: tonic::Request<pb::SetRequest>,
    ) -> Result<tonic::Response<pb::SetResponse>, Status> {
        let request = request.into_inner();
        let event = semantic::set_request(&request)?;
        match self.0.unary(&actions::SET, event).await? {
            Answer::Set(timestamp) => Ok(tonic::Response::new(semantic::set_response(
                &request, &timestamp,
            )?)),
            _ => Err(Status::internal("netget: wrong Set answer")),
        }
    }
    type SubscribeStream =
        Pin<Box<dyn Stream<Item = Result<pb::SubscribeResponse, Status>> + Send>>;
    async fn subscribe(
        &self,
        request: tonic::Request<tonic::Streaming<pb::SubscribeRequest>>,
    ) -> Result<tonic::Response<Self::SubscribeStream>, Status> {
        let mut input = request.into_inner();
        let first = input
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("missing SubscriptionList"))?;
        if !first.extension.is_empty() {
            return Err(Status::unimplemented("extensions excluded"));
        }
        let Some(pb::subscribe_request::Request::Subscribe(list)) = first.request else {
            return Err(Status::invalid_argument(
                "first request must contain SubscriptionList",
            ));
        };
        let subscription = semantic::subscription(&list)?;
        let state = Subscription {
            ctx: self.0.clone(),
            subscription,
            input,
            pending: VecDeque::new(),
            input_closed: false,
            cycle: true,
            initial: true,
            synced: false,
            finished: false,
            wait: None,
            messages: 0,
            inputs: 1,
            handlers: 0,
        };
        let stream = futures::stream::try_unfold(state, |mut state| async move {
            let next = tokio::time::timeout_at(state.ctx.deadline, state.next())
                .await
                .map_err(|_| Status::deadline_exceeded("netget: subscription deadline"))??;
            Ok(next.map(|next| (next, state)))
        });
        Ok(tonic::Response::new(Box::pin(stream)))
    }
}
struct Subscription {
    ctx: Context,
    subscription: semantic::Subscription,
    input: tonic::Streaming<pb::SubscribeRequest>,
    pending: VecDeque<pb::SubscribeResponse>,
    input_closed: bool,
    cycle: bool,
    initial: bool,
    synced: bool,
    finished: bool,
    wait: Option<Duration>,
    messages: usize,
    inputs: usize,
    handlers: usize,
}
impl Subscription {
    fn input(
        &mut self,
        request: Option<pb::SubscribeRequest>,
        allow_poll: bool,
    ) -> Result<(), Status> {
        let Some(request) = request else {
            self.input_closed = true;
            return Ok(());
        };
        self.inputs += 1;
        if self.inputs > MAX_MESSAGES {
            return Err(Status::resource_exhausted(
                "subscription input count exceeds 256",
            ));
        }
        if !request.extension.is_empty() {
            return Err(Status::unimplemented("extensions excluded"));
        }
        if !allow_poll
            || self.subscription.mode != Mode::Poll
            || !matches!(
                request.request,
                Some(pb::subscribe_request::Request::Poll(_))
            )
        {
            return Err(Status::invalid_argument(
                "only POLL mode accepts Poll after its completed snapshot",
            ));
        }
        self.cycle = true;
        self.synced = false;
        Ok(())
    }
    fn push(&mut self, response: pb::SubscribeResponse) -> Result<(), Status> {
        self.messages += 1;
        if self.messages > MAX_MESSAGES {
            return Err(Status::resource_exhausted(
                "subscription output count exceeds 256",
            ));
        }
        if self.pending.len() >= 16 {
            return Err(Status::resource_exhausted(
                "subscription pending count exceeds 16",
            ));
        }
        use prost::Message;
        let bytes = self.pending.iter().map(Message::encoded_len).sum::<usize>();
        if bytes.saturating_add(response.encoded_len()) > codec::MAX_MESSAGE_BYTES {
            return Err(Status::resource_exhausted(
                "pending subscription output exceeds 1 MiB",
            ));
        }
        self.pending.push_back(value::checked(response)?);
        Ok(())
    }
    fn sync(&mut self) -> Result<(), Status> {
        if !self.cycle || self.synced {
            return Err(Status::failed_precondition(
                "sync belongs once to each initial/POLL snapshot",
            ));
        }
        self.push(pb::SubscribeResponse {
            response: Some(pb::subscribe_response::Response::SyncResponse(true)),
            extension: vec![],
        })?;
        self.synced = true;
        self.cycle = false;
        match self.subscription.mode {
            Mode::Once => self.finished = true,
            Mode::Stream => self.wait = Some(Duration::from_millis(1000)),
            Mode::Poll => {}
        }
        Ok(())
    }
    async fn decision(&mut self) -> Result<Vec<Answer>, Status> {
        self.handlers += 1;
        if self.handlers > MAX_HANDLERS {
            return Err(Status::resource_exhausted(
                "subscription handler count exceeds 258",
            ));
        }
        let kind = if self.initial {
            &*actions::SUBSCRIBE
        } else if self.cycle {
            &*actions::POLL
        } else {
            &*actions::TICK
        };
        let context = self.ctx.clone();
        let future = context.decide(kind, self.subscription.event.clone());
        tokio::pin!(future);
        loop {
            tokio::select! {
                answer=&mut future=>return answer,
                request=self.input.message(),if !self.input_closed=>{self.input(request?,false)?;}
            }
        }
    }
    fn apply(&mut self, answers: Vec<Answer>) -> Result<(), Status> {
        let started_cycle = self.cycle;
        let mut terminal = false;
        for answer in answers {
            if terminal {
                return Err(Status::internal(
                    "netget: action after terminal subscription control",
                ));
            }
            match answer {
                Answer::Update(notifications) => {
                    if self.synced && started_cycle {
                        return Err(Status::failed_precondition("snapshot update after sync"));
                    }
                    if self.subscription.updates_only && started_cycle {
                        return Err(Status::failed_precondition(
                            "updates_only snapshot cannot contain updates",
                        ));
                    }
                    for notification in notifications {
                        value::notification_encoding(&notification, self.subscription.encoding)?;
                        self.push(pb::SubscribeResponse {
                            response: Some(pb::subscribe_response::Response::Update(notification)),
                            extension: vec![],
                        })?;
                    }
                }
                Answer::Sync => {
                    self.sync()?;
                    if self.subscription.mode != Mode::Stream {
                        terminal = true;
                    }
                }
                Answer::Wait(ms) => {
                    if self.subscription.mode != Mode::Stream || !self.synced {
                        return Err(Status::failed_precondition(
                            "wait requires STREAM initial sync",
                        ));
                    }
                    self.wait = Some(Duration::from_millis(ms));
                    terminal = true;
                }
                Answer::Finish => {
                    if self.subscription.mode != Mode::Stream || !self.synced {
                        return Err(Status::failed_precondition(
                            "finish requires STREAM initial sync",
                        ));
                    }
                    self.finished = true;
                    terminal = true;
                }
                Answer::Error(error) => return Err(error),
                _ => return Err(Status::internal("netget: wrong subscription answer")),
            }
        }
        if started_cycle && !self.synced {
            return Err(Status::internal(
                "netget: snapshot answer requires gnmi_sync",
            ));
        }
        if !started_cycle && !terminal {
            return Err(Status::internal(
                "netget: stream answer requires wait or finish",
            ));
        }
        self.initial = false;
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<pb::SubscribeResponse>, Status> {
        loop {
            if let Some(next) = self.pending.pop_front() {
                return Ok(Some(next));
            }
            if self.finished {
                return Ok(None);
            }
            if self.cycle {
                if self.subscription.updates_only {
                    self.sync()?;
                    self.initial = false;
                    continue;
                }
                let answer = self.decision().await?;
                self.apply(answer)?;
                continue;
            }
            if self.subscription.mode == Mode::Poll {
                if self.input_closed {
                    return Ok(None);
                }
                let request = self.input.message().await?;
                self.input(request, true)?;
                continue;
            }
            let wait = self
                .wait
                .take()
                .ok_or_else(|| Status::internal("netget: missing stream wait"))?;
            let sleep = tokio::time::sleep(wait);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _=&mut sleep=>break,
                    request=self.input.message(),if !self.input_closed=>{self.input(request?,false)?;}
                }
            }
            let answer = self.decision().await?;
            self.apply(answer)?;
        }
    }
}
struct GuardedBody<G> {
    body: tonic::body::BoxBody,
    guard: Option<G>,
}
impl<G: Send + Unpin + 'static> hyper::body::Body for GuardedBody<G> {
    type Data = Bytes;
    type Error = Status;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Status>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(result, std::task::Poll::Ready(None | Some(Err(_)))) {
            self.guard = None;
        }
        result
    }
    // Native tonic's trailers-only status must retain END_STREAM on its initial
    // HEADERS. Hyper then drops the body/guard; no fabricated DATA frame is needed.
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}
fn hold_body<G: Send + Unpin + 'static>(
    body: tonic::body::BoxBody,
    guard: G,
) -> tonic::body::BoxBody {
    GuardedBody {
        body,
        guard: Some(guard),
    }
    .boxed_unsync()
}
async fn unary(request: Request<Incoming>) -> Result<Request<tonic::body::BoxBody>, Status> {
    let (parts, mut body) = request.into_parts();
    let mut bytes = Vec::new();
    let mut length = None;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Status::invalid_argument("unreadable gNMI body"))?;
        if let Ok(data) = frame.into_data() {
            if bytes.len().saturating_add(data.len()) > codec::MAX_MESSAGE_BYTES + 5 {
                return Err(Status::resource_exhausted(
                    "gNMI unary body exceeds message bound",
                ));
            }
            bytes.extend_from_slice(&data);
            if length.is_none() && bytes.len() >= 5 {
                let size = u32::from_be_bytes(
                    bytes[1..5]
                        .try_into()
                        .map_err(|_| Status::invalid_argument("frame prefix"))?,
                ) as usize;
                if size > codec::MAX_MESSAGE_BYTES {
                    return Err(Status::resource_exhausted("gNMI message exceeds 1 MiB"));
                }
                length = Some(size + 5);
            }
            if length.is_some_and(|n| bytes.len() > n) {
                return Err(Status::invalid_argument(
                    "unary gNMI requires exactly one message",
                ));
            }
        }
    }
    if length != Some(bytes.len()) {
        return Err(Status::invalid_argument("incomplete gNMI message"));
    }
    Ok(Request::from_parts(
        parts,
        Full::new(Bytes::from(bytes))
            .map_err(|never| match never {})
            .boxed_unsync(),
    ))
}
fn headers(request: &Request<Incoming>) -> Result<(), Status> {
    let headers = request.headers();
    if headers.len() > 64
        || headers
            .iter()
            .map(|(n, v)| n.as_str().len() + v.as_bytes().len() + 4)
            .sum::<usize>()
            > 32768
    {
        return Err(Status::resource_exhausted("HTTP header bound"));
    }
    if request.method() != hyper::Method::POST
        || request.version() != hyper::Version::HTTP_2
        || request.uri().query().is_some()
    {
        return Err(Status::invalid_argument(
            "gNMI requires POST over HTTP/2 without a query",
        ));
    }
    let mut content = headers.get_all("content-type").iter();
    let value = content
        .next()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if content.next().is_some() || !matches!(value, "application/grpc" | "application/grpc+proto") {
        return Err(Status::invalid_argument(
            "gNMI requires unique application/grpc content type",
        ));
    }
    for name in ["grpc-encoding", "grpc-accept-encoding"] {
        if headers.get_all(name).iter().count() > 1 {
            return Err(Status::invalid_argument("duplicate gRPC encoding header"));
        }
    }
    Ok(())
}
async fn serve<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
    io: T,
    ctx: Context,
    rpcs: Arc<Semaphore>,
    timeout: Duration,
    next: Arc<AtomicU32>,
) {
    let executor = OwnedExecutor::default();
    let _children = ConnectionTasks(executor.clone());
    let activity = Arc::new(ConnectionActivity::new());
    let service_activity = activity.clone();
    let service_executor = executor.clone();
    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
        let ctx = ctx.clone();
        let rpcs = rpcs.clone();
        let activity = service_activity.clone();
        let executor = service_executor.clone();
        let next = next.clone();
        async move {
            let prepare = (|| -> Result<_, Status> {
                headers(&request)?;
                if !matches!(
                    request.uri().path(),
                    "/gnmi.gNMI/Capabilities"
                        | "/gnmi.gNMI/Get"
                        | "/gnmi.gNMI/Set"
                        | "/gnmi.gNMI/Subscribe"
                ) {
                    return Err(Status::unimplemented("unknown gNMI method"));
                }
                let timeout = crate::server::grpc::streaming::timeout(request.headers(), timeout)?;
                let deadline = tokio::time::Instant::now() + timeout;
                let permit = rpcs
                    .try_acquire_owned()
                    .map_err(|_| Status::unavailable("netget: receiver at RPC capacity"))?;
                let timer = executor.deadline(deadline)?;
                let busy = activity.busy();
                let id = next
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .map_err(|_| Status::resource_exhausted("gNMI RPC identifiers exhausted"))?;
                Ok((deadline, (permit, timer, busy), id))
            })();
            let (deadline, guard, id) = match prepare {
                Ok(v) => v,
                Err(status) => return Ok::<_, Infallible>(status.into_http()),
            };
            let mut ctx = ctx;
            ctx.deadline = deadline;
            ctx.id = id;
            let call = async move {
                let stream = request.uri().path() == "/gnmi.gNMI/Subscribe";
                let request = if stream {
                    request.map(|body| {
                        body.map_err(|_| Status::invalid_argument("unreadable gNMI body"))
                            .boxed_unsync()
                    })
                } else {
                    unary(request).await?
                };
                let mut receiver = pb::g_nmi_server::GNmiServer::new(Receiver(ctx))
                    .accept_compressed(CompressionEncoding::Gzip)
                    .send_compressed(CompressionEncoding::Gzip)
                    .max_decoding_message_size(codec::MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(codec::MAX_MESSAGE_BYTES);
                match receiver.call(request).await {
                    Ok(response) => Ok::<_, Status>(response),
                    Err(never) => match never {},
                }
            };
            let response = match tokio::time::timeout_at(deadline, call).await {
                Ok(Ok(v)) => v,
                Ok(Err(error)) => error.into_http(),
                Err(_) => Status::deadline_exceeded("netget: gNMI RPC deadline").into_http(),
            };
            Ok(response.map(|body| hold_body(body, guard)))
        }
    });
    let connection = hyper::server::conn::http2::Builder::new(executor)
        .max_concurrent_streams(16)
        .initial_stream_window_size(65536)
        .initial_connection_window_size(1048576)
        .max_header_list_size(32768)
        .max_frame_size(16384)
        .serve_connection(hyper_util::rt::TokioIo::new(io), service);
    tokio::pin!(connection);
    tokio::select! {_=&mut connection=>{},_=accept_bounded::watch_idle(activity,IDLE_BETWEEN_REQUESTS_TIMEOUT)=>{}}
}
pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<std::net::SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let tls = params
        .map(|p| p.get_optional_bool("use_tls"))
        .transpose()?
        .flatten()
        .unwrap_or(super::DEFAULT_TLS);
    let cert = params
        .map(|p| p.get_optional_string("cert_file"))
        .transpose()?
        .flatten();
    let key = params
        .map(|p| p.get_optional_string("key_file"))
        .transpose()?
        .flatten();
    let timeout = params
        .map(|p| p.get_optional_u64("rpc_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(super::DEFAULT_RPC_TIMEOUT_SECS);
    anyhow::ensure!(
        (1..=3600).contains(&timeout),
        "rpc_timeout_secs must be 1..3600"
    );
    anyhow::ensure!(
        tls || (cert.is_none() && key.is_none()),
        "cert_file/key_file require use_tls"
    );
    let acceptor = if tls {
        Some(tokio_rustls::TlsAcceptor::from(
            super::tls::server(
                cert.ok_or_else(|| anyhow::anyhow!("cert_file required"))?,
                key.ok_or_else(|| anyhow::anyhow!("key_file required"))?,
            )
            .await?,
        ))
    } else {
        None
    };
    let listen = ctx
        .socket_addr()
        .ok_or_else(|| anyhow::anyhow!("gNMI requires a literal IP host and port"))?;
    let listener = crate::server::socket_helpers::create_reusable_tcp_listener(listen).await?;
    let address = listener.local_addr()?;
    let owner = ctx.state.clone();
    let server = ctx.server_id;
    let limiter = accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
    let rpcs = Arc::new(Semaphore::new(MAX_ACTIVE_RPCS));
    let next = Arc::new(AtomicU32::new(1));
    let task = tokio::spawn(async move {
        loop {
            let Ok((socket, remote, permit)) = accept_bounded::accept_bounded(
                &listener,
                &limiter,
                if tls { b"" } else { REFUSAL },
                "gNMI",
                Some(&ctx.status_tx),
            )
            .await
            else {
                break;
            };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
                .add_connection_to_server(
                    server,
                    crate::state::server::ConnectionState {
                        id,
                        remote_addr: remote,
                        local_addr: address,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: now,
                        status: crate::state::server::ConnectionStatus::Active,
                        status_changed_at: now,
                        protocol_info: crate::state::server::ProtocolConnectionInfo::empty(),
                    },
                )
                .await;
            let context = Context {
                llm: ctx.llm_client.clone(),
                state: ctx.state.clone(),
                server,
                connection: id,
                protocol: Arc::new(actions::GnmiProtocol),
                id: 0,
                deadline: tokio::time::Instant::now(),
            };
            let cleanup = ctx.state.clone();
            let acceptor = acceptor.clone();
            let rpcs = rpcs.clone();
            let next = next.clone();
            ctx.state.spawn_server_task(server,async move{
                let _permit=permit;
                if matches!(tokio::time::timeout(FIRST_BYTE_READ_TIMEOUT,socket.peek(&mut[0u8;1])).await,Ok(Ok(n)) if n>0){
                    if let Some(acceptor)=acceptor{
                        if let Ok(Ok(socket))=tokio::time::timeout(Duration::from_secs(10),acceptor.accept(socket)).await{
                            if socket.get_ref().1.alpn_protocol()==Some(b"h2"){serve(socket,context,rpcs,Duration::from_secs(timeout),next).await;}
                        }
                    }else{serve(socket,context,rpcs,Duration::from_secs(timeout),next).await;}
                }
                cleanup.remove_connection_from_server(server,id).await;
            }).await;
        }
    });
    owner.register_server_task(server, task).await;
    Ok(address)
}
