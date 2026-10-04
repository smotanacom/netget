//! MQTT-SN 1.2 gateway over UDP. One task owns every session: datagrams, handler answers,
//! injected commands and a timer arrive on it in turn, so no state is shared. Handler calls run
//! as their own tasks and report back; a client's later datagrams wait while its call is out.
pub mod actions;
pub mod packet;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{Context, Result};
use packet::{Flags, Packet, Qos};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub const DEFAULT_GATEWAY_ID: u8 = 1;
pub const MAX_SESSIONS: usize = 256;
pub const MAX_TOPICS: usize = 10_000;
/// Messages held for one sleeping or disconnected session.
pub const MAX_HELD: usize = 100;
/// Datagrams from one client waiting while its handler call is out.
pub const MAX_BACKLOG: usize = 64;
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
pub const RETRIES: u32 = 3;
const TICK: Duration = Duration::from_millis(200);

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    addr: SocketAddr,
    node: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct Will {
    topic: String,
    msg: Vec<u8>,
    qos: Qos,
    retain: bool,
}

#[derive(Clone, Debug)]
struct Delivery {
    topic: String,
    data: Vec<u8>,
    qos: Qos,
    retain: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Active,
    Asleep,
    Awake,
    Disconnected,
}

enum Waiting {
    RegAck(Delivery, u16),
    PubAck,
    PubRec,
    PubComp,
}

struct Inflight {
    msg_id: u16,
    waiting: Waiting,
    bytes: Vec<u8>,
    deadline: tokio::time::Instant,
    tries: u32,
}

struct Session {
    client_id: String,
    key: Key,
    conn: ConnectionId,
    clean: bool,
    keep_alive: u16,
    state: State,
    heard: tokio::time::Instant,
    sleep_until: Option<tokio::time::Instant>,
    will: Option<Will>,
    known: HashSet<u16>,
    subscriptions: Vec<(String, Qos)>,
    held: VecDeque<Delivery>,
    inflight: Option<Inflight>,
    /// QoS 2 publishes acknowledged with PUBREC and awaiting PUBREL.
    received: HashSet<u16>,
    busy: bool,
    backlog: VecDeque<Packet>,
    next_id: u16,
}

/// A CONNECT collecting its will before the handler is asked.
struct Pending {
    client_id: String,
    flags: Flags,
    keep_alive: u16,
    will_topic: Option<(Flags, String)>,
    busy: bool,
}

enum Op {
    Connect {
        pending_flags: Flags,
        keep_alive: u16,
        client_id: String,
        will: Option<Will>,
    },
    Message {
        topic: String,
        flags: Flags,
        msg_id: u16,
        topic_ref: u16,
        data: Vec<u8>,
    },
    Subscribe {
        flags: Flags,
        msg_id: u16,
        filter: String,
        ack_topic: u16,
        qos: Qos,
    },
}

enum Input {
    Answer {
        key: Key,
        op: Op,
        answer: Result<Json, &'static str>,
        publishes: Vec<Json>,
    },
    Command {
        client_id: String,
        command: ClientCommand,
    },
}

struct Gateway {
    ctx: SpawnContext,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    gateway_id: u8,
    sessions: HashMap<String, Session>,
    by_key: HashMap<Key, String>,
    pending: HashMap<Key, Pending>,
    topics: HashMap<String, u16>,
    names: HashMap<u16, String>,
    predefined: HashMap<u16, String>,
    next_topic: u16,
    inputs: mpsc::UnboundedSender<Input>,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let mut gateway_id = DEFAULT_GATEWAY_ID;
    let mut predefined = HashMap::new();
    if let Some(p) = ctx.startup_params.as_ref() {
        if let Some(id) = p.get_optional_u64("gateway_id")? {
            gateway_id = u8::try_from(id).context("gateway_id is 0-255")?;
        }
        if let Some(map) = p.get_optional_object("predefined_topics")? {
            anyhow::ensure!(map.len() <= 1024, "at most 1024 predefined topics");
            for (id, topic) in map {
                let id: u16 = id.parse().context("a predefined topic id is 1-65535")?;
                let topic = topic.as_str().context("a predefined topic is a name")?;
                anyhow::ensure!(
                    id != 0 && packet::valid_topic(topic),
                    "predefined topic {id} is not a valid id and name"
                );
                predefined.insert(id, topic.to_owned());
            }
        }
    }
    let socket = Arc::new(UdpSocket::bind(ctx.legacy_listen_addr()).await?);
    let local = socket.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("MQTT-SN gateway {gateway_id} on udp {local}"));
    let (tx, rx) = mpsc::unbounded_channel();
    let mut gw = Gateway {
        ctx: ctx.clone(),
        socket,
        local,
        gateway_id,
        sessions: HashMap::new(),
        by_key: HashMap::new(),
        pending: HashMap::new(),
        topics: HashMap::new(),
        names: HashMap::new(),
        predefined,
        next_topic: 1,
        inputs: tx,
    };
    let task = tokio::spawn(async move { gw.run(rx).await });
    ctx.state.register_server_task(ctx.server_id, task).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, operation: &str, decision: &str) {
    let summary = format!("MQTT-SN operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

fn payload_json(data: &[u8]) -> (Json, &'static str) {
    match std::str::from_utf8(data) {
        Ok(s) => (json!(s), "utf8"),
        Err(_) => (json!(hex::encode(data)), "hex"),
    }
}

impl Gateway {
    async fn run(&mut self, mut inputs: mpsc::UnboundedReceiver<Input>) {
        let mut buf = vec![0u8; 65_535];
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                r = self.socket.recv_from(&mut buf) => match r {
                    Ok((n, from)) => {
                        let d = buf[..n].to_vec();
                        self.datagram(from, &d).await;
                    }
                    Err(e) => Log::new(Some(&self.ctx.status_tx)).debug(format!("MQTT-SN recv: {e}")),
                },
                Some(i) = inputs.recv() => match i {
                    Input::Answer { key, op, answer, publishes } => self.answered(key, op, answer, publishes).await,
                    Input::Command { client_id, command } => self.command(&client_id, command).await,
                },
                _ = tick.tick() => self.timers().await,
            }
        }
    }

    async fn send(&self, key: &Key, p: &Packet) {
        let mut bytes = packet::encode(p);
        if let Some(node) = &key.node {
            bytes = packet::wrap_forwarder(node, &bytes);
        }
        if let Err(e) = self.socket.send_to(&bytes, key.addr).await {
            Log::new(Some(&self.ctx.status_tx)).debug(format!("MQTT-SN send to {}: {e}", key.addr));
        }
    }

    async fn datagram(&mut self, from: SocketAddr, d: &[u8]) {
        let (key, inner) = match packet::unwrap_forwarder(d) {
            Ok(Some((node, inner))) => (
                Key {
                    addr: from,
                    node: Some(node),
                },
                inner,
            ),
            Ok(None) => (
                Key {
                    addr: from,
                    node: None,
                },
                d,
            ),
            Err(e) => {
                outcome(&self.ctx, "decode", "protocol_refusal");
                Log::new(Some(&self.ctx.status_tx)).debug(format!("MQTT-SN from {from}: {e:#}"));
                return;
            }
        };
        let p = match packet::decode(inner) {
            Ok(p) => p,
            Err(e) => {
                outcome(&self.ctx, "decode", "protocol_refusal");
                Log::new(Some(&self.ctx.status_tx)).debug(format!("MQTT-SN from {from}: {e:#}"));
                return;
            }
        };
        if let Some(cid) = self.by_key.get(&key).cloned() {
            if let Some(s) = self.sessions.get_mut(&cid) {
                s.heard = tokio::time::Instant::now();
                if s.busy
                    && !matches!(
                        p,
                        Packet::PubAck { .. }
                            | Packet::PubRec { .. }
                            | Packet::PubComp { .. }
                            | Packet::RegAck { .. }
                            | Packet::PingReq { .. }
                    )
                {
                    if s.backlog.len() < MAX_BACKLOG {
                        s.backlog.push_back(p);
                    }
                    return;
                }
            }
        }
        self.packet(key, p).await;
    }

    fn session_for(&mut self, key: &Key) -> Option<&mut Session> {
        let cid = self.by_key.get(key)?;
        self.sessions.get_mut(cid)
    }

    fn topic_id(&mut self, name: &str) -> Option<u16> {
        if let Some(id) = self.topics.get(name) {
            return Some(*id);
        }
        if self.topics.len() >= MAX_TOPICS {
            return None;
        }
        while self.predefined.contains_key(&self.next_topic)
            || self.names.contains_key(&self.next_topic)
            || self.next_topic == 0
        {
            self.next_topic = self.next_topic.wrapping_add(1);
        }
        let id = self.next_topic;
        self.next_topic = self.next_topic.wrapping_add(1);
        self.topics.insert(name.to_owned(), id);
        self.names.insert(id, name.to_owned());
        Some(id)
    }

    /// The topic a PUBLISH or SUBSCRIBE refers to by id.
    fn resolve(&self, flags: &Flags, id: u16) -> Option<String> {
        match flags.topic_id_type {
            packet::TOPIC_PREDEFINED => self.predefined.get(&id).cloned(),
            packet::TOPIC_SHORT => Some(packet::short_name(id)),
            _ => self.names.get(&id).cloned(),
        }
    }

    async fn packet(&mut self, key: Key, p: Packet) {
        match p {
            Packet::SearchGw { .. } => {
                self.send(
                    &key,
                    &Packet::GwInfo {
                        gw_id: self.gateway_id,
                        gw_add: vec![],
                    },
                )
                .await;
            }
            Packet::Connect {
                flags,
                duration,
                client_id,
            } => self.connect(key, flags, duration, client_id).await,
            Packet::WillTopic { flags, topic } => {
                let Some(pend) = self.pending.get_mut(&key) else {
                    return;
                };
                if topic.is_empty() {
                    pend.will_topic = None;
                    let (f, ka, cid) = (pend.flags, pend.keep_alive, pend.client_id.clone());
                    self.ask_connect(key, f, ka, cid, None).await;
                } else {
                    pend.will_topic = Some((flags, topic));
                    self.send(&key, &Packet::WillMsgReq).await;
                }
            }
            Packet::WillMsg { msg } => {
                let Some(pend) = self.pending.get_mut(&key) else {
                    return;
                };
                if pend.busy {
                    return;
                }
                let will = pend.will_topic.clone().map(|(f, topic)| Will {
                    topic,
                    msg,
                    qos: f.qos.max(0),
                    retain: f.retain,
                });
                let (f, ka, cid) = (pend.flags, pend.keep_alive, pend.client_id.clone());
                self.ask_connect(key, f, ka, cid, will).await;
            }
            Packet::Publish {
                flags,
                topic,
                msg_id,
                data,
            } => self.publish_in(key, flags, topic, msg_id, data).await,
            Packet::PubRel { msg_id } => {
                if let Some(s) = self.session_for(&key) {
                    s.received.remove(&msg_id);
                }
                self.send(&key, &Packet::PubComp { msg_id }).await;
            }
            Packet::PubAck { msg_id, .. } | Packet::PubComp { msg_id } => {
                let done = self.session_for(&key).is_some_and(|s| {
                    matches!(&s.inflight, Some(i) if i.msg_id == msg_id && matches!(i.waiting, Waiting::PubAck | Waiting::PubComp))
                });
                if done {
                    if let Some(s) = self.session_for(&key) {
                        s.inflight = None;
                    }
                    self.pump(&key).await;
                }
            }
            Packet::PubRec { msg_id } => {
                let rel = packet::encode(&Packet::PubRel { msg_id });
                let mut send = false;
                if let Some(s) = self.session_for(&key) {
                    if let Some(i) = s.inflight.as_mut().filter(|i| {
                        i.msg_id == msg_id
                            && matches!(i.waiting, Waiting::PubRec | Waiting::PubComp)
                    }) {
                        i.waiting = Waiting::PubComp;
                        i.bytes = rel;
                        i.tries = 0;
                        i.deadline = tokio::time::Instant::now() + RETRY_AFTER;
                        send = true;
                    }
                }
                if send {
                    self.send(&key, &Packet::PubRel { msg_id }).await;
                }
            }
            Packet::RegAck {
                msg_id,
                rc,
                topic_id,
            } => {
                let Some(s) = self.session_for(&key) else {
                    return;
                };
                let Some(i) = s
                    .inflight
                    .take_if(|i| i.msg_id == msg_id && matches!(i.waiting, Waiting::RegAck(..)))
                else {
                    return;
                };
                let Waiting::RegAck(delivery, id) = i.waiting else {
                    return;
                };
                if rc == packet::ACCEPTED && topic_id == id {
                    s.known.insert(id);
                    s.held.push_front(delivery);
                }
                self.pump(&key).await;
            }
            Packet::Register { topic, msg_id, .. } => {
                if self.session_for(&key).is_none() {
                    return;
                }
                let (id, rc) = if !packet::valid_topic(&topic) {
                    (0, packet::NOT_SUPPORTED)
                } else {
                    match self.topic_id(&topic) {
                        Some(id) => (id, packet::ACCEPTED),
                        None => (0, packet::CONGESTION),
                    }
                };
                if let Some(s) = self.session_for(&key) {
                    if rc == packet::ACCEPTED {
                        s.known.insert(id);
                    }
                }
                self.send(
                    &key,
                    &Packet::RegAck {
                        topic_id: id,
                        msg_id,
                        rc,
                    },
                )
                .await;
            }
            Packet::Subscribe {
                flags,
                msg_id,
                topic_name,
                topic_id,
            } => {
                self.subscribe(key, flags, msg_id, topic_name, topic_id)
                    .await
            }
            Packet::Unsubscribe {
                flags,
                msg_id,
                topic_name,
                topic_id,
            } => {
                let name = topic_name.or_else(|| self.resolve(&flags, topic_id));
                if let Some(s) = self.session_for(&key) {
                    if let Some(n) = name {
                        s.subscriptions.retain(|(f, _)| *f != n);
                    }
                    self.send(&key, &Packet::UnsubAck { msg_id }).await;
                }
            }
            Packet::PingReq { client_id } => {
                let Some(s) = self.session_for(&key) else {
                    if let Some(cid) = client_id {
                        self.wake_moved(key, cid).await;
                    }
                    return;
                };
                if client_id.is_some() && matches!(s.state, State::Asleep | State::Awake) {
                    s.state = State::Awake;
                    self.pump(&key).await;
                } else {
                    self.send(&key, &Packet::PingResp).await;
                }
            }
            Packet::Disconnect { duration } => {
                let Some(s) = self.session_for(&key) else {
                    self.send(&key, &Packet::Disconnect { duration: None })
                        .await;
                    return;
                };
                match duration {
                    Some(d) if d > 0 => {
                        s.state = State::Asleep;
                        s.sleep_until = Some(
                            tokio::time::Instant::now() + Duration::from_millis(d as u64 * 1500),
                        );
                        self.send(&key, &Packet::Disconnect { duration: None })
                            .await;
                    }
                    _ => {
                        s.will = None;
                        self.send(&key, &Packet::Disconnect { duration: None })
                            .await;
                        self.end(&key, false).await;
                    }
                }
            }
            Packet::WillTopicUpd { flags, topic } => {
                if let Some(s) = self.session_for(&key) {
                    if topic.is_empty() {
                        s.will = None;
                    } else {
                        let msg = s.will.as_ref().map(|w| w.msg.clone()).unwrap_or_default();
                        s.will = Some(Will {
                            topic,
                            msg,
                            qos: flags.qos.max(0),
                            retain: flags.retain,
                        });
                    }
                    self.send(
                        &key,
                        &Packet::WillTopicResp {
                            rc: packet::ACCEPTED,
                        },
                    )
                    .await;
                }
            }
            Packet::WillMsgUpd { msg } => {
                if let Some(s) = self.session_for(&key) {
                    if let Some(w) = s.will.as_mut() {
                        w.msg = msg;
                    }
                    self.send(
                        &key,
                        &Packet::WillMsgResp {
                            rc: packet::ACCEPTED,
                        },
                    )
                    .await;
                }
            }
            other => {
                Log::new(Some(&self.ctx.status_tx)).debug(format!(
                    "MQTT-SN from {}: unexpected {}",
                    key.addr,
                    other.name()
                ));
            }
        }
    }

    /// A sleeping client woke from a new address: move its session there.
    async fn wake_moved(&mut self, key: Key, client_id: String) {
        let Some(s) = self.sessions.get_mut(&client_id) else {
            return;
        };
        if !matches!(s.state, State::Asleep | State::Awake) {
            return;
        }
        self.by_key.remove(&s.key);
        s.key = key.clone();
        s.state = State::Awake;
        self.by_key.insert(key.clone(), client_id);
        self.pump(&key).await;
    }

    async fn connect(&mut self, key: Key, flags: Flags, keep_alive: u16, client_id: String) {
        if self.pending.get(&key).is_some_and(|p| p.busy) {
            return;
        }
        let live = self
            .sessions
            .values()
            .filter(|s| s.state != State::Disconnected)
            .count();
        let known = self.sessions.contains_key(&client_id);
        if !known && (live >= MAX_SESSIONS || self.pending.len() >= MAX_SESSIONS) {
            outcome(&self.ctx, "connect", "protocol_refusal");
            self.send(
                &key,
                &Packet::ConnAck {
                    rc: packet::CONGESTION,
                },
            )
            .await;
            return;
        }
        if flags.will {
            self.pending.insert(
                key.clone(),
                Pending {
                    client_id,
                    flags,
                    keep_alive,
                    will_topic: None,
                    busy: false,
                },
            );
            self.send(&key, &Packet::WillTopicReq).await;
        } else {
            self.pending.insert(
                key.clone(),
                Pending {
                    client_id: client_id.clone(),
                    flags,
                    keep_alive,
                    will_topic: None,
                    busy: false,
                },
            );
            self.ask_connect(key, flags, keep_alive, client_id, None)
                .await;
        }
    }

    async fn ask_connect(
        &mut self,
        key: Key,
        flags: Flags,
        keep_alive: u16,
        client_id: String,
        will: Option<Will>,
    ) {
        if let Some(p) = self.pending.get_mut(&key) {
            p.busy = true;
        }
        let mut data = json!({
            "client_id": client_id,
            "address": key.addr.to_string(),
            "clean_session": flags.clean_session,
            "keep_alive_secs": keep_alive,
            "will": null,
        });
        if let Some(w) = &will {
            let (msg, enc) = payload_json(&w.msg);
            data["will"] = json!({"topic": w.topic, "message": msg, "message_encoding": enc, "qos": w.qos, "retain": w.retain});
        }
        if let Some(n) = &key.node {
            data["forwarder_node"] = json!(hex::encode(n));
        }
        let op = Op::Connect {
            pending_flags: flags,
            keep_alive,
            client_id,
            will,
        };
        self.ask(
            key,
            op,
            Event::new(&actions::CONNECT_EVENT, data),
            "connect",
        )
        .await;
    }

    async fn ask(&self, key: Key, op: Op, event: Event, operation: &'static str) {
        let ctx = self.ctx.clone();
        let inputs = self.inputs.clone();
        let state = self.ctx.state.clone();
        let conn = self
            .by_key
            .get(&key)
            .and_then(|c| self.sessions.get(c))
            .map(|s| s.conn);
        let task = async move {
            let (answer, publishes) = match call_llm(
                &ctx.llm_client,
                &ctx.state,
                ctx.server_id,
                conn,
                &event,
                &actions::MqttSnProtocol,
            )
            .await
            {
                Err(_) => {
                    outcome(&ctx, operation, "fail_closed_llm_error");
                    (Err("fail_closed_llm_error"), vec![])
                }
                Ok(result) => {
                    let mut all = Vec::new();
                    let mut pending = result.protocol_results;
                    while let Some(r) = pending.pop() {
                        match r {
                            ActionResult::Custom { data, .. } => all.push(data),
                            ActionResult::Multiple(items) => pending.extend(items),
                            _ => {}
                        }
                    }
                    all.reverse();
                    let (decisions, publishes): (Vec<Json>, Vec<Json>) =
                        all.into_iter().partition(|a| a["type"] != "mqttsn_publish");
                    let answer = match (result.failures.is_empty(), decisions.len()) {
                        (true, 1) => Ok(decisions.into_iter().next().unwrap_or_default()),
                        (true, 0) => {
                            outcome(&ctx, operation, "model_silent");
                            Err("model_silent")
                        }
                        _ => {
                            outcome(&ctx, operation, "fail_closed_invalid_reply");
                            Err("fail_closed_invalid_reply")
                        }
                    };
                    (answer, publishes)
                }
            };
            let _ = inputs.send(Input::Answer {
                key,
                op,
                answer,
                publishes,
            });
        };
        state.spawn_server_task(self.ctx.server_id, task).await;
    }

    fn mark_busy(&mut self, key: &Key, busy: bool) {
        if let Some(s) = self.session_for(key) {
            s.busy = busy;
        }
    }

    async fn publish_in(
        &mut self,
        key: Key,
        flags: Flags,
        topic_ref: u16,
        msg_id: u16,
        data: Vec<u8>,
    ) {
        let session = self.by_key.get(&key).cloned();
        if flags.qos != -1 {
            let usable = session
                .as_ref()
                .and_then(|c| self.sessions.get(c))
                .is_some_and(|s| matches!(s.state, State::Active | State::Awake));
            if !usable {
                self.send(&key, &Packet::Disconnect { duration: None })
                    .await;
                return;
            }
        } else if flags.topic_id_type == packet::TOPIC_NORMAL {
            return;
        }
        if flags.qos == 2
            && self
                .session_for(&key)
                .is_some_and(|s| s.received.contains(&msg_id))
        {
            self.send(&key, &Packet::PubRec { msg_id }).await;
            return;
        }
        let Some(topic) = self.resolve(&flags, topic_ref) else {
            if flags.qos != -1 {
                self.send(
                    &key,
                    &Packet::PubAck {
                        topic_id: topic_ref,
                        msg_id,
                        rc: packet::INVALID_TOPIC_ID,
                    },
                )
                .await;
            }
            return;
        };
        let subscribers = self
            .sessions
            .values()
            .filter(|s| {
                s.subscriptions
                    .iter()
                    .any(|(f, _)| packet::matches(f, &topic))
            })
            .count();
        let (payload, enc) = payload_json(&data);
        let event = Event::new(
            &actions::MESSAGE_EVENT,
            json!({
                "client_id": session.clone(),
                "topic": topic,
                "payload": payload,
                "payload_encoding": enc,
                "qos": flags.qos,
                "retain": flags.retain,
                "subscribers": subscribers,
            }),
        );
        self.mark_busy(&key, true);
        self.ask(
            key,
            Op::Message {
                topic,
                flags,
                msg_id,
                topic_ref,
                data,
            },
            event,
            "publish",
        )
        .await;
    }

    async fn subscribe(
        &mut self,
        key: Key,
        flags: Flags,
        msg_id: u16,
        name: Option<String>,
        topic_id: u16,
    ) {
        let Some(cid) = self.by_key.get(&key).cloned() else {
            self.send(&key, &Packet::Disconnect { duration: None })
                .await;
            return;
        };
        let qos = flags.qos.clamp(0, 2);
        let (filter, ack_topic) = match name {
            Some(n) if packet::valid_filter(&n) => {
                let id = if n.contains(['+', '#']) {
                    Some(0)
                } else if let Some(short) = packet::short_topic(&n) {
                    Some(short)
                } else {
                    self.topic_id(&n)
                };
                match id {
                    Some(id) => (n, id),
                    None => {
                        self.send(
                            &key,
                            &Packet::SubAck {
                                flags: Flags {
                                    qos,
                                    ..Flags::default()
                                },
                                topic_id: 0,
                                msg_id,
                                rc: packet::CONGESTION,
                            },
                        )
                        .await;
                        return;
                    }
                }
            }
            Some(_) => {
                self.send(
                    &key,
                    &Packet::SubAck {
                        flags: Flags {
                            qos,
                            ..Flags::default()
                        },
                        topic_id: 0,
                        msg_id,
                        rc: packet::NOT_SUPPORTED,
                    },
                )
                .await;
                return;
            }
            None => match self.resolve(&flags, topic_id) {
                Some(n) => (n, topic_id),
                None => {
                    self.send(
                        &key,
                        &Packet::SubAck {
                            flags: Flags {
                                qos,
                                ..Flags::default()
                            },
                            topic_id,
                            msg_id,
                            rc: packet::INVALID_TOPIC_ID,
                        },
                    )
                    .await;
                    return;
                }
            },
        };
        let event = Event::new(
            &actions::SUBSCRIBE_EVENT,
            json!({"client_id": cid, "topic": filter, "qos": qos}),
        );
        self.mark_busy(&key, true);
        self.ask(
            key,
            Op::Subscribe {
                flags,
                msg_id,
                filter,
                ack_topic,
                qos,
            },
            event,
            "subscribe",
        )
        .await;
    }

    async fn answered(
        &mut self,
        key: Key,
        op: Op,
        answer: Result<Json, &'static str>,
        publishes: Vec<Json>,
    ) {
        let accepted = matches!(&answer, Ok(a) if a["type"] == "mqttsn_accept");
        let rc = match &answer {
            Ok(a) if a["type"] == "mqttsn_reject" => {
                packet::return_code(a["return_code"].as_str().unwrap_or_default())
                    .unwrap_or(packet::NOT_SUPPORTED)
            }
            Ok(_) => packet::ACCEPTED,
            Err(_) => packet::CONGESTION,
        };
        let operation = match &op {
            Op::Connect { .. } => "connect",
            Op::Message { .. } => "publish",
            Op::Subscribe { .. } => "subscribe",
        };
        if answer.is_ok() {
            outcome(
                &self.ctx,
                operation,
                if accepted {
                    "model_answer"
                } else {
                    "model_reject"
                },
            );
        }
        match op {
            Op::Connect {
                pending_flags,
                keep_alive,
                client_id,
                will,
            } => {
                self.pending.remove(&key);
                if accepted {
                    self.open_session(&key, pending_flags, keep_alive, &client_id, will)
                        .await;
                    self.send(
                        &key,
                        &Packet::ConnAck {
                            rc: packet::ACCEPTED,
                        },
                    )
                    .await;
                } else {
                    self.send(&key, &Packet::ConnAck { rc }).await;
                }
            }
            Op::Message {
                topic,
                flags,
                msg_id,
                topic_ref,
                data,
            } => {
                if accepted {
                    self.route(&topic, &data, flags.qos.max(0), flags.retain)
                        .await;
                    match flags.qos {
                        1 => {
                            self.send(
                                &key,
                                &Packet::PubAck {
                                    topic_id: topic_ref,
                                    msg_id,
                                    rc: packet::ACCEPTED,
                                },
                            )
                            .await
                        }
                        2 => {
                            if let Some(s) = self.session_for(&key) {
                                s.received.insert(msg_id);
                            }
                            self.send(&key, &Packet::PubRec { msg_id }).await;
                        }
                        _ => {}
                    }
                } else if flags.qos != -1 {
                    self.send(
                        &key,
                        &Packet::PubAck {
                            topic_id: topic_ref,
                            msg_id,
                            rc,
                        },
                    )
                    .await;
                }
            }
            Op::Subscribe {
                flags,
                msg_id,
                filter,
                ack_topic,
                qos,
            } => {
                let granted = answer
                    .as_ref()
                    .ok()
                    .and_then(|a| a["qos"].as_i64())
                    .map(|q| q as Qos)
                    .unwrap_or(qos)
                    .min(qos);
                if accepted {
                    if let Some(s) = self.session_for(&key) {
                        s.subscriptions.retain(|(f, _)| *f != filter);
                        s.subscriptions.push((filter, granted));
                        if ack_topic != 0 && flags.topic_id_type == packet::TOPIC_NORMAL {
                            s.known.insert(ack_topic);
                        }
                    }
                    self.send(
                        &key,
                        &Packet::SubAck {
                            flags: Flags {
                                qos: granted,
                                ..Flags::default()
                            },
                            topic_id: ack_topic,
                            msg_id,
                            rc: packet::ACCEPTED,
                        },
                    )
                    .await;
                } else {
                    self.send(
                        &key,
                        &Packet::SubAck {
                            flags: Flags {
                                qos,
                                ..Flags::default()
                            },
                            topic_id: 0,
                            msg_id,
                            rc,
                        },
                    )
                    .await;
                }
            }
        }
        for p in publishes {
            self.gateway_publish(&p).await;
        }
        self.mark_busy(&key, false);
        while let Some(next) = self
            .session_for(&key)
            .filter(|s| !s.busy)
            .and_then(|s| s.backlog.pop_front())
        {
            Box::pin(self.packet(key.clone(), next)).await;
        }
    }

    async fn gateway_publish(&mut self, v: &Json) {
        let Ok((topic, data, qos, retain)) = actions::publish_from(v) else {
            return;
        };
        match v.get("client_id").and_then(Json::as_str) {
            Some(cid) => {
                if let Some(key) = self.sessions.get(cid).map(|s| s.key.clone()) {
                    self.deliver(
                        &key,
                        Delivery {
                            topic,
                            data,
                            qos,
                            retain,
                        },
                    )
                    .await;
                }
            }
            None => self.route(&topic, &data, qos, retain).await,
        }
    }

    async fn open_session(
        &mut self,
        key: &Key,
        flags: Flags,
        keep_alive: u16,
        client_id: &str,
        will: Option<Will>,
    ) {
        // A client id moving to a new address takes its session along.
        if let Some(old) = self.sessions.get(client_id).map(|s| s.key.clone()) {
            if old != *key {
                self.by_key.remove(&old);
            }
        }
        let resume = !flags.clean_session && self.sessions.contains_key(client_id);
        if !resume {
            if let Some(old) = self.sessions.remove(client_id) {
                self.close_connection(&old).await;
            }
        }
        let now = tokio::time::Instant::now();
        if let Some(s) = self.sessions.get_mut(client_id) {
            s.key = key.clone();
            s.state = State::Active;
            s.keep_alive = keep_alive;
            s.heard = now;
            s.will = will;
            s.sleep_until = None;
            s.known.clear();
            s.inflight = None;
        } else {
            let conn = ConnectionId::new(self.ctx.state.get_next_unified_id().await);
            let t = Instant::now();
            self.ctx
                .state
                .add_connection_to_server(
                    self.ctx.server_id,
                    ConnectionState {
                        id: conn,
                        remote_addr: key.addr,
                        local_addr: self.local,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: t,
                        status: ConnectionStatus::Active,
                        status_changed_at: t,
                        protocol_info: ProtocolConnectionInfo::new(json!({"client_id": client_id})),
                    },
                )
                .await;
            let mut rx = crate::server::peer_support::register_peer_channel(
                &self.ctx.state,
                self.ctx.server_id,
                conn.as_u32(),
            )
            .await;
            let inputs = self.inputs.clone();
            let cid = client_id.to_owned();
            self.ctx
                .state
                .spawn_server_task(self.ctx.server_id, async move {
                    while let Some(command) = rx.recv().await {
                        if inputs
                            .send(Input::Command {
                                client_id: cid.clone(),
                                command,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                })
                .await;
            self.sessions.insert(
                client_id.to_owned(),
                Session {
                    client_id: client_id.to_owned(),
                    key: key.clone(),
                    conn,
                    clean: flags.clean_session,
                    keep_alive,
                    state: State::Active,
                    heard: now,
                    sleep_until: None,
                    will,
                    known: HashSet::new(),
                    subscriptions: Vec::new(),
                    held: VecDeque::new(),
                    inflight: None,
                    received: HashSet::new(),
                    busy: false,
                    backlog: VecDeque::new(),
                    next_id: 1,
                },
            );
        }
        if let Some(s) = self.sessions.get_mut(client_id) {
            s.clean = flags.clean_session;
        }
        self.by_key.insert(key.clone(), client_id.to_owned());
        self.pump(key).await;
    }

    async fn close_connection(&self, s: &Session) {
        self.ctx
            .state
            .remove_peer_handle(self.ctx.server_id, s.conn.as_u32())
            .await;
        self.ctx
            .state
            .update_connection_status(self.ctx.server_id, s.conn, ConnectionStatus::Closed)
            .await;
        let _ = self.ctx.status_tx.send("__UPDATE_UI__".into());
    }

    /// The session ends: lost (its will is published) or disconnected. A session without clean
    /// session is kept, holding messages, until the client connects again.
    async fn end(&mut self, key: &Key, lost: bool) {
        let Some(cid) = self.by_key.remove(key) else {
            return;
        };
        let Some(s) = self.sessions.get_mut(&cid) else {
            return;
        };
        let will = if lost { s.will.take() } else { None };
        if s.clean {
            if let Some(s) = self.sessions.remove(&cid) {
                self.close_connection(&s).await;
            }
        } else {
            s.state = State::Disconnected;
            s.inflight = None;
            s.sleep_until = None;
        }
        if let Some(w) = will {
            self.route(&w.topic, &w.msg, w.qos, w.retain).await;
        }
    }

    async fn route(&mut self, topic: &str, data: &[u8], qos: Qos, retain: bool) {
        let targets: Vec<(Key, Qos)> = self
            .sessions
            .values()
            .filter_map(|s| {
                let granted = s
                    .subscriptions
                    .iter()
                    .filter(|(f, _)| packet::matches(f, topic))
                    .map(|(_, q)| *q)
                    .max()?;
                Some((s.key.clone(), granted.min(qos)))
            })
            .collect();
        for (key, q) in targets {
            self.deliver(
                &key,
                Delivery {
                    topic: topic.to_owned(),
                    data: data.to_vec(),
                    qos: q,
                    retain,
                },
            )
            .await;
        }
    }

    async fn deliver(&mut self, key: &Key, d: Delivery) {
        let Some(cid) = self
            .sessions
            .values()
            .find(|s| s.key == *key)
            .map(|s| s.client_id.clone())
        else {
            return;
        };
        let Some(s) = self.sessions.get_mut(&cid) else {
            return;
        };
        if s.held.len() >= MAX_HELD {
            s.held.pop_front();
        }
        s.held.push_back(d);
        if matches!(s.state, State::Active | State::Awake) {
            self.pump(key).await;
        }
    }

    /// Send the next held message if nothing is outstanding; an awake client with nothing left
    /// gets PINGRESP and goes back to sleep.
    async fn pump(&mut self, key: &Key) {
        loop {
            let Some(cid) = self.by_key.get(key).cloned() else {
                return;
            };
            let predefined: Option<u16>;
            let delivery = {
                let Some(s) = self.sessions.get_mut(&cid) else {
                    return;
                };
                if !matches!(s.state, State::Active | State::Awake) || s.inflight.is_some() {
                    return;
                }
                match s.held.pop_front() {
                    Some(d) => d,
                    None => {
                        if s.state == State::Awake {
                            s.state = State::Asleep;
                            self.send(key, &Packet::PingResp).await;
                        }
                        return;
                    }
                }
            };
            predefined = self
                .predefined
                .iter()
                .find(|(_, n)| **n == delivery.topic)
                .map(|(id, _)| *id);
            let (topic_id_type, topic) = if let Some(id) = predefined {
                (packet::TOPIC_PREDEFINED, id)
            } else if let Some(short) = packet::short_topic(&delivery.topic) {
                (packet::TOPIC_SHORT, short)
            } else {
                match self.topic_id(&delivery.topic) {
                    Some(id) => (packet::TOPIC_NORMAL, id),
                    None => continue,
                }
            };
            let Some(s) = self.sessions.get_mut(&cid) else {
                return;
            };
            s.next_id = s.next_id.checked_add(1).unwrap_or(1);
            let msg_id = s.next_id;
            let now = tokio::time::Instant::now();
            if topic_id_type == packet::TOPIC_NORMAL && !s.known.contains(&topic) {
                let p = Packet::Register {
                    topic_id: topic,
                    msg_id,
                    topic: delivery.topic.clone(),
                };
                s.inflight = Some(Inflight {
                    msg_id,
                    waiting: Waiting::RegAck(delivery, topic),
                    bytes: packet::encode(&p),
                    deadline: now + RETRY_AFTER,
                    tries: 0,
                });
                self.send(key, &p).await;
                return;
            }
            let p = Packet::Publish {
                flags: Flags {
                    qos: delivery.qos,
                    retain: delivery.retain,
                    topic_id_type,
                    ..Flags::default()
                },
                topic,
                msg_id: if delivery.qos > 0 { msg_id } else { 0 },
                data: delivery.data,
            };
            if delivery.qos > 0 {
                let waiting = if delivery.qos == 1 {
                    Waiting::PubAck
                } else {
                    Waiting::PubRec
                };
                let mut dup = p.clone();
                if let Packet::Publish { flags, .. } = &mut dup {
                    flags.dup = true;
                }
                s.inflight = Some(Inflight {
                    msg_id,
                    waiting,
                    bytes: packet::encode(&dup),
                    deadline: now + RETRY_AFTER,
                    tries: 0,
                });
                self.send(key, &p).await;
                return;
            }
            self.send(key, &p).await;
        }
    }

    async fn timers(&mut self) {
        let now = tokio::time::Instant::now();
        let mut resend = Vec::new();
        let mut give_up = Vec::new();
        let mut lost = Vec::new();
        for s in self.sessions.values_mut() {
            if let Some(i) = s.inflight.as_mut() {
                if now >= i.deadline {
                    if i.tries < RETRIES {
                        i.tries += 1;
                        i.deadline = now + RETRY_AFTER;
                        resend.push((s.key.clone(), i.bytes.clone()));
                    } else {
                        give_up.push(s.key.clone());
                    }
                }
            }
            let silent = s.state == State::Active
                && s.keep_alive > 0
                && now.duration_since(s.heard) > Duration::from_millis(s.keep_alive as u64 * 1500);
            let overslept = s.state == State::Asleep && s.sleep_until.is_some_and(|t| now > t);
            if silent || overslept {
                lost.push(s.key.clone());
            }
        }
        for (key, bytes) in resend {
            let bytes = match &key.node {
                Some(n) => packet::wrap_forwarder(n, &bytes),
                None => bytes,
            };
            let _ = self.socket.send_to(&bytes, key.addr).await;
        }
        for key in give_up {
            if let Some(s) = self.session_for(&key) {
                s.inflight = None;
            }
            self.pump(&key).await;
        }
        for key in lost {
            Log::new(Some(&self.ctx.status_tx))
                .info(format!("MQTT-SN client at {} lost", key.addr));
            self.end(&key, true).await;
        }
    }

    async fn command(&mut self, client_id: &str, command: ClientCommand) {
        let action = command.action.clone();
        let key = self.sessions.get(client_id).map(|s| s.key.clone());
        let outcome = match (action["type"].as_str(), key) {
            (_, None) => ClientSendOutcome::Rejected {
                error: "the client has no session".into(),
            },
            (Some("disconnect"), Some(key)) => {
                self.send(&key, &Packet::Disconnect { duration: None })
                    .await;
                if let Some(s) = self.sessions.get_mut(client_id) {
                    s.clean = true;
                    s.will = None;
                }
                self.end(&key, false).await;
                ClientSendOutcome::Disconnected
            }
            (Some("mqttsn_publish"), Some(key)) => match actions::publish_from(&action) {
                Ok((topic, data, qos, retain)) => {
                    let n = data.len();
                    self.deliver(
                        &key,
                        Delivery {
                            topic,
                            data,
                            qos,
                            retain,
                        },
                    )
                    .await;
                    ClientSendOutcome::Sent { bytes_sent: n }
                }
                Err(e) => ClientSendOutcome::Rejected {
                    error: e.to_string(),
                },
            },
            _ => ClientSendOutcome::Rejected {
                error: "a client accepts mqttsn_publish or disconnect".into(),
            },
        };
        crate::client::command_support::reply(command, Ok(outcome));
    }
}
