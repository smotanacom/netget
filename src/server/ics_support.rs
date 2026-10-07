//! Shared task ownership and bounded I/O for request/reply device simulators.
//! Wire formats and decisions remain in each protocol's session implementation.
use crate::{
    llm::actions::{client_trait::Client, protocol_trait::Server},
    protocol::{ConnectContext, Event, SpawnContext},
    server::connection::ConnectionId,
    state::{AccessLogOwner, ClientStatus},
};
use anyhow::{ensure, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn parameter(
    name: &str,
    kind: &str,
    description: &str,
    required: bool,
) -> crate::llm::actions::Parameter {
    crate::llm::actions::Parameter {
        name: name.into(),
        type_hint: kind.into(),
        description: description.into(),
        required,
    }
}
pub fn action(
    name: &str,
    description: &str,
    parameters: Vec<crate::llm::actions::Parameter>,
    example: Value,
) -> crate::llm::actions::ActionDefinition {
    crate::llm::actions::ActionDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
        example,
        log_template: Some(crate::protocol::LogTemplate::new().with_info(name)),
    }
}
pub fn number(v: &Value, key: &str, max: u64) -> Result<u64> {
    let n = v[key]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("{key} must be an unsigned integer"))?;
    ensure!(n <= max, "{key} exceeds {max}");
    Ok(n)
}
/// A bounded framed stream read. The header determines the whole length, validated before allocation.
pub async fn frame(
    stream: &mut TcpStream,
    header_len: usize,
    max: usize,
    length: fn(&[u8]) -> Result<usize>,
) -> Result<Vec<u8>> {
    let mut header = vec![0; header_len];
    stream.read_exact(&mut header).await?;
    let len = length(&header)?;
    ensure!(len >= header_len && len <= max, "invalid frame length");
    header.resize(len, 0);
    stream.read_exact(&mut header[header_len..]).await?;
    Ok(header)
}
#[async_trait]
pub trait DeviceSession: Send + 'static {
    async fn read(&mut self, stream: &mut TcpStream) -> Result<Vec<u8>>;
    /// Automatic transport replies or a semantic request for the handler.
    fn receive(&mut self, frame: &[u8]) -> Result<(Vec<u8>, Option<Value>)>;
    fn answer(&mut self, request: &Value, action: Option<&Value>) -> Result<Vec<u8>>;
}
#[async_trait]
pub trait ScannerSession: Send + 'static {
    async fn open(&mut self, stream: &mut TcpStream) -> Result<()>;
    async fn exchange(&mut self, stream: &mut TcpStream, action: &Value) -> Result<Value>;
    async fn idle(&mut self, stream: &mut TcpStream) -> Result<Option<Value>> {
        let mut b = [0];
        stream.peek(&mut b).await?;
        anyhow::bail!("unexpected data or peer closed")
    }
}
pub async fn spawn<S: DeviceSession, F: Fn() -> S + Send + Sync + Clone + 'static>(
    ctx: SpawnContext,
    protocol: Arc<dyn Server>,
    factory: F,
    kinds: &'static [crate::protocol::EventType],
) -> Result<SocketAddr> {
    let listener =
        crate::server::socket_helpers::create_reusable_tcp_listener(ctx.legacy_listen_addr())
            .await?;
    spawn_accept_bounded(ctx, protocol, factory, kinds, listener).await
}
pub async fn spawn_accept_bounded<
    S: DeviceSession,
    F: Fn() -> S + Send + Sync + Clone + 'static,
>(
    ctx: SpawnContext,
    protocol: Arc<dyn Server>,
    factory: F,
    kinds: &'static [crate::protocol::EventType],
    listener: tokio::net::TcpListener,
) -> Result<SocketAddr> {
    let local = listener.local_addr()?;
    let state = ctx.state.clone();
    let sid = ctx.server_id;
    let task = tokio::spawn(async move {
        let cap = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let Ok((mut stream, peer, permit)) = crate::server::accept_bounded::accept_bounded(
                &listener,
                &cap,
                b"",
                protocol.protocol_name(),
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
                    sid,
                    crate::state::server::ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: local,
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
            let mut commands =
                crate::server::peer_support::register_peer_channel(&ctx.state, sid, id.as_u32())
                    .await;
            let factory = factory.clone();
            let child = ctx.clone();
            let p = protocol.clone();
            ctx.state.spawn_server_task(sid, async move {
                let _permit = permit; let mut session = factory(); let mut first = true;
                let result: Result<()> = async {
                    loop {
                        let input = {
                            let read=tokio::time::timeout(if first {FIRST_BYTE_TIMEOUT} else {IDLE_TIMEOUT},session.read(&mut stream));
                            tokio::pin!(read);
                            loop {
                                tokio::select! {
                                    v=&mut read=>break Some(v??),
                                    Some(command)=commands.recv()=>{
                                        if command.action["type"]=="disconnect" {crate::client::command_support::reply(command,Ok(crate::state::client_handles::ClientSendOutcome::Disconnected));break None;}
                                        crate::client::command_support::reply(command,Ok(crate::state::client_handles::ClientSendOutcome::Rejected{error:"Only disconnect is valid outside a pending request".into()}));
                                    }
                                }
                            }
                        };
                        let Some(input)=input else {break;};
                        first = false;
                        child.state.update_connection_stats(sid, id, Some(input.len() as u64), None, Some(1), None).await;
                        let (automatic, request) = session.receive(&input)?;
                        let mut reply = automatic;
                        if let Some(request) = request {
                            let event = Event::new(&kinds[0], request.clone());
                            let result = crate::llm::action_helper::call_llm(&child.llm_client, &child.state, sid, Some(id), &event, p.as_ref()).await;
                            let answer = result.as_ref().ok().and_then(|r| r.protocol_results.iter().find_map(|a| if let crate::llm::ActionResult::Custom {data,..}=a {Some(data)} else {None}));
                            if answer.is_none() { crate::logging::emit::Log::new(Some(&child.status_tx)).warn(format!("{} decision=fail_closed_no_action", p.protocol_name())); }
                            reply.extend(session.answer(&request, answer)?);
                        }
                        if !reply.is_empty() { stream.write_all(&reply).await?; child.state.update_connection_stats(sid, id, None, Some(reply.len() as u64), None, Some(1)).await; }
                    } Ok(())
                }.await;
                if let Err(e) = result { crate::logging::emit::Log::new(Some(&child.status_tx)).debug(format!("{} peer ended: {e}", p.protocol_name())); }
                let _ = stream.shutdown().await;
                child.state.remove_peer_handle(sid, id.as_u32()).await;
                child.state.update_connection_status(sid, id, crate::state::server::ConnectionStatus::Closed).await;
            }).await;
        }
    });
    state.register_server_task(sid, task).await;
    Ok(local)
}
pub async fn connect<S: ScannerSession>(
    ctx: ConnectContext,
    protocol: Arc<dyn Client>,
    mut session: S,
    kinds: &'static [crate::protocol::EventType],
) -> Result<SocketAddr> {
    ensure!(!ctx.remote_addr.is_empty(), "remote_addr is required");
    let mut stream =
        tokio::time::timeout(RESPONSE_TIMEOUT, TcpStream::connect(&ctx.remote_addr)).await??;
    tokio::time::timeout(RESPONSE_TIMEOUT, session.open(&mut stream)).await??;
    let local = stream.local_addr()?;
    let mut commands =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let state = ctx.state.clone();
    let cid = ctx.client_id;
    state
        .spawn_client_task(cid, async move {
            // Handler work has its own task so an unanswered manual turn does not block injected commands.
            let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<Event>(32);
            let (actions_tx, mut actions_rx) = tokio::sync::mpsc::channel::<Value>(32);
            let event_ctx = ctx.clone();
            let p = protocol.clone();
            let turns = ctx
                .state
                .spawn_client_task(cid, async move {
                    while let Some(event) = events_rx.recv().await {
                        let instruction = event_ctx
                            .state
                            .get_instruction_for_client(cid)
                            .await
                            .unwrap_or_default();
                        let memory = event_ctx
                            .state
                            .get_memory_for_client(cid)
                            .await
                            .unwrap_or_default();
                        match crate::client::llm_budget::call_llm_for_client(
                            &event_ctx.llm_client,
                            &event_ctx.state,
                            cid.to_string(),
                            &instruction,
                            &memory,
                            Some(&event),
                            p.as_ref(),
                            &event_ctx.status_tx,
                        )
                        .await
                        {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates {
                                    event_ctx.state.set_memory_for_client(cid, memory).await;
                                }
                                for a in result.actions {
                                    if actions_tx.send(a).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => crate::logging::emit::Log::new(Some(&event_ctx.status_tx))
                                .warn(format!("{} client handler: {e}", p.protocol_name())),
                        }
                    }
                })
                .await;
            let _ = events_tx.try_send(Event::new(
                &kinds[0],
                serde_json::json!({"remote_addr":ctx.remote_addr}),
            ));
            loop {
                let (action, command) = tokio::select! {
                    Some(c) = commands.recv() => (c.action.clone(),Some(c)),
                    Some(a) = actions_rx.recv() => (a,None),
                    incoming = session.idle(&mut stream) => {match incoming {Ok(Some(response))=>{let _=events_tx.try_send(Event::new(&kinds[1],response));},Ok(None)=>{},Err(_)=>break};continue;},
                    else => break,
                };
                if action["type"] == "disconnect" {
                    if let Some(c) = command {
                        crate::client::command_support::reply(
                            c,
                            Ok(crate::state::client_handles::ClientSendOutcome::Disconnected),
                        );
                    }
                    break;
                }
                if let Err(e) = protocol.execute_action(action.clone()) {
                    if let Some(c) = command {
                        crate::client::command_support::reply(
                            c,
                            Ok(crate::state::client_handles::ClientSendOutcome::Rejected {
                                error: e.to_string(),
                            }),
                        );
                    }
                    continue;
                }
                let result = match tokio::time::timeout(
                    RESPONSE_TIMEOUT,
                    session.exchange(&mut stream, &action),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("device response deadline exceeded")),
                };
                let terminal = result.is_err();
                match result {
                    Ok(response) => {
                        ctx.state
                            .record_access_log(
                                AccessLogOwner::Client(cid.as_u32()),
                                protocol.protocol_name(),
                                None,
                                &kinds[1].id,
                                response.clone(),
                                vec![],
                            )
                            .await;
                        let _ = events_tx.try_send(Event::new(&kinds[1], response.clone()));
                        if let Some(c) = command {
                            crate::client::command_support::reply(
                                c,
                                Ok(crate::state::client_handles::ClientSendOutcome::Executed {
                                    detail: response.to_string(),
                                }),
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(c) = command {
                            crate::client::command_support::reply(c, Err(e));
                        }
                    }
                }
                if terminal {
                    break;
                }
            }
            turns.abort();
            let _ = stream.shutdown().await;
            ctx.state.remove_client_handle(cid).await;
            ctx.state
                .update_client_status(cid, ClientStatus::Disconnected)
                .await;
        })
        .await;
    Ok(local)
}

#[async_trait]
pub trait DatagramSession: Send + 'static {
    async fn exchange(
        &mut self,
        socket: &mut tokio::net::UdpSocket,
        action: &Value,
    ) -> Result<Value>;
    async fn idle(&mut self, socket: &mut tokio::net::UdpSocket) -> Result<Option<Value>> {
        let mut b = [0];
        socket.recv(&mut b).await?;
        anyhow::bail!("unexpected datagram")
    }
}
pub async fn connect_udp<S: DatagramSession>(
    ctx: ConnectContext,
    protocol: Arc<dyn Client>,
    mut session: S,
    kinds: &'static [crate::protocol::EventType],
) -> Result<SocketAddr> {
    ensure!(!ctx.remote_addr.is_empty(), "remote_addr is required");
    let remote: SocketAddr = ctx.remote_addr.parse()?;
    let mut stream = tokio::net::UdpSocket::bind(if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    stream.connect(remote).await?;
    let local = stream.local_addr()?;
    let mut commands =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let state = ctx.state.clone();
    let cid = ctx.client_id;
    state
        .spawn_client_task(cid, async move {
            // Handler work has its own task so an unanswered manual turn does not block injected commands.
            let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<Event>(32);
            let (actions_tx, mut actions_rx) = tokio::sync::mpsc::channel::<Value>(32);
            let event_ctx = ctx.clone();
            let p = protocol.clone();
            let turns = ctx
                .state
                .spawn_client_task(cid, async move {
                    while let Some(event) = events_rx.recv().await {
                        let instruction = event_ctx
                            .state
                            .get_instruction_for_client(cid)
                            .await
                            .unwrap_or_default();
                        let memory = event_ctx
                            .state
                            .get_memory_for_client(cid)
                            .await
                            .unwrap_or_default();
                        match crate::client::llm_budget::call_llm_for_client(
                            &event_ctx.llm_client,
                            &event_ctx.state,
                            cid.to_string(),
                            &instruction,
                            &memory,
                            Some(&event),
                            p.as_ref(),
                            &event_ctx.status_tx,
                        )
                        .await
                        {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates {
                                    event_ctx.state.set_memory_for_client(cid, memory).await;
                                }
                                for a in result.actions {
                                    if actions_tx.send(a).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => crate::logging::emit::Log::new(Some(&event_ctx.status_tx))
                                .warn(format!("{} client handler: {e}", p.protocol_name())),
                        }
                    }
                })
                .await;
            let _ = events_tx.try_send(Event::new(
                &kinds[0],
                serde_json::json!({"remote_addr":ctx.remote_addr}),
            ));
            loop {
                let (action, command) = tokio::select! {
                    Some(c) = commands.recv() => (c.action.clone(),Some(c)),
                    Some(a) = actions_rx.recv() => (a,None),
                    incoming = session.idle(&mut stream) => {match incoming {Ok(Some(response))=>{let _=events_tx.try_send(Event::new(&kinds[1],response));},Ok(None)=>{},Err(_)=>break};continue;},
                    else => break,
                };
                if action["type"] == "disconnect" {
                    if let Some(c) = command {
                        crate::client::command_support::reply(
                            c,
                            Ok(crate::state::client_handles::ClientSendOutcome::Disconnected),
                        );
                    }
                    break;
                }
                if let Err(e) = protocol.execute_action(action.clone()) {
                    if let Some(c) = command {
                        crate::client::command_support::reply(
                            c,
                            Ok(crate::state::client_handles::ClientSendOutcome::Rejected {
                                error: e.to_string(),
                            }),
                        );
                    }
                    continue;
                }
                let result = match tokio::time::timeout(
                    RESPONSE_TIMEOUT,
                    session.exchange(&mut stream, &action),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("device response deadline exceeded")),
                };
                let terminal = result.is_err();
                match result {
                    Ok(response) => {
                        ctx.state
                            .record_access_log(
                                AccessLogOwner::Client(cid.as_u32()),
                                protocol.protocol_name(),
                                None,
                                &kinds[1].id,
                                response.clone(),
                                vec![],
                            )
                            .await;
                        let _ = events_tx.try_send(Event::new(&kinds[1], response.clone()));
                        if let Some(c) = command {
                            crate::client::command_support::reply(
                                c,
                                Ok(crate::state::client_handles::ClientSendOutcome::Executed {
                                    detail: response.to_string(),
                                }),
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(c) = command {
                            crate::client::command_support::reply(c, Err(e));
                        }
                    }
                }
                if terminal {
                    break;
                }
            }
            turns.abort();

            ctx.state.remove_client_handle(cid).await;
            ctx.state
                .update_client_status(cid, ClientStatus::Disconnected)
                .await;
        })
        .await;
    Ok(local)
}
