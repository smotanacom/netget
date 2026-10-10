//! Apache Pulsar broker (server role). Rust owns the connection handshake, lookups, producers,
//! subscriptions, flow control, delivery and acknowledgement (`broker.rs`); the model decides
//! which producers and subscriptions to allow and whether each message is accepted.
pub mod actions;
pub mod broker;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::Result;
use broker::Broker;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use wire::{command_type as t, server_error as se, BaseCommand};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
/// Producers and consumers one connection may hold.
pub const MAX_HANDLES: usize = 256;
/// Frames waiting for one connection's writer.
pub const WRITE_QUEUE: usize = 1024;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let idle = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&idle),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    let idle = Duration::from_secs(idle);
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Pulsar broker listening on {addr}"));
    let broker = Arc::new(Broker::default());
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept =
        tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                let (socket, peer, permit) =
                    match accept_bounded(&listener, &limiter, b"", "Pulsar", Some(&ctx.status_tx))
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
                let broker = broker.clone();
                ctx.state
                    .spawn_server_task(server_id, async move {
                        let _permit = permit;
                        if let Err(e) = session(&child, &broker, id, socket, peer, idle).await {
                            Log::new(Some(&child.status_tx))
                                .warn(format!("Pulsar connection {id} ended: {e:#}"));
                        }
                        broker.drop_connection(u64::from(id.as_u32()));
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
    let summary = format!("Pulsar connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The model's verdict on an event, and the messages it publishes beside it.
enum Verdict {
    Accept,
    Reject(String),
    /// No usable answer: refuse, saying only this.
    Failed(&'static str),
}

async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> (Verdict, Vec<Value>) {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::PulsarProtocol,
    )
    .await
    {
        Ok(r) if r.failures.is_empty() => r,
        Ok(_) => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            return (
                Verdict::Failed(crate::utils::WireFailure::Unavailable.text()),
                Vec::new(),
            );
        }
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return (
                Verdict::Failed(crate::utils::wire_failure_text(&e)),
                Vec::new(),
            );
        }
    };
    let mut stack = result.protocol_results;
    let mut ordered = Vec::new();
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { data, .. } => ordered.push(data),
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    ordered.reverse();
    let mut verdicts = Vec::new();
    let mut publish = Vec::new();
    for a in ordered {
        match a["type"].as_str().unwrap_or_default() {
            actions::ACCEPT => verdicts.push(Verdict::Accept),
            actions::REJECT => verdicts.push(Verdict::Reject(
                a["message"].as_str().unwrap_or("rejected").to_string(),
            )),
            _ => publish.push(a),
        }
    }
    let verdict = match verdicts.len() {
        0 => {
            outcome(ctx, id, operation, "model_silent");
            Verdict::Failed(crate::utils::WireFailure::Unavailable.text())
        }
        1 => {
            let v = verdicts.pop().unwrap_or(Verdict::Failed(""));
            outcome(
                ctx,
                id,
                operation,
                if matches!(v, Verdict::Accept) {
                    "model_answer"
                } else {
                    "model_reject"
                },
            );
            v
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Verdict::Failed(crate::utils::WireFailure::Unavailable.text())
        }
    };
    (verdict, publish)
}

/// Publish the model's own messages, logging any it got wrong.
fn publish_extras(ctx: &SpawnContext, broker: &Broker, extras: &[Value]) {
    for a in extras {
        let r = (|| -> Result<u64> {
            let topic = wire::full_topic(a["topic"].as_str().unwrap_or_default())?;
            let payload = wire::bytes(
                a["payload"].as_str().unwrap_or_default(),
                a["encoding"].as_str(),
            )?;
            let properties = a["properties"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| wire::KeyValue {
                            key: k.clone(),
                            value: v.as_str().unwrap_or_default().to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let entry = wire::Entry {
                properties,
                key: a["key"].as_str().map(str::to_string),
                event_time: None,
                sequence_id: 0,
                payload,
            };
            Ok(broker.publish(&topic, "netget", vec![entry]).entry_id)
        })();
        let log = Log::new(Some(&ctx.status_tx));
        match r {
            Ok(entry) => log.info(format!(
                "Pulsar published {} as netget (entry {entry})",
                a["topic"]
            )),
            Err(e) => log.warn(format!("Pulsar {} not published: {e:#}", a["topic"])),
        }
    }
}

fn error(request_id: u64, code: i32, message: &str) -> Vec<u8> {
    let mut c = BaseCommand::of(t::ERROR);
    c.error = Some(wire::CommandError {
        request_id,
        error: code,
        message: message.to_string(),
    });
    wire::simple(&c)
}

fn success(request_id: u64) -> Vec<u8> {
    let mut c = BaseCommand::of(t::SUCCESS);
    c.success = Some(wire::CommandSuccess { request_id });
    wire::simple(&c)
}

fn send_error(producer_id: u64, sequence_id: u64, code: i32, message: &str) -> Vec<u8> {
    let mut c = BaseCommand::of(t::SEND_ERROR);
    c.send_error = Some(wire::CommandSendError {
        producer_id,
        sequence_id,
        error: code,
        message: message.to_string(),
    });
    wire::simple(&c)
}

/// How to refuse one message so the client fails that send and goes on. Pulsar has no single
/// answer: the Java client fails a send refused with NotAllowedError, but the C++ library (and
/// the Python and Node clients built on it) treats every SendError except ChecksumError as a
/// broken connection, reconnects at once and sends the message again, forever. ChecksumError
/// is the one refusal it fails the send with, so C++ clients get that, with the model's reason
/// as its message.
pub fn refusal_code(client_version: &str) -> i32 {
    if client_version.starts_with("Pulsar-CPP") {
        se::CHECKSUM_ERROR
    } else {
        se::NOT_ALLOWED_ERROR
    }
}

const ACCESS_MODES: [&str; 4] = [
    "Shared",
    "Exclusive",
    "WaitForExclusive",
    "ExclusiveWithFencing",
];

struct Conn {
    connected: bool,
    /// The ServerError a refused message is answered with (see `refusal_code`).
    refusal: i32,
    /// producer id → (topic, producer name)
    producers: HashMap<u64, (String, String)>,
    /// consumer ids this connection holds
    consumers: HashMap<u64, ()>,
}

async fn session(
    ctx: &SpawnContext,
    broker: &Arc<Broker>,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    idle: Duration,
) -> Result<()> {
    let local = socket.local_addr()?;
    let (mut reader, mut writer) = tokio::io::split(socket);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
    let stats = ctx.clone();
    ctx.state
        .spawn_server_task(ctx.server_id, async move {
            while let Some(frame) = rx.recv().await {
                if tokio::time::timeout(wire::IO_TIMEOUT, writer.write_all(&frame))
                    .await
                    .map(|r| r.is_err())
                    .unwrap_or(true)
                {
                    break;
                }
                stats
                    .state
                    .update_connection_stats(
                        stats.server_id,
                        id,
                        None,
                        Some(frame.len() as u64),
                        None,
                        Some(1),
                    )
                    .await;
            }
            let _ = writer.shutdown().await;
        })
        .await;
    let log = Log::new(Some(&ctx.status_tx));
    let mut conn = Conn {
        connected: false,
        refusal: se::NOT_ALLOWED_ERROR,
        producers: HashMap::new(),
        consumers: HashMap::new(),
    };
    let conn_key = u64::from(id.as_u32());
    while let Some((cmd, payload)) = wire::read_frame(&mut reader, idle).await? {
        ctx.state
            .update_connection_stats(ctx.server_id, id, None, None, Some(1), None)
            .await;
        if !conn.connected && cmd.r#type != t::CONNECT {
            anyhow::bail!("command {} before CONNECT", cmd.r#type);
        }
        let reply: Vec<u8> = match cmd.r#type {
            t::CONNECT => {
                let c = cmd.connect.clone().unwrap_or_default();
                anyhow::ensure!(!conn.connected, "a second CONNECT");
                conn.connected = true;
                conn.refusal = refusal_code(&c.client_version);
                log.info(format!(
                    "Pulsar connection {id}: {} protocol v{}{}",
                    c.client_version,
                    c.protocol_version.unwrap_or(0),
                    c.auth_method_name
                        .as_deref()
                        .filter(|m| *m != "none")
                        .map(|m| format!(", auth {m} (not checked)"))
                        .unwrap_or_default()
                ));
                let mut r = BaseCommand::of(t::CONNECTED);
                r.connected = Some(wire::CommandConnected {
                    server_version: concat!("NetGet/", env!("CARGO_PKG_VERSION")).into(),
                    protocol_version: Some(
                        c.protocol_version.unwrap_or(0).min(wire::PROTOCOL_VERSION),
                    ),
                    max_message_size: Some(wire::MAX_FRAME as i32),
                });
                wire::simple(&r)
            }
            t::PING => wire::simple(&BaseCommand {
                pong: Some(wire::CommandPong {}),
                ..BaseCommand::of(t::PONG)
            }),
            t::PONG => continue,
            t::PARTITIONED_METADATA => {
                let m = cmd.partition_metadata.unwrap_or_default();
                let mut r = BaseCommand::of(t::PARTITIONED_METADATA_RESPONSE);
                r.partition_metadata_response = Some(match wire::full_topic(&m.topic) {
                    Ok(_) => wire::CommandPartitionedTopicMetadataResponse {
                        partitions: Some(0),
                        request_id: m.request_id,
                        response: Some(0),
                        ..Default::default()
                    },
                    Err(e) => wire::CommandPartitionedTopicMetadataResponse {
                        request_id: m.request_id,
                        response: Some(1),
                        error: Some(se::INVALID_TOPIC_NAME),
                        message: Some(e.to_string()),
                        ..Default::default()
                    },
                });
                wire::simple(&r)
            }
            t::LOOKUP => {
                let l = cmd.lookup_topic.unwrap_or_default();
                let mut r = BaseCommand::of(t::LOOKUP_RESPONSE);
                r.lookup_topic_response = Some(match wire::full_topic(&l.topic) {
                    Ok(_) => wire::CommandLookupTopicResponse {
                        broker_service_url: Some(format!("pulsar://{local}")),
                        response: Some(1),
                        request_id: l.request_id,
                        authoritative: Some(true),
                        proxy_through_service_url: Some(false),
                        ..Default::default()
                    },
                    Err(e) => wire::CommandLookupTopicResponse {
                        response: Some(2),
                        request_id: l.request_id,
                        error: Some(se::INVALID_TOPIC_NAME),
                        message: Some(e.to_string()),
                        ..Default::default()
                    },
                });
                wire::simple(&r)
            }
            t::PRODUCER => {
                let p = cmd.producer.unwrap_or_default();
                let topic = match wire::full_topic(&p.topic) {
                    Ok(t) => t,
                    Err(e) => {
                        tx.send(error(p.request_id, se::INVALID_TOPIC_NAME, &e.to_string()))
                            .await?;
                        continue;
                    }
                };
                if conn.producers.len() + conn.consumers.len() >= MAX_HANDLES {
                    tx.send(error(
                        p.request_id,
                        se::TOO_MANY_REQUESTS,
                        "too many producers and consumers on one connection",
                    ))
                    .await?;
                    continue;
                }
                if p.encrypted == Some(true) {
                    tx.send(error(
                        p.request_id,
                        se::NOT_ALLOWED_ERROR,
                        "encrypted producers are not supported",
                    ))
                    .await?;
                    continue;
                }
                let name = p
                    .producer_name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| broker.producer_name());
                let mode = ACCESS_MODES
                    .get(p.producer_access_mode.unwrap_or(0) as usize)
                    .copied()
                    .unwrap_or("Shared");
                let event = Event::new(
                    &actions::PRODUCER_EVENT,
                    json!({"topic": topic, "producer_name": name, "access_mode": mode, "remote_addr": peer.to_string()}),
                );
                let (verdict, extras) = ask(ctx, id, event, "producer").await;
                publish_extras(ctx, broker, &extras);
                match verdict {
                    Verdict::Accept => {
                        conn.producers.insert(p.producer_id, (topic, name.clone()));
                        let mut r = BaseCommand::of(t::PRODUCER_SUCCESS);
                        r.producer_success = Some(wire::CommandProducerSuccess {
                            request_id: p.request_id,
                            producer_name: name,
                            last_sequence_id: Some(-1),
                            schema_version: Some(Vec::new()),
                            producer_ready: Some(true),
                        });
                        wire::simple(&r)
                    }
                    Verdict::Reject(why) => error(p.request_id, se::AUTHORIZATION_ERROR, &why),
                    Verdict::Failed(why) => error(p.request_id, se::SERVICE_NOT_READY, why),
                }
            }
            t::SEND => {
                let s = cmd.send.unwrap_or_default();
                let Some((topic, name)) = conn.producers.get(&s.producer_id).cloned() else {
                    tx.send(send_error(
                        s.producer_id,
                        s.sequence_id,
                        se::NOT_ALLOWED_ERROR,
                        "no such producer",
                    ))
                    .await?;
                    continue;
                };
                let (meta, body) = match payload {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        tx.send(send_error(
                            s.producer_id,
                            s.sequence_id,
                            se::NOT_ALLOWED_ERROR,
                            "SEND without a payload",
                        ))
                        .await?;
                        continue;
                    }
                    Err(wire::PayloadError::Checksum) => {
                        log.warn(format!(
                            "Pulsar connection {id}: checksum mismatch on {topic} seq {}",
                            s.sequence_id
                        ));
                        tx.send(send_error(
                            s.producer_id,
                            s.sequence_id,
                            se::CHECKSUM_ERROR,
                            "checksum mismatch",
                        ))
                        .await?;
                        continue;
                    }
                    Err(wire::PayloadError::Malformed(why)) => {
                        tx.send(send_error(
                            s.producer_id,
                            s.sequence_id,
                            se::NOT_ALLOWED_ERROR,
                            &why,
                        ))
                        .await?;
                        continue;
                    }
                };
                // C++-based clients treat any SendError but ChecksumError as a broken
                // connection: they reconnect and send the same message again. Answer that
                // from memory rather than asking the model each time.
                if let Some(why) = broker.refused(&topic, &name, s.sequence_id) {
                    outcome(ctx, id, "message", "remembered_refusal");
                    tx.send(send_error(s.producer_id, s.sequence_id, conn.refusal, &why))
                        .await?;
                    continue;
                }
                let entries = match wire::entries(&meta, &body) {
                    Ok(e) => e,
                    Err(e) => {
                        tx.send(send_error(
                            s.producer_id,
                            s.sequence_id,
                            se::NOT_ALLOWED_ERROR,
                            &e.to_string(),
                        ))
                        .await?;
                        continue;
                    }
                };
                // Each message of a batch is decided on its own; the batch is accepted only
                // if every one of its messages is.
                let mut refusal: Option<(i32, String)> = None;
                let mut extras_all = Vec::new();
                for e in &entries {
                    let (text, encoding) = wire::show(&e.payload);
                    let props: serde_json::Map<String, Value> = e
                        .properties
                        .iter()
                        .map(|kv| (kv.key.clone(), json!(kv.value)))
                        .collect();
                    let event = Event::new(
                        &actions::MESSAGE_EVENT,
                        json!({"topic": topic, "producer_name": name, "sequence_id": e.sequence_id,
                               "payload": text, "encoding": encoding, "properties": props, "key": e.key,
                               "event_time": e.event_time, "remote_addr": peer.to_string()}),
                    );
                    let (verdict, extras) = ask(ctx, id, event, "message").await;
                    extras_all.extend(extras);
                    match verdict {
                        Verdict::Accept => {}
                        Verdict::Reject(why) => {
                            refusal = Some((conn.refusal, why));
                            break;
                        }
                        Verdict::Failed(why) => {
                            refusal = Some((conn.refusal, why.to_string()));
                            break;
                        }
                    }
                }
                let reply = match refusal {
                    Some((code, why)) => {
                        broker.remember_refusal(&topic, &name, s.sequence_id, &why);
                        send_error(s.producer_id, s.sequence_id, code, &why)
                    }
                    None => {
                        let last = broker.publish(&topic, &name, entries);
                        let mut r = BaseCommand::of(t::SEND_RECEIPT);
                        r.send_receipt = Some(wire::CommandSendReceipt {
                            producer_id: s.producer_id,
                            sequence_id: s.sequence_id,
                            message_id: Some(last),
                            highest_sequence_id: s.highest_sequence_id,
                        });
                        wire::simple(&r)
                    }
                };
                // The model's own messages follow the one it was answering.
                tx.send(reply).await?;
                publish_extras(ctx, broker, &extras_all);
                continue;
            }
            t::SUBSCRIBE => {
                let s = cmd.subscribe.unwrap_or_default();
                let topic = match wire::full_topic(&s.topic) {
                    Ok(t) => t,
                    Err(e) => {
                        tx.send(error(s.request_id, se::INVALID_TOPIC_NAME, &e.to_string()))
                            .await?;
                        continue;
                    }
                };
                if conn.producers.len() + conn.consumers.len() >= MAX_HANDLES {
                    tx.send(error(
                        s.request_id,
                        se::TOO_MANY_REQUESTS,
                        "too many producers and consumers on one connection",
                    ))
                    .await?;
                    continue;
                }
                if s.subscription.is_empty() || s.subscription.len() > 256 {
                    tx.send(error(
                        s.request_id,
                        se::SUBSCRIPTION_NOT_FOUND,
                        "subscription name must be 1-256 bytes",
                    ))
                    .await?;
                    continue;
                }
                let sub_type = wire::SUB_TYPES
                    .get(s.sub_type as usize)
                    .copied()
                    .unwrap_or("Exclusive");
                if let Err(why) = broker.can_subscribe(&topic, &s.subscription, s.sub_type) {
                    tx.send(error(s.request_id, se::CONSUMER_BUSY, &why))
                        .await?;
                    continue;
                }
                let event = Event::new(
                    &actions::SUBSCRIBE_EVENT,
                    json!({"topic": topic, "subscription": s.subscription, "sub_type": sub_type,
                           "consumer_name": s.consumer_name, "remote_addr": peer.to_string()}),
                );
                let (verdict, extras) = ask(ctx, id, event, "subscribe").await;
                let r = match verdict {
                    Verdict::Accept => match broker.subscribe(
                        &topic,
                        &s.subscription,
                        s.sub_type,
                        conn_key,
                        s.consumer_id,
                        tx.clone(),
                    ) {
                        Ok(()) => {
                            conn.consumers.insert(s.consumer_id, ());
                            success(s.request_id)
                        }
                        Err(why) => error(s.request_id, se::CONSUMER_BUSY, &why),
                    },
                    Verdict::Reject(why) => error(s.request_id, se::AUTHORIZATION_ERROR, &why),
                    Verdict::Failed(why) => error(s.request_id, se::SERVICE_NOT_READY, why),
                };
                tx.send(r).await?;
                publish_extras(ctx, broker, &extras);
                continue;
            }
            t::FLOW => {
                let f = cmd.flow.unwrap_or_default();
                broker.flow(conn_key, f.consumer_id, f.message_permits);
                continue;
            }
            t::ACK => {
                let a = cmd.ack.unwrap_or_default();
                broker.ack(conn_key, a.consumer_id, a.ack_type == 1, &a.message_id);
                match a.request_id {
                    Some(request_id) => {
                        let mut r = BaseCommand::of(38);
                        r.ack_response = Some(wire::CommandAckResponse {
                            consumer_id: a.consumer_id,
                            request_id: Some(request_id),
                        });
                        wire::simple(&r)
                    }
                    None => continue,
                }
            }
            t::REDELIVER_UNACKNOWLEDGED_MESSAGES => {
                let r = cmd.redeliver_unacknowledged_messages.unwrap_or_default();
                broker.redeliver(conn_key, r.consumer_id, &r.message_ids);
                continue;
            }
            t::UNSUBSCRIBE => {
                let u = cmd.unsubscribe.unwrap_or_default();
                match broker.unsubscribe(conn_key, u.consumer_id) {
                    Ok(()) => {
                        conn.consumers.remove(&u.consumer_id);
                        success(u.request_id)
                    }
                    Err(why) => error(u.request_id, se::CONSUMER_BUSY, &why),
                }
            }
            t::CLOSE_CONSUMER => {
                let c = cmd.close_consumer.unwrap_or_default();
                conn.consumers.remove(&c.consumer_id);
                broker.close_consumer(conn_key, c.consumer_id);
                success(c.request_id)
            }
            t::CLOSE_PRODUCER => {
                let c = cmd.close_producer.unwrap_or_default();
                conn.producers.remove(&c.producer_id);
                success(c.request_id)
            }
            t::GET_LAST_MESSAGE_ID => {
                let g = cmd.get_last_message_id.unwrap_or_default();
                match broker.last_message_id(conn_key, g.consumer_id) {
                    Some(last) => {
                        let mut r = BaseCommand::of(t::GET_LAST_MESSAGE_ID_RESPONSE);
                        r.get_last_message_id_response =
                            Some(wire::CommandGetLastMessageIdResponse {
                                last_message_id: last,
                                request_id: g.request_id,
                            });
                        wire::simple(&r)
                    }
                    None => error(g.request_id, se::CONSUMER_NOT_FOUND, "no such consumer"),
                }
            }
            t::GET_SCHEMA => {
                let g = cmd.get_schema.unwrap_or_default();
                let mut r = BaseCommand::of(t::GET_SCHEMA_RESPONSE);
                r.get_schema_response = Some(wire::CommandGetSchemaResponse {
                    request_id: g.request_id,
                    error_code: Some(se::TOPIC_NOT_FOUND),
                    error_message: Some("this broker keeps no schemas".into()),
                });
                wire::simple(&r)
            }
            other => {
                log.debug(format!(
                    "Pulsar connection {id}: command {other} is not supported; ignored"
                ));
                continue;
            }
        };
        tx.send(reply).await?;
    }
    Ok(())
}
