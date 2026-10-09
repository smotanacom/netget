//! Clients for the classic inetd services (RFC 862-868). Each query is one exchange: a fresh
//! TCP connection or one UDP datagram. `connect` opens nothing, because connecting is itself
//! a request to Daytime, QOTD, Time and Chargen.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::ClientActionResult;
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::inetd::{actions::Service, wire};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use actions::Query;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::mpsc,
};

pub const DEFAULT_TRANSPORT: &str = "tcp";
/// How long a UDP query waits for its reply.
pub const UDP_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect(ctx: ConnectContext, service: Service) -> Result<SocketAddr> {
    let udp = match ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string("transport"))
        .transpose()?
        .flatten()
        .as_deref()
        .unwrap_or(DEFAULT_TRANSPORT)
    {
        "tcp" => false,
        "udp" => true,
        other => anyhow::bail!("transport must be tcp or udp, not {other}"),
    };
    let remote: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await
        .context("resolve the service address")?
        .next()
        .context("the service address resolved to nothing")?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        actions::ready_event(service),
        json!({"remote_addr": ctx.remote_addr, "transport": if udp { "udp" } else { "tcp" }}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
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
                actions::protocol(service),
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
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("{} client handler: {e}", service.name())),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            service,
            remote,
            udp,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx))
                    .warn(format!("{} client ended: {e}", service.name()));
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
    Ok(SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        0,
    ))
}

fn answer(injected: &mut Option<ClientCommand>, outcome: Result<ClientSendOutcome>) {
    if let Some(command) = injected.take() {
        crate::client::command_support::reply(command, outcome);
    }
}

/// Turn received bytes into the response event's data.
fn response(service: Service, transport: &str, sent: &[u8], received: &[u8]) -> Result<Value> {
    Ok(match service {
        Service::Echo => {
            let (data, encoding) = wire::encode(received);
            json!({"transport": transport, "data": data, "encoding": encoding, "bytes": received.len(), "matches": received == sent})
        }
        Service::Discard => json!({"transport": transport, "bytes": sent.len()}),
        Service::Daytime => {
            let text = String::from_utf8_lossy(received);
            json!({"transport": transport, "text": text.trim_end_matches(['\r', '\n'])})
        }
        Service::Qotd => {
            let quote = String::from_utf8_lossy(received).replace("\r\n", "\n");
            json!({"transport": transport, "quote": quote.trim_end_matches('\n')})
        }
        Service::Chargen => {
            let text = String::from_utf8_lossy(received).to_string();
            let conforms = wire::conforms_to_chargen(&text, &wire::default_charset());
            json!({"transport": transport, "text": text, "bytes": received.len(), "conforms": conforms})
        }
        Service::Time => {
            anyhow::ensure!(
                received.len() == 4,
                "a Time reply is 4 bytes, got {}",
                received.len()
            );
            let mut value =
                wire::describe_time([received[0], received[1], received[2], received[3]]);
            value["transport"] = json!(transport);
            value
        }
    })
}

/// One TCP exchange. `injected` is answered once the request is on the wire.
async fn over_tcp(
    service: Service,
    remote: SocketAddr,
    query: &Query,
    injected: &mut Option<ClientCommand>,
) -> Result<Option<Value>> {
    let mut stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(remote))
        .await
        .context("connect deadline")??;
    if !query.payload.is_empty() {
        tokio::time::timeout(wire::IO_TIMEOUT, stream.write_all(&query.payload))
            .await
            .context("write deadline")??;
    }
    answer(
        injected,
        Ok(ClientSendOutcome::Sent {
            bytes_sent: query.payload.len(),
        }),
    );
    let limit = match service {
        Service::Discard => {
            let _ = stream.shutdown().await;
            return Ok(Some(response(service, "tcp", &query.payload, &[])?));
        }
        Service::Echo => {
            stream.shutdown().await?;
            query.payload.len() as u64
        }
        Service::Chargen => query.read_bytes,
        Service::Time => 4,
        Service::Daytime | Service::Qotd => wire::MAX_STREAM_REPLY as u64,
    };
    let mut received = Vec::new();
    let mut bounded = (&mut stream).take(limit);
    tokio::time::timeout(wire::IO_TIMEOUT, bounded.read_to_end(&mut received))
        .await
        .context("reply deadline")??;
    Ok(Some(response(service, "tcp", &query.payload, &received)?))
}

/// One UDP exchange; `None` when the reply never came.
async fn over_udp(
    service: Service,
    remote: SocketAddr,
    query: &Query,
    injected: &mut Option<ClientCommand>,
) -> Result<Option<Value>> {
    let local: SocketAddr = if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let socket = UdpSocket::bind(local).await?;
    socket.connect(remote).await?;
    // RFC 864-868 ignore a request datagram's contents; one newline keeps it non-empty.
    let request: &[u8] = if query.payload.is_empty() {
        b"\n"
    } else {
        &query.payload
    };
    socket.send(request).await?;
    answer(
        injected,
        Ok(ClientSendOutcome::Sent {
            bytes_sent: request.len(),
        }),
    );
    if service == Service::Discard {
        return Ok(Some(response(service, "udp", &query.payload, &[])?));
    }
    let mut buf = vec![0u8; wire::READ_CHUNK];
    match tokio::time::timeout(UDP_REPLY_TIMEOUT, socket.recv(&mut buf)).await {
        Ok(Ok(n)) => Ok(Some(response(service, "udp", &query.payload, &buf[..n])?)),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Ok(None),
    }
}

async fn session(
    ctx: &ConnectContext,
    service: Service,
    remote: SocketAddr,
    udp: bool,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        let query = match actions::execute(service, action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                answer(&mut injected, Ok(ClientSendOutcome::Disconnected));
                return Ok(());
            }
            Ok(_) => actions::parse_query(service, &action),
            Err(e) => Err(e),
        };
        let query = match query {
            Ok(query) => query,
            Err(e) => {
                answer(
                    &mut injected,
                    Ok(ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    }),
                );
                continue;
            }
        };
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    service.name(),
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![],
                )
                .await;
        }
        let result = if udp {
            over_udp(service, remote, &query, &mut injected).await
        } else {
            over_tcp(service, remote, &query, &mut injected).await
        };
        match result {
            Ok(Some(data)) => events
                .try_send(Event::new(actions::response_event(service), data))
                .context("event queue full; consumer stalled")?,
            Ok(None) => Log::new(Some(&ctx.status_tx)).warn(format!(
                "{} query over udp got no reply within {:?}",
                service.name(),
                UDP_REPLY_TIMEOUT
            )),
            Err(e) => {
                answer(&mut injected, Err(anyhow::anyhow!(e.to_string())));
                Log::new(Some(&ctx.status_tx))
                    .warn(format!("{} query failed: {e:#}", service.name()));
            }
        }
    }
}
