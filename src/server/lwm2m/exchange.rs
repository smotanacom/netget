//! CoAP exchanges over one UDP socket, both directions: confirmable requests with RFC 7252
//! retransmission, piggybacked and separate responses matched by token, duplicate requests
//! answered from a cache, and observation notifications (RFC 7641) delivered by token.
use crate::server::coap::codec::{self, CoapMessage, MessageType};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

pub const OPT_OBSERVE: u16 = 6;
/// Datagrams larger than this are dropped (no block-wise transfer).
pub const MAX_DATAGRAM: usize = 16 * 1024;
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RETRANSMIT: u32 = 4;
/// How long a separate response may follow an empty ACK.
const SEPARATE_WAIT: Duration = Duration::from_secs(30);
const DEDUP_ENTRIES: usize = 512;
const OBSERVATIONS: usize = 1024;

type Key = (SocketAddr, Vec<u8>);

pub struct Exchange {
    socket: Arc<UdpSocket>,
    pending: Mutex<HashMap<Key, oneshot::Sender<CoapMessage>>>,
    acked: Mutex<HashMap<(SocketAddr, u16), oneshot::Sender<()>>>,
    observations: Mutex<HashMap<Key, mpsc::Sender<CoapMessage>>>,
    recent: Mutex<VecDeque<((SocketAddr, u16), Vec<u8>)>>,
    /// Confirmable requests handed out and not yet answered: retransmissions are dropped.
    inflight: Mutex<HashSet<(SocketAddr, u16)>>,
    mid: AtomicU16,
    token: AtomicU64,
}

impl Exchange {
    pub fn new(socket: Arc<UdpSocket>) -> Arc<Self> {
        let seed = crate::utils::clock::SystemTime::now()
            .duration_since(crate::utils::clock::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Arc::new(Self {
            socket,
            pending: Mutex::new(HashMap::new()),
            acked: Mutex::new(HashMap::new()),
            observations: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            inflight: Mutex::new(HashSet::new()),
            mid: AtomicU16::new(seed as u16),
            token: AtomicU64::new(seed),
        })
    }

    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    fn next_mid(&self) -> u16 {
        self.mid.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, peer: SocketAddr, m: &CoapMessage) -> Result<()> {
        let bytes = m.encode().map_err(|e| anyhow::anyhow!("{e}"))?;
        self.socket.send_to(&bytes, peer).await?;
        Ok(())
    }

    /// Send a confirmable request and wait for its response. With `observe`, notifications
    /// that follow on the same token are delivered there until the peer or the receiver stops.
    pub async fn request(
        &self,
        peer: SocketAddr,
        code: u8,
        options: Vec<(u16, Vec<u8>)>,
        payload: Vec<u8>,
        observe: Option<mpsc::Sender<CoapMessage>>,
    ) -> Result<CoapMessage> {
        let token = self
            .token
            .fetch_add(1, Ordering::Relaxed)
            .to_be_bytes()
            .to_vec();
        let mid = self.next_mid();
        let m = CoapMessage {
            mtype: MessageType::Confirmable,
            code,
            message_id: mid,
            token: token.clone(),
            options,
            payload,
        };
        let (tx, mut rx) = oneshot::channel();
        let (ack_tx, mut ack_rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((peer, token.clone()), tx);
        self.acked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((peer, mid), ack_tx);
        // Registered before sending, so a notification that overtakes the response is kept.
        if let Some(sink) = &observe {
            let mut obs = self.observations.lock().unwrap_or_else(|e| e.into_inner());
            if obs.len() < OBSERVATIONS {
                obs.insert((peer, token.clone()), sink.clone());
            }
        }
        let result = self.await_response(peer, &m, &mut rx, &mut ack_rx).await;
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(peer, token.clone()));
        self.acked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(peer, mid));
        let observing = matches!(&result, Ok(r) if r.option_values(OPT_OBSERVE).first().is_some() && codec::code_class(r.code) == 2);
        if observe.is_some() && !observing {
            self.forget(peer, &token);
        }
        result
    }

    /// Retransmit with exponential back-off until a response; after an empty ACK, wait for the
    /// separate response without retransmitting.
    async fn await_response(
        &self,
        peer: SocketAddr,
        m: &CoapMessage,
        rx: &mut oneshot::Receiver<CoapMessage>,
        ack_rx: &mut oneshot::Receiver<()>,
    ) -> Result<CoapMessage> {
        let mut wait = ACK_TIMEOUT;
        for _ in 0..=MAX_RETRANSMIT {
            self.send(peer, m).await?;
            tokio::select! {
                r = &mut *rx => return r.context("the exchange was dropped"),
                a = &mut *ack_rx => {
                    if a.is_ok() {
                        return tokio::time::timeout(SEPARATE_WAIT, rx)
                            .await
                            .context("no separate response followed the ACK")?
                            .context("the exchange was dropped");
                    }
                }
                _ = tokio::time::sleep(wait) => wait *= 2,
            }
        }
        bail!("{peer} did not answer")
    }

    /// Forget an observation (its notifications are answered with RST from now on).
    pub fn forget(&self, peer: SocketAddr, token: &[u8]) {
        self.observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(peer, token.to_vec()));
    }

    /// Answer a request: piggybacked on the ACK for a CON, a NON otherwise. Cached so a
    /// retransmitted request gets the same answer.
    pub async fn respond(
        &self,
        peer: SocketAddr,
        request: &CoapMessage,
        code: u8,
        options: Vec<(u16, Vec<u8>)>,
        payload: Vec<u8>,
    ) -> Result<()> {
        let (mtype, mid) = if request.mtype == MessageType::Confirmable {
            (MessageType::Acknowledgement, request.message_id)
        } else {
            (MessageType::NonConfirmable, self.next_mid())
        };
        let m = CoapMessage {
            mtype,
            code,
            message_id: mid,
            token: request.token.clone(),
            options,
            payload,
        };
        let bytes = m.encode().map_err(|e| anyhow::anyhow!("{e}"))?;
        self.socket.send_to(&bytes, peer).await?;
        if request.mtype == MessageType::Confirmable {
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(peer, request.message_id));
            let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            if recent.len() >= DEDUP_ENTRIES {
                recent.pop_front();
            }
            recent.push_back(((peer, request.message_id), bytes));
        }
        Ok(())
    }

    /// Send a notification for an observation the peer made (a NON carrying Observe).
    pub async fn notify(
        &self,
        peer: SocketAddr,
        token: &[u8],
        sequence: u32,
        options: Vec<(u16, Vec<u8>)>,
        payload: Vec<u8>,
    ) -> Result<()> {
        let mut opts = vec![(
            OPT_OBSERVE,
            (sequence & 0x00FF_FFFF).to_be_bytes()[1..].to_vec(),
        )];
        opts.extend(options);
        let m = CoapMessage {
            mtype: MessageType::NonConfirmable,
            code: codec::CODE_CONTENT,
            message_id: self.next_mid(),
            token: token.to_vec(),
            options: opts,
            payload,
        };
        self.send(peer, &m).await
    }

    /// Read datagrams forever: requests go to `requests`; responses, ACKs and notifications
    /// to whoever waits for them.
    pub async fn run(self: Arc<Self>, requests: mpsc::Sender<(SocketAddr, CoapMessage)>) {
        let mut buf = vec![0u8; MAX_DATAGRAM + 1];
        loop {
            let Ok((n, peer)) = self.socket.recv_from(&mut buf).await else {
                continue;
            };
            if n > MAX_DATAGRAM {
                continue;
            }
            let Ok(m) = CoapMessage::decode(&buf[..n]) else {
                continue;
            };
            if m.is_request() {
                if m.mtype == MessageType::Confirmable {
                    let cached = self
                        .recent
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|(k, _)| *k == (peer, m.message_id))
                        .map(|(_, b)| b.clone());
                    if let Some(bytes) = cached {
                        let _ = self.socket.send_to(&bytes, peer).await;
                        continue;
                    }
                    let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
                    if !inflight.insert((peer, m.message_id)) || inflight.len() > DEDUP_ENTRIES {
                        inflight.remove(&(peer, m.message_id));
                        continue;
                    }
                }
                if requests.send((peer, m)).await.is_err() {
                    return;
                }
                continue;
            }
            match m.mtype {
                MessageType::Acknowledgement | MessageType::Reset if m.is_empty_message() => {
                    if let Some(tx) = self
                        .acked
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&(peer, m.message_id))
                    {
                        let _ = tx.send(());
                    }
                    if m.mtype == MessageType::Reset {
                        self.observations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .retain(|(p, _), _| *p != peer);
                    }
                    continue;
                }
                _ => {}
            }
            // A response: confirm a separate CON response, then route it by token.
            if m.mtype == MessageType::Confirmable {
                let ack = CoapMessage {
                    mtype: MessageType::Acknowledgement,
                    code: codec::CODE_EMPTY,
                    message_id: m.message_id,
                    token: vec![],
                    options: vec![],
                    payload: vec![],
                };
                let _ = self.send(peer, &ack).await;
            }
            let key = (peer, m.token.clone());
            if let Some(tx) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key)
            {
                let _ = tx.send(m);
                continue;
            }
            let sink = self
                .observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .cloned();
            match sink {
                Some(s) if s.try_send(m.clone()).is_ok() || !s.is_closed() => {}
                _ => {
                    self.forget(peer, &key.1);
                    let rst = CoapMessage {
                        mtype: MessageType::Reset,
                        code: codec::CODE_EMPTY,
                        message_id: m.message_id,
                        token: vec![],
                        options: vec![],
                        payload: vec![],
                    };
                    let _ = self.send(peer, &rst).await;
                }
            }
        }
    }
}

/// URI path options for an LwM2M path like `/3/0/1`.
pub fn path_options(path: &str) -> Vec<(u16, Vec<u8>)> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| (codec::OPT_URI_PATH, s.as_bytes().to_vec()))
        .collect()
}

pub fn uint_option(number: u16, v: u32) -> (u16, Vec<u8>) {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|x| **x == 0).count();
    (number, b[skip..].to_vec())
}
