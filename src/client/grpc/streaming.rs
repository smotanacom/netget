//! One responsive owner for streams, unary calls, event handlers and injected controls.
use super::{actions, Applied, Dispatch, GrpcClientData, SocketGuard};
use crate::server::grpc::stream_codec::{self, DynamicCodec};
use crate::{
    llm::actions::{client_trait::Client, protocol_trait::Protocol},
    protocol::Event,
    state::{
        client_handles::{ClientCommand, ClientSendOutcome},
        AccessLogOwner, AppState, ClientId,
    },
};
use anyhow::{ensure, Context, Result};
use futures::{
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use prost_reflect::{DynamicMessage, MethodDescriptor};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, Mutex};
use tonic::{codec::CompressionEncoding, Status};

pub(super) const DEFAULT_STREAM_TIMEOUT: u64 = 300;
pub(super) const DEFAULT_IDLE_TIMEOUT: u64 = 120;
const MAX_ACTIVE: usize = 16;
const MAX_MESSAGES: usize = 256;
const MAX_FOLLOWUP: u8 = 4;
type Handler = BoxFuture<'static, (u8, Result<Vec<Value>>)>;
type Operation = BoxFuture<'static, Completion>;
enum Completion {
    Unary {
        command: Option<ClientCommand>,
        action: Value,
        result: Result<Applied>,
        depth: u8,
    },
    Stream {
        id: u32,
        event: Event,
        depth: u8,
    },
}
struct Entry {
    sender: Option<mpsc::Sender<DynamicMessage>>,
    abort: AbortHandle,
    input: prost_reflect::MessageDescriptor,
    input_count: usize,
}
#[derive(Clone)]
pub(super) struct ContextData {
    pub id: ClientId,
    pub data: Arc<Mutex<GrpcClientData>>,
    pub state: Arc<AppState>,
    pub llm: crate::llm::OllamaClient,
    pub status: mpsc::UnboundedSender<String>,
    pub protocol: Arc<actions::GrpcClientProtocol>,
}
fn handler(ctx: ContextData, event: Event, depth: u8) -> Handler {
    async move {
        let Some(instruction) = ctx.state.get_instruction_for_client(ctx.id).await else {
            return (depth, Ok(Vec::new()));
        };
        let memory = ctx
            .state
            .get_memory_for_client(ctx.id)
            .await
            .unwrap_or_default();
        let result = super::call_llm_for_client(
            &ctx.llm,
            &ctx.state,
            ctx.id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            ctx.protocol.as_ref(),
            &ctx.status,
        )
        .await;
        (
            depth,
            match result {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        ctx.state.set_memory_for_client(ctx.id, memory).await;
                    }
                    Ok(result.actions)
                }
                Err(error) => Err(error),
            },
        )
    }
    .boxed()
}
fn ended(id: u32, code: tonic::Code, message: &str, count: usize) -> Event {
    Event::new(
        &actions::GRPC_CLIENT_STREAM_ENDED_EVENT,
        json!({
            "stream_id":id,"code":code as i32,"message":crate::utils::truncate_for_llm(message,512),"response_count":count,
        }),
    )
}
async fn headers(
    events: &mpsc::Sender<(u8, Event)>,
    depth: u8,
    id: u32,
    method: &MethodDescriptor,
) -> Result<(), Status> {
    events.send((depth, Event::new(&actions::GRPC_CLIENT_STREAM_OPENED_EVENT,json!({
        "stream_id":id,"service":method.parent_service().full_name(),"method":method.name(),
        "client_streaming":method.is_client_streaming(),"server_streaming":method.is_server_streaming(),
    })))).await.map_err(|_|Status::cancelled("client owner removed"))
}
async fn received(
    events: &mpsc::Sender<(u8, Event)>,
    depth: u8,
    id: u32,
    message: DynamicMessage,
    count: usize,
) -> Result<(), Status> {
    let response =
        stream_codec::to_json(&message).map_err(|error| {
            Status::new(
                tonic::Code::from_i32(
                    crate::server::grpc::grpc_status_for_value_failure(&error) as i32
                ),
                "response does not fit typed stream values",
            )
        })?;
    events
        .send((
            depth,
            Event::new(
                &actions::GRPC_CLIENT_STREAM_MESSAGE_EVENT,
                json!({
                    "stream_id":id,"sequence":count,"response":response,
                }),
            ),
        ))
        .await
        .map_err(|_| Status::cancelled("client owner removed"))
}
fn metadata(action: &Value) -> Result<tonic::metadata::MetadataMap> {
    let mut headers = tonic::metadata::MetadataMap::new();
    let Some(metadata) = action.get("metadata") else {
        return Ok(headers);
    };
    let metadata = metadata.as_object().context("metadata must be an object")?;
    ensure!(metadata.len() <= 16, "metadata exceeds 16 fields");
    let mut total = 0usize;
    for (key, value) in metadata {
        let value = value
            .as_str()
            .context("metadata values must be ASCII strings")?;
        ensure!(
            !key.is_empty() && key.len() <= 128 && value.len() <= 1024,
            "metadata field too long"
        );
        ensure!(
            key.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.')),
            "metadata keys must be lowercase ASCII"
        );
        ensure!(
            !key.starts_with("grpc-")
                && !key.ends_with("-bin")
                && !matches!(
                    key.as_str(),
                    "te" | "host"
                        | "content-type"
                        | "content-length"
                        | "connection"
                        | "transfer-encoding"
                ),
            "reserved or binary metadata is outside the stream subset"
        );
        total = total
            .checked_add(key.len() + value.len())
            .context("metadata size overflow")?;
        ensure!(total <= 8192, "metadata exceeds 8 KiB");
        let key: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = key.parse()?;
        let value: tonic::metadata::MetadataValue<tonic::metadata::Ascii> = value.parse()?;
        headers.insert(key, value);
    }
    Ok(headers)
}
fn stream(
    id: u32,
    method: MethodDescriptor,
    channel: tonic::transport::Channel,
    request: Option<DynamicMessage>,
    input: Option<mpsc::Receiver<DynamicMessage>>,
    metadata: tonic::metadata::MetadataMap,
    gzip: bool,
    timeout: Duration,
    events: mpsc::Sender<(u8, Event)>,
    depth: u8,
    registration: futures::future::AbortRegistration,
) -> Operation {
    async move {
        let mut count = 0usize;
        let call = async {
            let codec = DynamicCodec { encode:method.input(), decode:method.output() };
            let path:http::uri::PathAndQuery = format!("/{}/{}",method.parent_service().full_name(),method.name()).parse().map_err(|_|Status::invalid_argument("invalid method path"))?;
            let mut client = tonic::client::Grpc::new(channel).max_decoding_message_size(stream_codec::MAX_MESSAGE_BYTES)
                .max_encoding_message_size(stream_codec::MAX_MESSAGE_BYTES).accept_compressed(CompressionEncoding::Gzip);
            if gzip { client = client.send_compressed(CompressionEncoding::Gzip); }
            client.ready().await.map_err(|_|Status::unavailable("gRPC channel is not ready"))?;
            if !method.is_client_streaming() {
                let mut request = tonic::Request::new(request.ok_or_else(||Status::invalid_argument("server-streaming requires request"))?);
                *request.metadata_mut() = metadata;
                request.set_timeout(timeout);
                let mut response = client.server_streaming(request,path,codec).await?.into_inner();
                headers(&events,depth,id,&method).await?;
                while let Some(message) = response.message().await? {
                    if count >= MAX_MESSAGES { return Err(Status::resource_exhausted("stream exceeds 256 response messages")); }
                    count += 1;
                    received(&events,depth,id,message,count).await?;
                }
            } else {
                let input = input.ok_or_else(||Status::internal("stream request receiver unavailable"))?;
                let event_tx = events.clone();
                let input = futures::stream::unfold((input,0usize),move |(mut input,sequence)| {
                    let events = event_tx.clone();
                    async move {
                        let message = input.recv().await?;
                        events.send((depth,Event::new(&actions::GRPC_CLIENT_STREAM_INPUT_READY_EVENT,json!({
                            "stream_id":id,"input_sequence":sequence+1,"queue_capacity":1,
                        })))).await.ok()?;
                        Some((message,(input,sequence+1)))
                    }
                });
                let mut request = tonic::Request::new(Box::pin(input));
                *request.metadata_mut() = metadata;
                request.set_timeout(timeout);
                if method.is_server_streaming() {
                    let mut response = client.streaming(request,path,codec).await?.into_inner();
                    headers(&events,depth,id,&method).await?;
                    while let Some(message) = response.message().await? {
                        if count >= MAX_MESSAGES { return Err(Status::resource_exhausted("stream exceeds 256 response messages")); }
                        count += 1;
                        received(&events,depth,id,message,count).await?;
                    }
                } else {
                    let response = client.client_streaming(request,path,codec).await?.into_inner();
                    headers(&events,depth,id,&method).await?;
                    count = 1;
                    received(&events,depth,id,response,count).await?;
                }
            }
            Ok::<(),Status>(())
        };
        let result = Abortable::new(tokio::time::timeout(timeout,call),registration).await;
        let event = match result {
            Ok(Ok(Ok(()))) => ended(id,tonic::Code::Ok,"",count),
            Ok(Ok(Err(status))) => ended(id,status.code(),status.message(),count),
            Ok(Err(_)) => ended(id,tonic::Code::DeadlineExceeded,"stream deadline exceeded",count),
            Err(_) => ended(id,tonic::Code::Cancelled,"stream cancelled",count),
        };
        Completion::Stream {id,event,depth}
    }.boxed()
}
async fn report(ctx: &ContextData, command: ClientCommand, outcome: Result<ClientSendOutcome>) {
    let result = match &outcome {
        Ok(value) => serde_json::to_value(value).unwrap_or(Value::Null),
        Err(error) => json!({"error":error.to_string()}),
    };
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.id.as_u32()),
            ctx.protocol.protocol_name(),
            None,
            "injected_action",
            command.action.clone(),
            vec![result],
        )
        .await;
    crate::client::command_support::reply(command, outcome);
}
struct Owner {
    ctx: ContextData,
    entries: HashMap<u32, Entry>,
    used: HashSet<u32>,
    operations: FuturesUnordered<Operation>,
    handlers: FuturesUnordered<Handler>,
    pending: VecDeque<(u8, Event)>,
    event_tx: mpsc::Sender<(u8, Event)>,
    timeout: Duration,
}
impl Owner {
    async fn action(&mut self, action: Value, command: Option<ClientCommand>, depth: u8) -> bool {
        let kind = action["type"].as_str().unwrap_or("");
        if kind == "disconnect" {
            if let Some(command) = command {
                report(&self.ctx, command, Ok(ClientSendOutcome::Disconnected)).await;
            }
            return true;
        }
        let result = self.ctx.protocol.execute_action(action.clone());
        if let Err(error) = result {
            if let Some(command) = command {
                report(
                    &self.ctx,
                    command,
                    Ok(ClientSendOutcome::Rejected {
                        error: error.to_string(),
                    }),
                )
                .await;
            }
            return false;
        }
        if kind.starts_with("grpc_stream_") {
            let outcome =
                self.control(&action, depth)
                    .await
                    .map(|()| ClientSendOutcome::Executed {
                        detail: "stream control queued".into(),
                    });
            if let Some(command) = command {
                report(&self.ctx, command, outcome).await;
            } else if let Err(error) = outcome {
                crate::logging::emit::Log::new(Some(&self.ctx.status))
                    .warn(format!("gRPC stream control refused: {error}"));
            }
            return false;
        }
        if self.operations.len() + self.pending.len() >= MAX_ACTIVE
            || self.handlers.len() >= MAX_ACTIVE
        {
            if let Some(command) = command {
                report(
                    &self.ctx,
                    command,
                    Ok(ClientSendOutcome::Rejected {
                        error: "client at operation capacity".into(),
                    }),
                )
                .await;
            }
            return false;
        }
        let ctx = self.ctx.clone();
        let data = result.unwrap();
        self.operations.push(
            async move {
                let result = super::apply_grpc_action(
                    ctx.id,
                    data,
                    ctx.data.clone(),
                    &ctx.state,
                    &ctx.llm,
                    &ctx.status,
                    &ctx.protocol,
                    Dispatch::Defer,
                )
                .await;
                Completion::Unary {
                    command,
                    action,
                    result,
                    depth,
                }
            }
            .boxed(),
        );
        false
    }
    async fn control(&mut self, action: &Value, depth: u8) -> Result<()> {
        let id = action["stream_id"]
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .filter(|id| *id != 0)
            .context("stream_id must be 1..4294967295")?;
        match action["type"].as_str() {
            Some("grpc_stream_start") => {
                ensure!(depth <= MAX_FOLLOWUP, "stream follow-up exceeds depth 4");
                ensure!(
                    self.operations.len() + self.pending.len() < MAX_ACTIVE
                        && self.handlers.len() < MAX_ACTIVE,
                    "client at operation/handler capacity"
                );
                ensure!(
                    self.used.len() < MAX_MESSAGES && !self.used.contains(&id),
                    "stream id reused or session exceeds 256 streams"
                );
                let service = action["service"].as_str().context("service required")?;
                let name = action["method"].as_str().context("method required")?;
                ensure!(
                    service.len() <= 1024 && name.len() <= 256,
                    "method name too long"
                );
                let (method, channel) = {
                    let data = self.ctx.data.lock().await;
                    (
                        data.descriptor_pool
                            .get_service_by_name(service)
                            .and_then(|service| {
                                service.methods().find(|method| method.name() == name)
                            })
                            .context("method absent from schema")?,
                        data.channel.clone(),
                    )
                };
                ensure!(
                    method.is_client_streaming() || method.is_server_streaming(),
                    "method is unary; use call_grpc_method"
                );
                stream_codec::check_descriptor(&method.input())?;
                stream_codec::check_descriptor(&method.output())?;
                let mut request = action
                    .get("request")
                    .map(|request| stream_codec::from_json(request, &method.input()))
                    .transpose()?;
                ensure!(
                    method.is_client_streaming() || request.is_some(),
                    "server-streaming requires request"
                );
                let input_count = usize::from(request.is_some());
                let metadata = metadata(action)?;
                let gzip = action
                    .get("gzip")
                    .map(|value| value.as_bool().context("gzip must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                let (sender, input) = if method.is_client_streaming() {
                    let (sender, input) = mpsc::channel(1);
                    if let Some(request) = request.take() {
                        sender.try_send(request)?;
                    }
                    (Some(sender), Some(input))
                } else {
                    (None, None)
                };
                let (abort, registration) = AbortHandle::new_pair();
                self.entries.insert(
                    id,
                    Entry {
                        sender,
                        abort,
                        input: method.input(),
                        input_count,
                    },
                );
                self.used.insert(id);
                self.operations.push(stream(
                    id,
                    method,
                    channel,
                    request,
                    input,
                    metadata,
                    gzip,
                    self.timeout,
                    self.event_tx.clone(),
                    depth,
                    registration,
                ));
            }
            Some("grpc_stream_send") => {
                let entry = self
                    .entries
                    .get_mut(&id)
                    .context("stream is no longer active")?;
                ensure!(
                    entry.input_count < MAX_MESSAGES,
                    "stream exceeds 256 input messages"
                );
                let sender = entry
                    .sender
                    .as_ref()
                    .context("stream input is already half-closed")?;
                let message = stream_codec::from_json(&action["message"], &entry.input)?;
                sender
                    .try_send(message)
                    .map_err(|_| anyhow::anyhow!("stream input queue full or closed"))?;
                entry.input_count += 1;
            }
            Some("grpc_stream_finish") => {
                let entry = self
                    .entries
                    .get_mut(&id)
                    .context("stream is no longer active")?;
                ensure!(
                    entry.sender.take().is_some(),
                    "stream input is already half-closed"
                );
            }
            Some("grpc_stream_cancel") => {
                self.entries
                    .get(&id)
                    .context("stream is no longer active")?
                    .abort
                    .abort();
            }
            _ => anyhow::bail!("unknown stream control"),
        }
        Ok(())
    }
}
pub(super) async fn run(
    ctx: ContextData,
    _socket: SocketGuard,
    mut commands: mpsc::Receiver<ClientCommand>,
    mut automatic: mpsc::Receiver<Value>,
    connected: Event,
    timeout: Duration,
    idle: Duration,
) {
    let (events, mut event_rx) = mpsc::channel(MAX_ACTIVE);
    let mut owner = Owner {
        ctx: ctx.clone(),
        entries: HashMap::new(),
        used: HashSet::new(),
        operations: FuturesUnordered::new(),
        handlers: FuturesUnordered::new(),
        pending: VecDeque::new(),
        event_tx: events,
        timeout,
    };
    owner
        .handlers
        .push(handler(ctx.clone(), connected, u8::MAX));
    let mut activity = tokio::time::Instant::now();
    loop {
        while owner.handlers.len() < MAX_ACTIVE {
            let Some((depth, event)) = owner.pending.pop_front() else {
                break;
            };
            owner.handlers.push(handler(ctx.clone(), event, depth));
        }
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {break};
                activity = tokio::time::Instant::now();
                if owner.action(command.action.clone(),Some(command),0).await {break};
            }
            Some(action) = automatic.recv() => {
                activity = tokio::time::Instant::now();
                if owner.action(action,None,0).await {break};
            }
            Some((depth,event)) = event_rx.recv(), if owner.handlers.len() < MAX_ACTIVE => {
                activity = tokio::time::Instant::now();
                owner.handlers.push(handler(ctx.clone(),event,depth));
            }
            Some(completion) = owner.operations.next(), if !owner.operations.is_empty() => {
                activity = tokio::time::Instant::now();
                match completion {
                    Completion::Stream {id,event,depth} => {
                        owner.entries.remove(&id);
                        owner.pending.push_back((depth,event));
                    }
                    Completion::Unary {command,action,result,depth} => {
                        let (outcome,event,stop) = match result {
                            Ok(Applied::Sent {bytes_sent,pending_notify}) => (Ok(ClientSendOutcome::Sent {bytes_sent}),pending_notify,false),
                            Ok(Applied::Ran(detail)) => (Ok(ClientSendOutcome::Executed {detail}),None,false),
                            Ok(Applied::Disconnect) => (Ok(ClientSendOutcome::Disconnected),None,true),
                            Err(error) => (Err(error),None,false),
                        };
                        if let Some(command) = command {report(&ctx,command,outcome).await;}
                        else if let Err(error) = outcome {crate::logging::emit::Log::new(Some(&ctx.status)).warn(format!("gRPC action {} failed: {error}",action["type"]));}
                        if let Some(data) = event { owner.pending.push_back((depth,Event::new(&actions::GRPC_CLIENT_RESPONSE_RECEIVED_EVENT,data))); }
                        if stop {break;}
                    }
                }
            }
            Some((depth,result)) = owner.handlers.next(), if !owner.handlers.is_empty() => {
                activity = tokio::time::Instant::now();
                match result {
                    Ok(actions) if actions.len() <= MAX_ACTIVE => {
                        let next = if depth == u8::MAX {0} else {depth.saturating_add(1)};
                        let mut stop = false;
                        for action in actions {
                            if depth != u8::MAX && next > MAX_FOLLOWUP && !matches!(action["type"].as_str(),Some("disconnect"|"grpc_stream_send"|"grpc_stream_finish"|"grpc_stream_cancel")) {continue;}
                            if owner.action(action,None,next).await {stop=true;break;}
                        }
                        if stop {break;}
                    }
                    Ok(_) => crate::logging::emit::Log::new(Some(&ctx.status)).warn("gRPC handler exceeds 16 actions"),
                    Err(error) => crate::logging::emit::Log::new(Some(&ctx.status)).warn(format!("gRPC handler failed: {error}")),
                }
            }
            _ = tokio::time::sleep_until(activity+idle), if owner.operations.is_empty() && owner.handlers.is_empty() && owner.pending.is_empty() => break,
        }
    }
    ctx.state.remove_client_handle(ctx.id).await;
    ctx.state
        .update_client_status(ctx.id, crate::state::ClientStatus::Disconnected)
        .await;
    let _ = ctx.status.send("__UPDATE_UI__".into());
}
