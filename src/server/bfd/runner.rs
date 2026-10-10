//! Runs one [`Session`] against the network: periodic transmission with jitter, the Final
//! answer to a Poll, the Detection Time, and commands from the model or the dashboard.
//! The server and the client each run one of these per session; the model is consulted
//! outside it, through the [`Note`]s it sends.
use super::packet::{self, ControlPacket, State};
use super::session::{Session, Timers};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub enum Input {
    Packet {
        bytes: Vec<u8>,
        packet: ControlPacket,
    },
    /// An action for this session. `depth` is how many model turns led to it; `command` is
    /// the injected caller waiting for an outcome, if any.
    Action {
        action: Value,
        depth: u32,
        command: Option<ClientCommand>,
    },
}

pub enum Note {
    Changed {
        previous: State,
        snapshot: Value,
        depth: u32,
    },
    /// The session ended (idle, or its inputs closed).
    Ended { local_discr: u32 },
}

/// Validate a session action: `bfd_set_timers`, `bfd_admin_down`, `bfd_admin_up`.
pub fn check_action(v: &Value) -> Result<()> {
    match v["type"].as_str().unwrap_or_default() {
        "bfd_set_timers" => {
            Timers::from_json(v, Timers::default())?;
        }
        "bfd_admin_down" => {
            diag_of(v)?;
        }
        "bfd_admin_up" => {}
        other => bail!("{other:?} is not a BFD session action"),
    }
    Ok(())
}

fn diag_of(v: &Value) -> Result<u8> {
    match v["diag"].as_str() {
        None => Ok(7),
        Some(name) => packet::diag_code(name),
    }
}

fn apply(s: &mut Session, v: &Value) -> Result<Option<(State, State)>> {
    check_action(v)?;
    Ok(match v["type"].as_str().unwrap_or_default() {
        "bfd_set_timers" => {
            s.set_timers(Timers::from_json(v, s.configured)?);
            None
        }
        "bfd_admin_down" => s.admin_down(diag_of(v)?),
        _ => s.admin_up(),
    })
}

pub struct Link {
    pub socket: Arc<UdpSocket>,
    pub dest: SocketAddr,
    pub label: String,
}

impl Link {
    async fn send(&self, s: &mut Session, final_: bool) {
        let bytes = s.packet(final_);
        if let Err(e) = self.socket.send_to(&bytes, self.dest).await {
            tracing::warn!("{}: sending to {} failed: {e}", self.label, self.dest);
        }
    }
}

/// Run until the inputs close, or — when `idle` is given — until the session has been Down
/// with nothing heard from the peer for that long.
pub async fn run(
    mut s: Session,
    link: Link,
    mut inputs: mpsc::Receiver<Input>,
    notes: mpsc::Sender<Note>,
    idle: Option<Duration>,
) {
    let started = Instant::now();
    let mut next_tx = Instant::now();
    loop {
        let detect = s.detection_deadline();
        let idle_at = idle.and_then(|d| {
            matches!(s.state, State::Down | State::AdminDown)
                .then(|| s.last_rx.unwrap_or(started) + d)
        });
        let (transition, depth) = tokio::select! {
            input = inputs.recv() => match input {
                None => break,
                Some(Input::Packet { bytes, packet }) => {
                    let could_send = s.may_transmit();
                    match s.receive(&bytes, &packet, Instant::now()) {
                        Ok(r) => {
                            if r.send_final {
                                link.send(&mut s, true).await;
                            }
                            if r.transition.is_some() || !could_send {
                                next_tx = Instant::now();
                            }
                            (r.transition, 0)
                        }
                        Err(e) => {
                            tracing::warn!("{} dropped a packet decision=dropped: {e:#}", link.label);
                            continue;
                        }
                    }
                }
                Some(Input::Action { action, depth, command }) => match apply(&mut s, &action) {
                    Ok(t) => {
                        // On the wire before the caller hears it was done.
                        if s.may_transmit() {
                            link.send(&mut s, false).await;
                        }
                        next_tx = Instant::now() + s.next_interval();
                        if let Some(c) = command {
                            crate::client::command_support::reply(c, Ok(ClientSendOutcome::Executed {
                                detail: s.snapshot().to_string(),
                            }));
                        }
                        (t, depth)
                    }
                    Err(e) => {
                        tracing::warn!("{} refused {}: {e:#}", link.label, action["type"]);
                        if let Some(c) = command {
                            crate::client::command_support::reply(c, Ok(ClientSendOutcome::Rejected {
                                error: e.to_string(),
                            }));
                        }
                        continue;
                    }
                },
            },
            _ = tokio::time::sleep_until(next_tx) => {
                if s.may_transmit() {
                    link.send(&mut s, false).await;
                }
                next_tx = Instant::now() + s.next_interval();
                continue;
            }
            _ = sleep_until_opt(detect) => (s.detection_expired(), 0),
            _ = sleep_until_opt(idle_at) => {
                tracing::info!("{}: Down with nothing heard for {:?}; ending the session", link.label, idle.unwrap_or_default());
                break;
            }
        };
        if let Some((previous, _)) = transition {
            let note = Note::Changed {
                previous,
                snapshot: s.snapshot(),
                depth,
            };
            if !notes.is_closed() && notes.try_send(note).is_err() {
                tracing::warn!(
                    "{}: a state change was not reported: the model is behind decision=note_queue_full",
                    link.label
                );
            }
        }
    }
    let _ = notes.try_send(Note::Ended {
        local_discr: s.local_discr,
    });
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// A tx socket bound to `ip` on a source port in 49152–65535 (RFC 5881 §4), TTL 255.
pub async fn tx_socket(ip: std::net::IpAddr) -> Result<UdpSocket> {
    let mut last = None;
    for _ in 0..64 {
        let port = packet::SOURCE_PORTS.start()
            + (rand::random::<u16>() % (packet::SOURCE_PORTS.end() - packet::SOURCE_PORTS.start()));
        match UdpSocket::bind(SocketAddr::new(ip, port)).await {
            Ok(s) => {
                if ip.is_ipv4() {
                    s.set_ttl(255).context("setting TTL 255")?;
                }
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("no source port")))
    .context("no free BFD source port in 49152-65535")
}

/// The event data for a state change, given who the peer is.
pub fn state_event(peer: &str, previous: State, snapshot: &Value) -> Value {
    let mut data = snapshot.clone();
    data["peer"] = json!(peer);
    data["previous_state"] = json!(previous.name());
    data
}
