//! The broker's delivery machinery: topics, subscriptions, consumers with their permits and
//! unacknowledged messages, and a bounded backlog per subscription. Everything is behind one
//! lock that no `.await` is ever held across; frames reach consumers through each connection's
//! writer queue.
use super::wire::{self, BaseCommand, Entry, MessageIdData, MessageMetadata};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Messages a subscription holds for consumers without permits; past it the oldest go.
pub const MAX_BACKLOG: usize = 10_000;
/// Messages one consumer may hold unacknowledged; past it the oldest are forgotten.
pub const MAX_UNACKED: usize = 10_000;
/// Topics one broker keeps.
pub const MAX_TOPICS: usize = 10_000;
/// Refused sends remembered, so a client that resends one is refused again without asking.
pub const MAX_REFUSALS: usize = 4096;

/// (topic, producer name, sequence id): one send, as a client would resend it.
type SendKey = (String, String, u64);

#[derive(Clone)]
pub struct Stored {
    pub id: (u64, u64),
    pub metadata: MessageMetadata,
    pub payload: Vec<u8>,
    pub redelivery: u32,
}

struct Consumer {
    conn: u64,
    consumer_id: u64,
    tx: mpsc::Sender<Vec<u8>>,
    permits: u32,
    unacked: VecDeque<Stored>,
}

#[derive(Default)]
struct Sub {
    sub_type: i32,
    consumers: Vec<Consumer>,
    backlog: VecDeque<Stored>,
    next: usize,
}

struct Topic {
    ledger_id: u64,
    next_entry: u64,
    subs: HashMap<String, Sub>,
}

#[derive(Default)]
pub struct Broker {
    topics: Mutex<HashMap<String, Topic>>,
    next_ledger: AtomicU64,
    next_producer: AtomicU64,
    /// (topic, producer name, sequence id) → why it was refused, oldest first.
    refusals: Mutex<(HashMap<SendKey, String>, VecDeque<SendKey>)>,
}

fn frame(consumer_id: u64, m: &Stored) -> Vec<u8> {
    let mut c = BaseCommand::of(wire::command_type::MESSAGE);
    c.message = Some(wire::CommandMessage {
        consumer_id,
        message_id: MessageIdData {
            ledger_id: m.id.0,
            entry_id: m.id.1,
            partition: Some(-1),
            batch_index: Some(-1),
        },
        redelivery_count: Some(m.redelivery),
    });
    wire::with_payload(&c, &m.metadata, &m.payload)
}

impl Sub {
    /// Hand backlog messages to consumers with permits, in order.
    fn drain(&mut self) {
        while let Some(m) = self.backlog.front() {
            let n = self.consumers.len();
            if n == 0 {
                return;
            }
            // Exclusive and Failover deliver to the first consumer; Shared and Key_Shared
            // (treated as Shared) round-robin over those with permits.
            let order: Vec<usize> = if matches!(self.sub_type, 0 | 2) {
                vec![0]
            } else {
                (0..n).map(|i| (self.next + i) % n).collect()
            };
            let Some(i) = order.into_iter().find(|i| self.consumers[*i].permits > 0) else {
                return;
            };
            let c = &mut self.consumers[i];
            if c.tx.try_send(frame(c.consumer_id, m)).is_err() {
                return;
            }
            c.permits -= 1;
            if c.unacked.len() >= MAX_UNACKED {
                c.unacked.pop_front();
            }
            if let Some(m) = self.backlog.pop_front() {
                c.unacked.push_back(m);
            }
            self.next = (i + 1) % n;
        }
    }

    fn enqueue(&mut self, m: Stored) {
        if self.backlog.len() >= MAX_BACKLOG {
            self.backlog.pop_front();
        }
        self.backlog.push_back(m);
    }
}

impl Broker {
    /// Why this send was refused before, if it was.
    pub fn refused(&self, topic: &str, producer: &str, sequence_id: u64) -> Option<String> {
        let r = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        r.0.get(&(topic.to_string(), producer.to_string(), sequence_id))
            .cloned()
    }

    pub fn remember_refusal(&self, topic: &str, producer: &str, sequence_id: u64, why: &str) {
        let mut r = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        let key = (topic.to_string(), producer.to_string(), sequence_id);
        if r.0.len() >= MAX_REFUSALS {
            if let Some(old) = r.1.pop_front() {
                r.0.remove(&old);
            }
        }
        if r.0.insert(key.clone(), why.to_string()).is_none() {
            r.1.push_back(key);
        }
    }

    pub fn producer_name(&self) -> String {
        format!(
            "netget-{}",
            self.next_producer.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn with_consumer<R>(
        &self,
        conn: u64,
        consumer_id: u64,
        f: impl FnOnce(&mut Topic, &str, usize) -> R,
    ) -> Option<R> {
        let mut topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        for topic in topics.values_mut() {
            let found = topic.subs.iter().find_map(|(name, sub)| {
                sub.consumers
                    .iter()
                    .position(|c| c.conn == conn && c.consumer_id == consumer_id)
                    .map(|i| (name.clone(), i))
            });
            if let Some((name, i)) = found {
                return Some(f(topic, &name, i));
            }
        }
        None
    }

    /// Store messages on a topic and deliver them; the id of the last.
    pub fn publish(&self, topic: &str, producer: &str, entries: Vec<Entry>) -> MessageIdData {
        let mut topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        if !topics.contains_key(topic) && topics.len() >= MAX_TOPICS {
            // A full broker forgets the topic with no subscriptions first.
            if let Some(k) = topics
                .iter()
                .find(|(_, t)| t.subs.is_empty())
                .map(|(k, _)| k.clone())
            {
                topics.remove(&k);
            }
        }
        let t = topics.entry(topic.to_string()).or_insert_with(|| Topic {
            ledger_id: self.next_ledger.fetch_add(1, Ordering::Relaxed) + 1,
            next_entry: 0,
            subs: HashMap::new(),
        });
        let mut last = (t.ledger_id, 0);
        for e in entries {
            let id = (t.ledger_id, t.next_entry);
            t.next_entry += 1;
            last = id;
            let stored = Stored {
                id,
                metadata: MessageMetadata {
                    producer_name: producer.to_string(),
                    sequence_id: e.sequence_id,
                    publish_time: wire::now_millis(),
                    properties: e.properties,
                    partition_key: e.key,
                    event_time: e.event_time,
                    ..Default::default()
                },
                payload: e.payload,
                redelivery: 0,
            };
            for sub in t.subs.values_mut() {
                sub.enqueue(stored.clone());
                sub.drain();
            }
        }
        MessageIdData {
            ledger_id: last.0,
            entry_id: last.1,
            partition: Some(-1),
            batch_index: Some(-1),
        }
    }

    /// Whether a consumer could join: an Exclusive subscription takes one consumer, and a
    /// subscription keeps the type it was created with.
    pub fn can_subscribe(&self, topic: &str, sub: &str, sub_type: i32) -> Result<(), String> {
        let topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = topics.get(topic).and_then(|t| t.subs.get(sub)) {
            if !s.consumers.is_empty() && s.sub_type != sub_type {
                return Err(format!(
                    "subscription {sub} is {}",
                    wire::SUB_TYPES.get(s.sub_type as usize).unwrap_or(&"?")
                ));
            }
            if s.sub_type == 0 && !s.consumers.is_empty() {
                return Err(format!(
                    "exclusive subscription {sub} already has a consumer"
                ));
            }
        }
        Ok(())
    }

    pub fn subscribe(
        &self,
        topic: &str,
        sub: &str,
        sub_type: i32,
        conn: u64,
        consumer_id: u64,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<(), String> {
        self.can_subscribe(topic, sub, sub_type)?;
        let mut topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        if !topics.contains_key(topic) && topics.len() >= MAX_TOPICS {
            return Err("too many topics".into());
        }
        let t = topics.entry(topic.to_string()).or_insert_with(|| Topic {
            ledger_id: self.next_ledger.fetch_add(1, Ordering::Relaxed) + 1,
            next_entry: 0,
            subs: HashMap::new(),
        });
        let s = t.subs.entry(sub.to_string()).or_default();
        if s.consumers.is_empty() {
            s.sub_type = sub_type;
        }
        s.consumers.push(Consumer {
            conn,
            consumer_id,
            tx,
            permits: 0,
            unacked: VecDeque::new(),
        });
        Ok(())
    }

    pub fn flow(&self, conn: u64, consumer_id: u64, permits: u32) {
        self.with_consumer(conn, consumer_id, |t, name, i| {
            if let Some(s) = t.subs.get_mut(name) {
                s.consumers[i].permits = s.consumers[i].permits.saturating_add(permits);
                s.drain();
            }
        });
    }

    pub fn ack(&self, conn: u64, consumer_id: u64, cumulative: bool, ids: &[MessageIdData]) {
        self.with_consumer(conn, consumer_id, |t, name, i| {
            if let Some(s) = t.subs.get_mut(name) {
                let c = &mut s.consumers[i];
                for id in ids {
                    let key = (id.ledger_id, id.entry_id);
                    if cumulative {
                        c.unacked.retain(|m| m.id > key);
                    } else {
                        c.unacked.retain(|m| m.id != key);
                    }
                }
            }
        });
    }

    /// Put a consumer's unacknowledged messages (all, or those named) back at the front.
    pub fn redeliver(&self, conn: u64, consumer_id: u64, ids: &[MessageIdData]) {
        self.with_consumer(conn, consumer_id, |t, name, i| {
            if let Some(s) = t.subs.get_mut(name) {
                let c = &mut s.consumers[i];
                let (back, keep): (Vec<Stored>, Vec<Stored>) = c.unacked.drain(..).partition(|m| {
                    ids.is_empty() || ids.iter().any(|id| (id.ledger_id, id.entry_id) == m.id)
                });
                c.unacked = keep.into();
                for mut m in back.into_iter().rev() {
                    m.redelivery += 1;
                    s.backlog.push_front(m);
                }
                s.drain();
            }
        });
    }

    fn remove(&self, conn: u64, consumer_id: u64, unsubscribe: bool) -> Option<Result<(), String>> {
        self.with_consumer(conn, consumer_id, |t, name, i| {
            let Some(s) = t.subs.get_mut(name) else {
                return Ok(());
            };
            if unsubscribe && s.consumers.len() > 1 {
                return Err("other consumers are still connected to this subscription".to_string());
            }
            let c = s.consumers.remove(i);
            if unsubscribe {
                t.subs.remove(name);
            } else {
                for m in c.unacked.into_iter().rev() {
                    s.backlog.push_front(Stored {
                        redelivery: m.redelivery + 1,
                        ..m
                    });
                }
                s.next = 0;
                s.drain();
            }
            Ok(())
        })
    }

    pub fn unsubscribe(&self, conn: u64, consumer_id: u64) -> Result<(), String> {
        self.remove(conn, consumer_id, true)
            .unwrap_or_else(|| Err("no such consumer".into()))
    }

    pub fn close_consumer(&self, conn: u64, consumer_id: u64) {
        self.remove(conn, consumer_id, false);
    }

    pub fn last_message_id(&self, conn: u64, consumer_id: u64) -> Option<MessageIdData> {
        self.with_consumer(conn, consumer_id, |t, _, _| MessageIdData {
            ledger_id: t.ledger_id,
            entry_id: t.next_entry.saturating_sub(1),
            partition: Some(-1),
            batch_index: Some(-1),
        })
    }

    /// A connection went away: its consumers leave, their messages go back to the backlog.
    pub fn drop_connection(&self, conn: u64) {
        let ids: Vec<u64> = {
            let topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
            topics
                .values()
                .flat_map(|t| t.subs.values())
                .flat_map(|s| s.consumers.iter())
                .filter(|c| c.conn == conn)
                .map(|c| c.consumer_id)
                .collect()
        };
        for id in ids {
            self.close_consumer(conn, id);
        }
    }
}
