//! TUN/TAP interface endpoint.
//!
//! NetGet becomes one end of a real network interface: the host routes packets to it, and each
//! one is decoded into structured header fields. This is the item that changes what NetGet
//! *is* — any layer-3 service can be synthesised without implementing it — and it comes with
//! one design problem that had to be solved before a line of transport code was worth writing.
//!
//! # The problem: a per-packet LLM call is unusable
//!
//! A single `ping` is one packet per second. A TCP handshake is three packets in milliseconds.
//! Anything real is thousands. A version that asks the model per packet would look broken and
//! would exhaust the LLM budget in seconds.
//!
//! # The answer: three gates, in order, and the model is the last one
//!
//! ```text
//!   frame from the interface
//!         │
//!         ├─ decode (native)                       ── undecodable ─▶ counted, dropped
//!         │
//!         ├─ GATE 1  packet_filter (native)        ── no match ────▶ counted, dropped
//!         │          default "icmp"
//!         │
//!         ├─ GATE 2  the server's event_handlers   ── a rule matches ▶ script/static answers,
//!         │          THE PRIMARY ANSWER PATH                          no model call at all
//!         │
//!         └─ GATE 3  llm_escalation + a rolling    ── over budget ──▶ counted, dropped,
//!                    per-minute budget, default 6                     decision=fail_closed_rate_limited
//!                          │
//!                          ▼
//!                    one model call
//! ```
//!
//! Gate 1 is native code on every packet and is where the volume goes. Gate 2 is the project's
//! existing script/static handler machinery, which `CLAUDE.md` calls "the right default for
//! deterministic behavior" — here it is not a preference but the intended answer path, and
//! `llm_escalation: "never"` makes it the *only* one. Gate 3 exists so that even a
//! misconfigured filter cannot turn a busy interface into a budget fire: it is a ceiling, and
//! packets over it are dropped rather than queued, because a queued packet is meaningless once
//! the sender has moved on.
//!
//! Escalation is therefore explicit at every step: a packet reaches the model only if the
//! operator's filter admitted it, no handler claimed it, escalation is enabled, and the budget
//! has room.
//!
//! # Failure is silence
//!
//! There is no error packet. An LLM failure, a refused `send_packet`, an exhausted budget —
//! all of them drop the packet and write nothing. A fabricated packet on a real interface is
//! indistinguishable from a spoof, and NetGet has no idea what the host expected. The *log*
//! carries the distinction (`decision=…`), the way `src/server/radius/` does.
//!
//! # Transport and pipeline are separated
//!
//! Creating the interface needs root, so the transport can never run in the test suite.
//! Everything above it — decode, filter, budget, event, handler, action, write — runs over a
//! pair of `mpsc` channels in [`TunTapEngine::spawn_over_channels`], which is exactly the code
//! path the real device uses. See `src/server/tuntap/CLAUDE.md` for what that does and does
//! not prove.

pub mod actions;
pub mod packet;

pub use actions::TunTapProtocol;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext, StartupParams};
use crate::scripting::EventHandlerType;
use crate::state::app_state::AppState;
use crate::state::ServerId;

use actions::{
    TUNTAP_INTERFACE_DOWN_EVENT, TUNTAP_INTERFACE_UP_EVENT, TUNTAP_PACKET_RECEIVED_EVENT,
};
use packet::{DecodedPacket, LinkMode, PacketFilter, PacketInformation};

/// How long the read loop waits before re-checking whether it has been asked to stop.
const READ_POLL: Duration = Duration::from_millis(500);

/// Depth of the channels between the device tasks and the pipeline.
///
/// Deliberately bounded, and deliberately small. A backlog of packets is not useful: by the
/// time a hundred-deep queue drains, the sender has retransmitted or given up. Dropping under
/// pressure is the correct behaviour for an interface, and it keeps the memory a busy link can
/// cost NetGet fixed.
const LINK_QUEUE_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Whether a packet no handler answered may reach the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmEscalation {
    /// Never. The server is purely deterministic: handlers answer, everything else is dropped.
    Never,
    /// A packet no event handler claimed may reach the model, subject to the budget.
    Unhandled,
}

impl LlmEscalation {
    /// Parse the `llm_escalation` startup parameter.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "never" | "off" | "none" => Ok(Self::Never),
            "unhandled" | "fallback" => Ok(Self::Unhandled),
            other => Err(format!(
                "llm_escalation must be \"never\" or \"unhandled\", got {other:?}"
            )),
        }
    }

    /// The name used in parameters, logs and event data.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Unhandled => "unhandled",
        }
    }
}

/// Everything the nine declared startup parameters resolve to.
///
/// Every field here is read by the engine. `startup_param_drift_test` fails the build on a
/// parameter that is declared and read nowhere, and this struct is where "read" happens.
#[derive(Debug, Clone)]
pub struct TunTapConfig {
    /// Requested interface name; the platform may hand back a different one.
    pub interface_name: Option<String>,
    /// Layer 3 (TUN) or layer 2 (TAP).
    pub mode: LinkMode,
    /// Local address for the interface.
    pub address: String,
    /// Netmask for that address.
    pub netmask: String,
    /// Interface MTU.
    pub mtu: u16,
    /// Gate 1: which packets become events at all.
    pub filter: PacketFilter,
    /// Gate 3, part one: may an unhandled packet reach the model?
    pub escalation: LlmEscalation,
    /// Gate 3, part two: how often, at most.
    pub llm_max_per_minute: u32,
    /// How to read the platform's per-packet prefix.
    pub packet_information: PacketInformation,
}

impl Default for TunTapConfig {
    fn default() -> Self {
        Self {
            interface_name: None,
            mode: LinkMode::Tun,
            address: "10.7.0.1".to_string(),
            netmask: "255.255.255.0".to_string(),
            mtu: 1500,
            // Only pings, and at most six a minute. A ping is one packet per second and is the
            // one thing an operator can watch by hand; everything wider is an explicit choice.
            filter: PacketFilter::parse("icmp").expect("the default filter parses"),
            escalation: LlmEscalation::Unhandled,
            llm_max_per_minute: 6,
            packet_information: PacketInformation::None,
        }
    }
}

impl TunTapConfig {
    /// Resolve the declared startup parameters.
    ///
    /// Every error is propagated with `?` — `StartupParams` accessors return
    /// `Result<_, StartupParamError>` precisely so an undeclared key or a wrong-typed value
    /// produces a clean error naming the key instead of killing the task that is starting the
    /// server.
    pub fn from_params(params: &Option<StartupParams>) -> Result<Self> {
        let mut cfg = Self::default();
        let Some(params) = params else {
            return Ok(cfg);
        };

        cfg.interface_name = params.get_optional_string("interface_name")?;

        if let Some(v) = params.get_optional_string("mode")? {
            cfg.mode = LinkMode::parse(&v).map_err(|e| anyhow!(e))?;
        }
        if let Some(v) = params.get_optional_string("address")? {
            v.parse::<std::net::IpAddr>()
                .with_context(|| format!("address {v:?} is not an IP address"))?;
            cfg.address = v;
        }
        if let Some(v) = params.get_optional_string("netmask")? {
            v.parse::<std::net::IpAddr>()
                .with_context(|| format!("netmask {v:?} is not an IP address"))?;
            cfg.netmask = v;
        }
        if let Some(v) = params.get_optional_u64("mtu")? {
            if !(576..=65535).contains(&v) {
                return Err(anyhow!("mtu must be between 576 and 65535, got {v}"));
            }
            cfg.mtu = v as u16;
        }
        if let Some(v) = params.get_optional_string("packet_filter")? {
            cfg.filter = PacketFilter::parse(&v).map_err(|e| anyhow!(e))?;
        }
        if let Some(v) = params.get_optional_string("llm_escalation")? {
            cfg.escalation = LlmEscalation::parse(&v).map_err(|e| anyhow!(e))?;
        }
        if let Some(v) = params.get_optional_u64("llm_max_per_minute")? {
            if v > 3600 {
                return Err(anyhow!(
                    "llm_max_per_minute must be between 0 and 3600, got {v}"
                ));
            }
            cfg.llm_max_per_minute = v as u32;
        }
        if let Some(v) = params.get_optional_string("packet_information")? {
            cfg.packet_information = PacketInformation::parse(&v).map_err(|e| anyhow!(e))?;
        }

        Ok(cfg)
    }

    /// One line describing how much of the traffic will ever be looked at.
    pub fn escalation_summary(&self) -> String {
        format!(
            "filter={:?} escalation={} max_per_minute={}",
            self.filter.as_str(),
            self.escalation.as_str(),
            self.llm_max_per_minute
        )
    }
}

// ---------------------------------------------------------------------------
// The budget
// ---------------------------------------------------------------------------

/// A rolling one-minute ceiling on model consultations.
///
/// A plain sliding window rather than a token bucket, because the guarantee an operator needs
/// is the literal one the parameter promises: *never more than N in any minute*. A leaky
/// bucket refilling continuously permits short bursts above N, which is exactly what this is
/// meant to prevent.
#[derive(Debug)]
pub struct EscalationBudget {
    max_per_minute: u32,
    window: Duration,
    hits: VecDeque<Instant>,
}

impl EscalationBudget {
    /// A budget of `max_per_minute` consultations per rolling minute. Zero forbids all.
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            window: Duration::from_secs(60),
            hits: VecDeque::new(),
        }
    }

    /// The configured ceiling.
    pub fn max_per_minute(&self) -> u32 {
        self.max_per_minute
    }

    /// How many consultations remain in the current window, as of `now`.
    pub fn remaining_at(&mut self, now: Instant) -> u32 {
        self.expire(now);
        self.max_per_minute.saturating_sub(self.hits.len() as u32)
    }

    /// Take one consultation if the window has room. Returns false when it does not.
    pub fn try_take(&mut self) -> bool {
        self.try_take_at(Instant::now())
    }

    /// [`Self::try_take`] against a caller-supplied clock, so the window is testable without
    /// sleeping for a minute.
    pub fn try_take_at(&mut self, now: Instant) -> bool {
        if self.max_per_minute == 0 {
            return false;
        }
        self.expire(now);
        if self.hits.len() as u32 >= self.max_per_minute {
            return false;
        }
        self.hits.push_back(now);
        true
    }

    fn expire(&mut self, now: Instant) {
        while let Some(front) = self.hits.front() {
            if now.duration_since(*front) >= self.window {
                self.hits.pop_front();
            } else {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// What the endpoint did with everything it saw.
///
/// These are the numbers that make the escalation design auditable: `received` versus
/// `escalated_to_llm` is exactly how much traffic the deterministic gates absorbed.
#[derive(Debug, Default)]
pub struct TunTapStats {
    /// Frames read from the interface.
    pub received: AtomicU64,
    /// Frames that could not be decoded.
    pub decode_errors: AtomicU64,
    /// Packets gate 1 rejected.
    pub filtered_out: AtomicU64,
    /// Packets an event handler answered, costing no model call.
    pub handled_by_rule: AtomicU64,
    /// Packets that reached the model.
    pub escalated_to_llm: AtomicU64,
    /// Packets dropped because the per-minute budget was exhausted.
    pub dropped_over_budget: AtomicU64,
    /// Packets dropped because `llm_escalation` is `never`.
    pub dropped_escalation_disabled: AtomicU64,
    /// Packets NetGet wrote back to the interface.
    pub sent: AtomicU64,
    /// Answers that named a packet NetGet refused to build.
    pub refused_builds: AtomicU64,
    /// Answers whose frame did not match the interface's layer.
    pub refused_layer: AtomicU64,
}

impl TunTapStats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Read a counter.
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// Where the packet ended up, for the log.
///
/// Silence is the failure mode here, so it must never be ambiguous: an operator grepping
/// `decision=fail_closed_` finds every packet NetGet was asked about and did not answer, and
/// `model_drop` (the model said drop) is a different token from `model_silent` (the model said
/// nothing at all) — the distinction whose collapse `CLAUDE.md` records as the OAuth2 defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// A handler produced a packet.
    HandlerSend,
    /// A handler explicitly dropped it.
    HandlerDrop,
    /// A handler ran and produced nothing.
    HandlerSilent,
    /// The model produced a packet.
    ModelSend,
    /// The model explicitly dropped it.
    ModelDrop,
    /// The model produced no usable action.
    ModelSilent,
    /// The LLM call failed.
    FailClosedLlmError,
    /// An answer named a packet that could not be built.
    FailClosedBuildError,
    /// The per-minute budget was exhausted.
    FailClosedRateLimited,
    /// `llm_escalation` is `never` and no handler claimed the packet.
    LlmDisabled,
}

impl Decision {
    /// The stable token written to the log after `decision=`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HandlerSend => "handler_send",
            Self::HandlerDrop => "handler_drop",
            Self::HandlerSilent => "handler_silent",
            Self::ModelSend => "model_send",
            Self::ModelDrop => "model_drop",
            Self::ModelSilent => "model_silent",
            Self::FailClosedLlmError => "fail_closed_llm_error",
            Self::FailClosedBuildError => "fail_closed_build_error",
            Self::FailClosedRateLimited => "fail_closed_rate_limited",
            Self::LlmDisabled => "llm_disabled",
        }
    }

    /// True for the decisions where NetGet was asked and could not answer.
    pub fn is_fail_closed(&self) -> bool {
        matches!(
            self,
            Self::FailClosedLlmError | Self::FailClosedBuildError | Self::FailClosedRateLimited
        )
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Everything the two channels are wired to on the test side of the pipeline.
pub struct ChannelLink {
    /// Push a frame here to have the pipeline treat it as arriving from the interface.
    pub ingress: mpsc::Sender<Vec<u8>>,
    /// Frames the pipeline decided to write to the interface come out here.
    pub egress: mpsc::Receiver<Vec<u8>>,
    /// Live counters for the running pipeline.
    pub stats: Arc<TunTapStats>,
    /// The pipeline task. Dropping `ingress` ends it.
    pub handle: tokio::task::JoinHandle<()>,
}

/// The decode → filter → handler → model → write pipeline.
///
/// Transport-free by construction: it consumes frames from an `mpsc::Receiver` and produces
/// them into an `mpsc::Sender`. The real device fills those channels from a TUN/TAP file
/// descriptor; a test fills them itself. Both run *this* code.
pub struct TunTapEngine {
    cfg: TunTapConfig,
    interface: String,
    llm_client: OllamaClient,
    state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: ServerId,
    protocol: TunTapProtocol,
    stats: Arc<TunTapStats>,
    budget: EscalationBudget,
}

impl TunTapEngine {
    /// Build a pipeline for `interface` under `cfg`.
    pub fn new(
        cfg: TunTapConfig,
        interface: impl Into<String>,
        llm_client: OllamaClient,
        state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
    ) -> Self {
        let budget = EscalationBudget::new(cfg.llm_max_per_minute);
        Self {
            cfg,
            interface: interface.into(),
            llm_client,
            state,
            status_tx,
            server_id,
            protocol: TunTapProtocol::new(),
            stats: Arc::new(TunTapStats::default()),
            budget,
        }
    }

    /// The counters this pipeline will update.
    pub fn stats(&self) -> Arc<TunTapStats> {
        self.stats.clone()
    }

    /// Run the pipeline over a pair of channels, on its own task.
    ///
    /// This is the unprivileged entry point: it is the whole event → handler → action path
    /// with the file descriptor replaced by two `mpsc` ends, which is how the pipeline is
    /// exercised without root.
    pub fn spawn_over_channels(self) -> ChannelLink {
        let (ingress_tx, ingress_rx) = mpsc::channel::<Vec<u8>>(LINK_QUEUE_DEPTH);
        let (egress_tx, egress_rx) = mpsc::channel::<Vec<u8>>(LINK_QUEUE_DEPTH);
        let stats = self.stats();
        let handle = tokio::spawn(async move {
            self.run(ingress_rx, egress_tx).await;
        });
        ChannelLink {
            ingress: ingress_tx,
            egress: egress_rx,
            stats,
            handle,
        }
    }

    /// The pipeline itself. Returns when the ingress channel closes.
    pub async fn run(
        mut self,
        mut ingress: mpsc::Receiver<Vec<u8>>,
        egress: mpsc::Sender<Vec<u8>>,
    ) {
        self.announce_up().await;

        while let Some(frame) = ingress.recv().await {
            TunTapStats::bump(&self.stats.received);
            self.handle_frame(&frame, &egress).await;
        }

        self.announce_down("closed").await;
    }

    /// Emit `tuntap_interface_up`.
    ///
    /// Lifecycle events bypass `packet_filter`, which is about packets, and are not charged to
    /// the per-minute budget: there are at most two of them for the life of the server, and
    /// rate-limiting them would make startup non-deterministic for no benefit. They do respect
    /// `llm_escalation`, so a purely deterministic server still makes no model call.
    async fn announce_up(&mut self) {
        let data = json!({
            "interface": self.interface,
            "mode": self.cfg.mode.as_str(),
            "address": self.cfg.address,
            "netmask": self.cfg.netmask,
            "mtu": self.cfg.mtu,
            "packet_filter": self.cfg.filter.as_str(),
            "llm_escalation": self.cfg.escalation.as_str(),
            "llm_max_per_minute": self.cfg.llm_max_per_minute,
        });
        Log::new(Some(&self.status_tx)).info(format!(
            "TUN/TAP {} up ({}, {}/{}, mtu {}) {}",
            self.interface,
            self.cfg.mode.as_str(),
            self.cfg.address,
            self.cfg.netmask,
            self.cfg.mtu,
            self.cfg.escalation_summary()
        ));
        let event = Event::new(&TUNTAP_INTERFACE_UP_EVENT, data);
        self.dispatch_lifecycle(&event).await;
    }

    /// Emit `tuntap_interface_down`, carrying the run's counters.
    async fn announce_down(&mut self, reason: &str) {
        let s = &self.stats;
        let data = json!({
            "interface": self.interface,
            "reason": reason,
            "packets_received": TunTapStats::get(&s.received),
            "packets_escalated": TunTapStats::get(&s.escalated_to_llm),
            "packets_sent": TunTapStats::get(&s.sent),
        });
        Log::new(Some(&self.status_tx)).info(format!(
            "TUN/TAP {} down: {} (received={} filtered={} handled={} escalated={} sent={})",
            self.interface,
            reason,
            TunTapStats::get(&s.received),
            TunTapStats::get(&s.filtered_out),
            TunTapStats::get(&s.handled_by_rule),
            TunTapStats::get(&s.escalated_to_llm),
            TunTapStats::get(&s.sent),
        ));
        let event = Event::new(&TUNTAP_INTERFACE_DOWN_EVENT, data);
        self.dispatch_lifecycle(&event).await;
    }

    /// A lifecycle event, answered by a handler if one matches and otherwise by the model —
    /// but never at all when escalation is off and no handler claims it.
    async fn dispatch_lifecycle(&mut self, event: &Event) {
        let handled = self.a_rule_answers(event.id()).await;
        if !handled && self.cfg.escalation == LlmEscalation::Never {
            debug!(
                "TUN/TAP {} {} decision={}",
                self.interface,
                event.id(),
                Decision::LlmDisabled.as_str()
            );
            return;
        }
        if let Err(e) = call_llm(
            &self.llm_client,
            &self.state,
            self.server_id,
            None,
            event,
            &self.protocol,
        )
        .await
        {
            // Nothing to write and nothing to fabricate: the interface is a lifecycle
            // announcement, not a request. The operator hears about it; the wire does not.
            Log::new(Some(&self.status_tx)).error(format!(
                "TUN/TAP {} {} decision={}: {}",
                self.interface,
                event.id(),
                Decision::FailClosedLlmError.as_str(),
                e
            ));
        }
    }

    /// One frame, all three gates.
    async fn handle_frame(&mut self, frame: &[u8], egress: &mpsc::Sender<Vec<u8>>) {
        let decoded = match packet::decode(frame, self.cfg.packet_information, self.cfg.mode) {
            Ok(p) => p,
            Err(e) => {
                TunTapStats::bump(&self.stats.decode_errors);
                // Undecodable frames are ordinary on a real link. TRACE, not WARN: a
                // per-packet warning on a busy interface is its own denial of service.
                trace!(
                    "TUN/TAP {} could not decode a {}-byte frame: {}",
                    self.interface,
                    frame.len(),
                    e
                );
                return;
            }
        };

        // --- Gate 1: the deterministic filter. This is where the volume goes. ---
        if !self.cfg.filter.matches(&decoded) {
            TunTapStats::bump(&self.stats.filtered_out);
            trace!(
                "TUN/TAP {} filtered out: {}",
                self.interface,
                decoded.summary()
            );
            return;
        }

        // --- Gate 2: a deterministic handler, which costs no model call. ---
        let handled_by_rule = self
            .a_rule_answers(TUNTAP_PACKET_RECEIVED_EVENT.id.as_str())
            .await;

        // --- Gate 3: escalation, only for what nothing else claimed. ---
        if !handled_by_rule {
            match self.cfg.escalation {
                LlmEscalation::Never => {
                    TunTapStats::bump(&self.stats.dropped_escalation_disabled);
                    self.log_decision(Decision::LlmDisabled, &decoded, None);
                    return;
                }
                LlmEscalation::Unhandled => {
                    if !self.budget.try_take() {
                        TunTapStats::bump(&self.stats.dropped_over_budget);
                        self.log_decision(
                            Decision::FailClosedRateLimited,
                            &decoded,
                            Some(format!(
                                "already used the {} consultations this minute allows",
                                self.budget.max_per_minute()
                            )),
                        );
                        return;
                    }
                }
            }
            TunTapStats::bump(&self.stats.escalated_to_llm);
        } else {
            TunTapStats::bump(&self.stats.handled_by_rule);
        }

        let event = Event::new(&TUNTAP_PACKET_RECEIVED_EVENT, decoded.to_event_data());
        let outcome = call_llm(
            &self.llm_client,
            &self.state,
            self.server_id,
            None,
            &event,
            &self.protocol,
        )
        .await;

        let result = match outcome {
            Ok(r) => r,
            Err(e) => {
                // The whole point of the protocol: no reply is written, ever. A packet
                // NetGet cannot justify is a packet the host would treat as genuine.
                self.log_decision(
                    Decision::FailClosedLlmError,
                    &decoded,
                    Some(format!("{e:#}")),
                );
                return;
            }
        };

        for message in &result.messages {
            debug!("TUN/TAP {}: {}", self.interface, message);
        }

        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut saw_explicit_drop = false;
        for r in &result.protocol_results {
            collect_output(r, &mut frames, &mut saw_explicit_drop);
        }

        if frames.is_empty() {
            let decision = match (handled_by_rule, saw_explicit_drop) {
                (true, true) => Decision::HandlerDrop,
                (true, false) => Decision::HandlerSilent,
                (false, true) => Decision::ModelDrop,
                (false, false) => Decision::ModelSilent,
            };
            self.log_decision(decision, &decoded, None);
            return;
        }

        for mut out in frames {
            if let Err(reason) = self.prepare_for_link(&mut out) {
                TunTapStats::bump(&self.stats.refused_layer);
                self.log_decision(Decision::FailClosedBuildError, &decoded, Some(reason));
                continue;
            }
            let len = out.len();
            match egress.try_send(out) {
                Ok(()) => {
                    TunTapStats::bump(&self.stats.sent);
                    self.log_decision(
                        if handled_by_rule {
                            Decision::HandlerSend
                        } else {
                            Decision::ModelSend
                        },
                        &decoded,
                        Some(format!("{len} bytes")),
                    );
                }
                Err(e) => {
                    // The link is backed up. Dropping is right: see LINK_QUEUE_DEPTH.
                    TunTapStats::bump(&self.stats.refused_builds);
                    self.log_decision(
                        Decision::FailClosedBuildError,
                        &decoded,
                        Some(format!(
                            "could not queue {len} bytes for the interface: {e}"
                        )),
                    );
                }
            }
        }
    }

    /// Bring an answer's bytes to what this interface actually accepts.
    ///
    /// `build_packet` is mode-agnostic — it emits an Ethernet frame when the answer named both
    /// MACs and a bare IP packet otherwise — so the check that the answer matches the
    /// interface's layer belongs here, where the layer is known. A mismatch is refused rather
    /// than corrected: silently stripping or inventing a link header would put a packet on the
    /// wire that nobody described.
    fn prepare_for_link(&self, out: &mut Vec<u8>) -> Result<(), String> {
        match self.cfg.mode {
            LinkMode::Tun => {
                match out.first().map(|b| b >> 4) {
                    Some(4) | Some(6) => {}
                    _ => {
                        return Err(
                            "this is a TUN (layer 3) interface, so send_packet must not carry \
                             source_mac/destination_mac — the answer produced an Ethernet frame"
                                .to_string(),
                        )
                    }
                }
                self.cfg
                    .packet_information
                    .prepend(out)
                    .map_err(|e| e.to_string())
            }
            LinkMode::Tap => {
                if out.len() < packet::ETHERNET_HEADER_LEN {
                    return Err("an Ethernet frame needs at least 14 bytes".to_string());
                }
                let ethertype = u16::from_be_bytes([out[12], out[13]]);
                if ethertype != packet::ETHERTYPE_IPV4 && ethertype != packet::ETHERTYPE_IPV6 {
                    return Err(
                        "this is a TAP (layer 2) interface, so send_packet must carry both \
                         source_mac and destination_mac — the answer produced a bare IP packet"
                            .to_string(),
                    );
                }
                Ok(())
            }
        }
    }

    /// Does a deterministic rule claim this event?
    ///
    /// Gate 2. A script, static or manual rule answers in-process and costs no model call, so
    /// it is exempt from the escalation budget — that is the whole reason the budget can be
    /// set as low as it is. An explicit `{"type":"llm"}` rule is *not* exempt: it asks for the
    /// model by name, and asking by name does not raise the ceiling.
    async fn a_rule_answers(&self, event_type_id: &str) -> bool {
        match self.state.get_event_handler_config(self.server_id).await {
            Some(config) => matches!(
                config.find_handler(event_type_id),
                Some(EventHandlerType::Script { .. })
                    | Some(EventHandlerType::Static { .. })
                    | Some(EventHandlerType::Manual { .. })
            ),
            None => false,
        }
    }

    /// One line per packet NetGet made a decision about.
    ///
    /// The fail-closed decisions are loud, because they are the ones where NetGet was asked
    /// and said nothing; the rest are DEBUG, because an interface produces far too many of
    /// them for INFO to remain readable.
    fn log_decision(&self, decision: Decision, packet: &DecodedPacket, detail: Option<String>) {
        let mut line = format!(
            "TUN/TAP {} {} decision={}",
            self.interface,
            packet.summary(),
            decision.as_str()
        );
        if let Some(detail) = detail {
            line.push_str(&format!(" ({detail})"));
        }
        let log = Log::new(Some(&self.status_tx));
        if decision.is_fail_closed() {
            log.error(format!("{line} — nothing was written to the interface"));
        } else {
            log.debug(line);
        }
    }
}

/// Pull writable frames out of an action result, noting whether a drop was explicit.
fn collect_output(result: &ActionResult, frames: &mut Vec<Vec<u8>>, saw_drop: &mut bool) {
    match result {
        ActionResult::Output(bytes) => frames.push(bytes.clone()),
        ActionResult::NoAction => *saw_drop = true,
        ActionResult::Multiple(inner) => {
            for r in inner {
                collect_output(r, frames, saw_drop);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The real transport
// ---------------------------------------------------------------------------

/// Creates the interface and wires it to a [`TunTapEngine`].
pub struct TunTapServer;

impl TunTapServer {
    /// Create the TUN/TAP device and start the pipeline.
    ///
    /// **This function awaits readiness and returns `Err` when the device cannot be created.**
    /// That is not a detail: creating an interface fails without root, and a server sitting in
    /// `Running` with no interface is exactly the ARP/DataLink defect the root `CLAUDE.md`
    /// records. `server_startup` sets `ServerStatus::Error` from this `Err`.
    ///
    /// **Never executed by the test suite** — see the module docs and
    /// `src/server/tuntap/CLAUDE.md`. Everything below `run()` is exercised over channels;
    /// this function is not.
    pub async fn spawn_with_llm_actions(ctx: SpawnContext) -> Result<SocketAddr> {
        let cfg = TunTapConfig::from_params(&ctx.startup_params)?;
        let listen_addr = ctx.legacy_listen_addr();

        if cfg.mode == LinkMode::Tap && cfg!(target_os = "macos") {
            // Refuse, do not downgrade. macOS `utun` is layer 3 only; there is no TAP device
            // without a third-party kext, and quietly handing back a TUN interface would mean
            // every Ethernet frame the operator expected simply never appeared.
            return Err(anyhow!(
                "mode \"tap\" is not available on macOS: the utun driver is layer 3 only and \
                 macOS ships no TAP device. Use mode \"tun\", or run on Linux where \
                 /dev/net/tun supports both."
            ));
        }

        let log = Log::new(Some(&ctx.status_tx));
        log.info(format!(
            "TUN/TAP creating a {} interface {} at {}/{} (mtu {}) — requires root",
            cfg.mode.as_str(),
            cfg.interface_name.as_deref().unwrap_or("(platform choice)"),
            cfg.address,
            cfg.netmask,
            cfg.mtu
        ));

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<String>>();
        let (ingress_tx, ingress_rx) = mpsc::channel::<Vec<u8>>(LINK_QUEUE_DEPTH);
        let (egress_tx, mut egress_rx) = mpsc::channel::<Vec<u8>>(LINK_QUEUE_DEPTH);

        // Device creation and the read loop are blocking: `tun` exposes a file descriptor, not
        // a future. `recv_timeout` bounds the read so the stop signal is noticed on an idle
        // interface, exactly as datalink uses pcap's read timeout.
        let stop = crate::utils::StopSignal::new();
        let stop_in_loop = stop.clone();
        let device_cfg = cfg.clone();
        let mtu = cfg.mtu;
        tokio::task::spawn_blocking(move || {
            let opened = (|| -> Result<(tun::Reader, tun::Writer, String)> {
                use tun::AbstractDevice;

                let mut configuration = tun::configure();
                configuration
                    .address(
                        device_cfg
                            .address
                            .parse::<std::net::IpAddr>()
                            .context("interface address")?,
                    )
                    .netmask(
                        device_cfg
                            .netmask
                            .parse::<std::net::IpAddr>()
                            .context("interface netmask")?,
                    )
                    .mtu(device_cfg.mtu)
                    .layer(match device_cfg.mode {
                        LinkMode::Tun => tun::Layer::L3,
                        LinkMode::Tap => tun::Layer::L2,
                    })
                    .up();
                if let Some(name) = &device_cfg.interface_name {
                    configuration.tun_name(name);
                }

                let device = tun::create(&configuration).context(
                    "could not create the TUN/TAP device (this needs root: /dev/net/tun on \
                     Linux, the utun control socket on macOS)",
                )?;
                let name = device
                    .tun_name()
                    .unwrap_or_else(|_| "(unnamed)".to_string());
                let (reader, writer) = device.split();
                Ok((reader, writer, name))
            })();

            let (reader, mut writer, name) = match opened {
                Ok(v) => {
                    let _ = ready_tx.send(Ok(v.2.clone()));
                    v
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            // The writer half runs on its own blocking thread so a slow write cannot stall the
            // read loop, and neither can block the async runtime.
            std::thread::spawn(move || {
                use std::io::Write;
                while let Some(frame) = egress_rx.blocking_recv() {
                    if let Err(e) = writer.write_all(&frame) {
                        tracing::error!("TUN/TAP write failed: {}", e);
                        break;
                    }
                }
            });

            let mut buf = vec![0u8; mtu as usize + packet::PACKET_INFORMATION_LEN + 64];
            loop {
                if stop_in_loop.is_stopped() {
                    break;
                }
                match reader.recv_timeout(&mut buf, READ_POLL) {
                    Ok(0) => continue,
                    Ok(n) => {
                        // try_send, not blocking_send: a full queue means the pipeline is
                        // behind, and an interface drops rather than applying backpressure to
                        // the kernel.
                        if ingress_tx.try_send(buf[..n].to_vec()).is_err() {
                            tracing::trace!("TUN/TAP {} dropped a frame: pipeline busy", name);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::error!("TUN/TAP {} read failed: {}", name, e);
                        break;
                    }
                }
            }
        });

        let interface = match ready_rx.await {
            Ok(Ok(name)) => name,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(anyhow!(
                    "the TUN/TAP device task exited before signalling readiness"
                ))
            }
        };

        let engine = TunTapEngine::new(
            cfg,
            interface.clone(),
            ctx.llm_client,
            ctx.state.clone(),
            ctx.status_tx.clone(),
            ctx.server_id,
        );
        tokio::spawn(async move {
            engine.run(ingress_rx, egress_tx).await;
        });

        // Registered only now that the device is genuinely up: `stop_server` aborts this
        // parked task, which trips `stop` and ends the blocking read loop.
        ctx.state
            .register_server_task(ctx.server_id, stop.park_task())
            .await;

        Log::new(Some(&ctx.status_tx)).info(format!("TUN/TAP interface {interface} is up"));

        Ok(listen_addr)
    }
}
