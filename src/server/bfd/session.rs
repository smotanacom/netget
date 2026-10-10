//! One BFD session in Asynchronous mode (RFC 5880 §6.8): the state machine, the timers, the
//! Poll Sequence and the authentication sequence numbers. Pure apart from the clock it is
//! handed, so the server (passive role) and the client (active role) run the same code.
//! Demand mode and the Echo function are not implemented: NetGet never sets the D bit and
//! always advertises a Required Min Echo RX Interval of zero.
use super::packet::{self, AuthConfig, ControlPacket, State};
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;

/// §6.8.3: while the session is not Up, Desired Min TX is at least one second.
pub const SLOW_TX_US: u32 = 1_000_000;
pub const DEFAULT_DESIRED_MIN_TX_MS: u32 = 300;
pub const DEFAULT_REQUIRED_MIN_RX_MS: u32 = 300;
pub const DEFAULT_DETECT_MULT: u8 = 3;
/// The range NetGet accepts for an interval, in milliseconds.
pub const MIN_INTERVAL_MS: u32 = 10;
pub const MAX_INTERVAL_MS: u32 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timers {
    pub desired_min_tx_us: u32,
    pub required_min_rx_us: u32,
    pub detect_mult: u8,
}

impl Default for Timers {
    fn default() -> Self {
        Self {
            desired_min_tx_us: DEFAULT_DESIRED_MIN_TX_MS * 1000,
            required_min_rx_us: DEFAULT_REQUIRED_MIN_RX_MS * 1000,
            detect_mult: DEFAULT_DETECT_MULT,
        }
    }
}

impl Timers {
    /// From an action or startup parameters: `desired_min_tx_ms`, `required_min_rx_ms`,
    /// `detect_mult`, each optional, falling back to `base`.
    pub fn from_json(v: &Value, base: Timers) -> Result<Self> {
        let ms = |key: &str, fallback: u32| -> Result<u32> {
            match v.get(key) {
                None | Some(Value::Null) => Ok(fallback),
                Some(x) => {
                    let n = x
                        .as_u64()
                        .filter(|n| (MIN_INTERVAL_MS as u64..=MAX_INTERVAL_MS as u64).contains(n));
                    match n {
                        Some(n) => Ok(n as u32 * 1000),
                        None => bail!(
                            "{key} must be a whole number of milliseconds, {MIN_INTERVAL_MS}-{MAX_INTERVAL_MS}"
                        ),
                    }
                }
            }
        };
        let detect_mult = match v.get("detect_mult") {
            None | Some(Value::Null) => base.detect_mult,
            Some(x) => {
                let n = x.as_u64().unwrap_or(0);
                ensure!((1..=255).contains(&n), "detect_mult must be 1-255");
                n as u8
            }
        };
        Ok(Self {
            desired_min_tx_us: ms("desired_min_tx_ms", base.desired_min_tx_us / 1000)?,
            required_min_rx_us: ms("required_min_rx_ms", base.required_min_rx_us / 1000)?,
            detect_mult,
        })
    }
}

/// What one received packet did to the session.
#[derive(Debug, Default)]
pub struct Received {
    /// The state before and after, when it changed.
    pub transition: Option<(State, State)>,
    /// The packet had Poll set: answer at once with Final.
    pub send_final: bool,
}

pub struct Session {
    pub local_discr: u32,
    pub remote_discr: u32,
    pub state: State,
    pub remote_state: State,
    pub local_diag: u8,
    /// What the operator or the model asked for.
    pub configured: Timers,
    /// What the packets say now (Desired Min TX raised to a second while not Up).
    advertised: Timers,
    /// The values the local timing actually uses: a Poll Sequence defers an increase of the
    /// transmit interval and a decrease of the receive interval until the Final (§6.8.3).
    tx_in_force_us: u32,
    rx_in_force_us: u32,
    pub poll: bool,
    pub remote_desired_min_tx_us: u32,
    pub remote_min_rx_us: u32,
    pub remote_detect_mult: u8,
    pub passive: bool,
    auth: Option<AuthConfig>,
    xmit_seq: u32,
    rcv_seq: Option<u32>,
    pub last_rx: Option<Instant>,
}

impl Session {
    pub fn new(
        local_discr: u32,
        configured: Timers,
        auth: Option<AuthConfig>,
        passive: bool,
    ) -> Self {
        let advertised = Timers {
            desired_min_tx_us: configured.desired_min_tx_us.max(SLOW_TX_US),
            ..configured
        };
        Self {
            local_discr,
            remote_discr: 0,
            state: State::Down,
            remote_state: State::Down,
            local_diag: 0,
            configured,
            advertised,
            tx_in_force_us: advertised.desired_min_tx_us,
            rx_in_force_us: advertised.required_min_rx_us,
            poll: false,
            remote_desired_min_tx_us: 0,
            // §6.8.1: initialized to 1 so the first packets go out.
            remote_min_rx_us: 1,
            remote_detect_mult: 0,
            passive,
            auth,
            xmit_seq: rand::random(),
            rcv_seq: None,
            last_rx: None,
        }
    }

    /// Advertise new values. While Up, a change starts a Poll Sequence; an increase of the
    /// transmit interval or a decrease of the receive interval takes effect at the Final.
    fn advertise(&mut self) {
        let next = Timers {
            desired_min_tx_us: if self.state == State::Up {
                self.configured.desired_min_tx_us
            } else {
                self.configured.desired_min_tx_us.max(SLOW_TX_US)
            },
            ..self.configured
        };
        if next == self.advertised {
            return;
        }
        if self.state == State::Up {
            self.poll = true;
            if next.desired_min_tx_us < self.tx_in_force_us {
                self.tx_in_force_us = next.desired_min_tx_us;
            }
            if next.required_min_rx_us > self.rx_in_force_us {
                self.rx_in_force_us = next.required_min_rx_us;
            }
        } else {
            self.tx_in_force_us = next.desired_min_tx_us;
            self.rx_in_force_us = next.required_min_rx_us;
        }
        self.advertised = next;
    }

    pub fn set_timers(&mut self, timers: Timers) {
        self.configured = timers;
        self.advertise();
    }

    fn set_state(&mut self, next: State, diag: u8) -> Option<(State, State)> {
        if next == self.state {
            return None;
        }
        let previous = self.state;
        self.state = next;
        self.local_diag = diag;
        self.advertise();
        Some((previous, next))
    }

    pub fn admin_down(&mut self, diag: u8) -> Option<(State, State)> {
        self.set_state(State::AdminDown, diag)
    }

    pub fn admin_up(&mut self) -> Option<(State, State)> {
        if self.state != State::AdminDown {
            return None;
        }
        self.set_state(State::Down, 0)
    }

    /// §6.8.6, after the packet has been decoded and matched to this session.
    pub fn receive(&mut self, bytes: &[u8], p: &ControlPacket, now: Instant) -> Result<Received> {
        packet::verify(bytes, p, self.auth.as_ref())?;
        if let (Some(config), Some(seq)) = (&self.auth, p.auth.as_ref().and_then(|a| a.sequence)) {
            if let Some(last) = self.rcv_seq {
                let window = 3 * p.detect_mult as u32;
                let ahead = seq.wrapping_sub(last);
                let ok = if config.kind.meticulous() {
                    (1..=window).contains(&ahead)
                } else {
                    ahead <= window
                };
                ensure!(
                    ok,
                    "sequence number {seq} is outside the window after {last}"
                );
            }
            self.rcv_seq = Some(seq);
        }
        let mut out = Received {
            send_final: p.poll,
            ..Default::default()
        };
        self.last_rx = Some(now);
        self.remote_discr = p.my_discriminator;
        self.remote_state = p.state;
        self.remote_desired_min_tx_us = p.desired_min_tx_us;
        self.remote_min_rx_us = p.required_min_rx_us;
        self.remote_detect_mult = p.detect_mult;
        if p.final_ && self.poll {
            self.poll = false;
            self.tx_in_force_us = self.advertised.desired_min_tx_us;
            self.rx_in_force_us = self.advertised.required_min_rx_us;
        }
        if self.state == State::AdminDown {
            return Ok(out);
        }
        out.transition = match (p.state, self.state) {
            (State::AdminDown, local) if local != State::Down => self.set_state(State::Down, 3),
            (State::AdminDown, _) => None,
            (State::Down, State::Down) => self.set_state(State::Init, 0),
            (State::Init, State::Down) => self.set_state(State::Up, 0),
            (State::Init | State::Up, State::Init) => self.set_state(State::Up, 0),
            (State::Down, State::Up) => self.set_state(State::Down, 3),
            _ => None,
        };
        Ok(out)
    }

    /// The packet to send now: `final_` answers a Poll.
    pub fn packet(&mut self, final_: bool) -> Vec<u8> {
        let p = ControlPacket {
            diag: self.local_diag,
            state: self.state,
            poll: self.poll && !final_,
            final_,
            control_plane_independent: false,
            demand: false,
            detect_mult: self.advertised.detect_mult,
            my_discriminator: self.local_discr,
            your_discriminator: self.remote_discr,
            desired_min_tx_us: self.advertised.desired_min_tx_us,
            required_min_rx_us: self.advertised.required_min_rx_us,
            required_min_echo_rx_us: 0,
            auth: None,
        };
        let auth = self.auth.as_ref().map(|a| {
            if a.kind != packet::AuthType::SimplePassword {
                self.xmit_seq = self.xmit_seq.wrapping_add(1);
            }
            (a, self.xmit_seq)
        });
        p.encode(auth)
    }

    /// §6.8.7: a passive system is silent until it knows the remote discriminator, and nobody
    /// sends to a peer asking for no packets.
    pub fn may_transmit(&self) -> bool {
        !(self.passive && self.remote_discr == 0) && self.remote_min_rx_us != 0
    }

    /// The interval to the next periodic packet, with the §6.8.7 jitter: 75–100% of the
    /// interval, or 75–90% with a Detect Mult of one.
    pub fn next_interval(&self) -> Duration {
        let base = self.tx_in_force_us.max(self.remote_min_rx_us) as u64;
        let top = if self.advertised.detect_mult == 1 {
            90
        } else {
            100
        };
        let pct = rand::random::<u64>() % (top - 75 + 1) + 75;
        Duration::from_micros(base * pct / 100)
    }

    pub fn tx_interval_us(&self) -> u32 {
        self.tx_in_force_us.max(self.remote_min_rx_us)
    }

    /// §6.8.4, Asynchronous mode.
    pub fn detection_time(&self) -> Duration {
        Duration::from_micros(
            self.remote_detect_mult as u64
                * self.rx_in_force_us.max(self.remote_desired_min_tx_us) as u64,
        )
    }

    /// When the session goes Down for silence, if it is waiting on the peer at all.
    pub fn detection_deadline(&self) -> Option<Instant> {
        match self.state {
            State::Init | State::Up => self.last_rx.map(|t| t + self.detection_time()),
            _ => None,
        }
    }

    /// The Detection Time passed: Down with diagnostic 1, and the remote discriminator is
    /// forgotten (§6.8.1).
    pub fn detection_expired(&mut self) -> Option<(State, State)> {
        self.remote_discr = 0;
        self.rcv_seq = None;
        self.set_state(State::Down, 1)
    }

    /// The session as the model sees it.
    pub fn snapshot(&self) -> Value {
        json!({
            "state": self.state.name(),
            "diag": packet::diag_name(self.local_diag),
            "remote_state": self.remote_state.name(),
            "local_discriminator": self.local_discr,
            "remote_discriminator": self.remote_discr,
            "desired_min_tx_ms": self.configured.desired_min_tx_us / 1000,
            "required_min_rx_ms": self.configured.required_min_rx_us / 1000,
            "detect_mult": self.configured.detect_mult,
            "remote_desired_min_tx_ms": self.remote_desired_min_tx_us / 1000,
            "remote_required_min_rx_ms": self.remote_min_rx_us / 1000,
            "remote_detect_mult": self.remote_detect_mult,
            "tx_interval_ms": self.tx_interval_us() / 1000,
            "detection_time_ms": self.detection_time().as_millis() as u64,
            "poll_in_progress": self.poll,
            "authentication": self.auth.as_ref().map(|a| a.kind.name()),
        })
    }
}
