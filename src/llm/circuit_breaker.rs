//! Circuit breaker for the LLM backend transport.
//!
//! Without one, an unreachable backend costs every request the full request timeout
//! independently. With the rate limiter's `max_concurrent: 1`, N pending connections
//! serialise into N × timeout before the first peer learns anything is wrong — a 120s
//! timeout and ten queued connections is twenty minutes of a server that looks `Running`.
//!
//! The breaker turns that into "the first K requests pay the timeout, everything after fails
//! in microseconds until the backend is worth trying again".
//!
//! # States
//!
//! * **Closed** — requests pass. Consecutive transport failures are counted.
//! * **Open** — reached after `failure_threshold` consecutive transport failures. Requests
//!   fail immediately for `cooldown`.
//! * **Half-open** — after the cooldown, the next request is let through as a probe. It
//!   closes the breaker if it succeeds and re-opens it for another cooldown if it does not.
//!
//! # What counts as a failure
//!
//! Only *transport* failures: timeouts, refused connections, DNS and socket errors. A
//! backend that answers "model not found" or "401 unauthorized" is reachable — that is an
//! application error, and it resets the counter rather than advancing it. See
//! [`is_transport_failure`].

use crate::utils::clock::Instant;
use std::sync::Mutex;
use std::time::Duration;

/// Consecutive transport failures before the breaker opens.
///
/// One failure is a blip — a model load, a restart, a dropped packet — and tripping on it
/// would make the system fragile. Three is enough to be confident the backend is down while
/// costing at most 3 × timeout to discover it; every request after that is free.
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 3;

/// How long the breaker stays open before letting a probe through.
///
/// Short enough that a backend which comes back up is picked up within one connection's
/// patience, long enough that a genuinely dead backend is probed twice a minute rather than
/// on every request.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// Observable state of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Requests pass through.
    Closed,
    /// Requests fail immediately.
    Open,
    /// The next request is a probe.
    HalfOpen,
}

impl std::fmt::Display for BreakerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BreakerState::Closed => write!(f, "closed"),
            BreakerState::Open => write!(f, "open"),
            BreakerState::HalfOpen => write!(f, "half-open"),
        }
    }
}

/// A snapshot of the breaker, for status displays and diagnostics.
#[derive(Debug, Clone)]
pub struct BreakerStatus {
    /// Current state.
    pub state: BreakerState,
    /// Consecutive transport failures recorded so far.
    pub consecutive_failures: u32,
    /// How long until the next probe is allowed (`None` unless open).
    pub retry_in: Option<Duration>,
    /// How many times the breaker has opened since it was created.
    pub trips: u64,
    /// The failure that opened it, for display.
    pub last_error: Option<String>,
}

impl BreakerStatus {
    /// Whether requests are currently being rejected without being attempted.
    pub fn is_open(&self) -> bool {
        self.state == BreakerState::Open
    }

    /// One-line summary suitable for a server status line or a log.
    pub fn summary(&self) -> String {
        match self.state {
            BreakerState::Closed => "LLM backend reachable".to_string(),
            BreakerState::HalfOpen => format!(
                "LLM backend circuit breaker half-open after {} trip(s): next request is a probe",
                self.trips
            ),
            BreakerState::Open => format!(
                "LLM backend circuit breaker OPEN after {} consecutive transport failure(s); \
                 failing fast for another {}s (last error: {})",
                self.consecutive_failures,
                self.retry_in.map(|d| d.as_secs()).unwrap_or(0),
                self.last_error.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

#[derive(Debug)]
struct Inner {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    trips: u64,
    last_error: Option<String>,
}

/// See the module documentation.
#[derive(Debug)]
pub struct CircuitBreaker {
    failure_threshold: u32,
    cooldown: Duration,
    inner: Mutex<Inner>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(DEFAULT_FAILURE_THRESHOLD, DEFAULT_COOLDOWN)
    }
}

impl CircuitBreaker {
    /// Create a breaker. A `failure_threshold` of 0 is treated as 1.
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            cooldown,
            inner: Mutex::new(Inner {
                consecutive_failures: 0,
                opened_at: None,
                trips: 0,
                last_error: None,
            }),
        }
    }

    /// Ask permission to make a request.
    ///
    /// Returns `Err` immediately — no I/O, no waiting — while the breaker is open. Once the
    /// cooldown has elapsed the call is allowed through as a probe, and the caller is
    /// expected to report the outcome via [`Self::record_success`] or
    /// [`Self::record_failure`].
    pub fn acquire(&self) -> Result<(), BreakerOpen> {
        let mut inner = self.lock();
        let Some(opened_at) = inner.opened_at else {
            return Ok(());
        };

        let elapsed = opened_at.elapsed();
        if elapsed < self.cooldown {
            return Err(BreakerOpen {
                consecutive_failures: inner.consecutive_failures,
                retry_in: self.cooldown - elapsed,
                last_error: inner.last_error.clone(),
            });
        }

        // Cooldown expired: let one request through as a probe. The failure count is left
        // at or above the threshold, so a failing probe re-opens the breaker immediately.
        inner.opened_at = None;
        Ok(())
    }

    /// Record that a request reached the backend and got an answer.
    ///
    /// Also the right call for an *application* error (unknown model, bad credentials): the
    /// backend is demonstrably reachable, which is all this breaker tracks.
    pub fn record_success(&self) {
        let mut inner = self.lock();
        inner.consecutive_failures = 0;
        inner.opened_at = None;
        inner.last_error = None;
    }

    /// Record a transport failure. Opens the breaker once the threshold is reached.
    ///
    /// Returns `true` if this failure opened the breaker (as opposed to it already being
    /// open or still below the threshold), so the caller can log the transition once.
    pub fn record_failure(&self, error: &str) -> bool {
        let mut inner = self.lock();
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        inner.last_error = Some(error.to_string());

        if inner.consecutive_failures >= self.failure_threshold && inner.opened_at.is_none() {
            inner.opened_at = Some(Instant::now());
            inner.trips += 1;
            return true;
        }
        false
    }

    /// Whether requests are currently rejected without being attempted.
    pub fn is_open(&self) -> bool {
        self.status().is_open()
    }

    /// Snapshot for status displays.
    pub fn status(&self) -> BreakerStatus {
        let inner = self.lock();
        let (state, retry_in) = match inner.opened_at {
            None if inner.consecutive_failures >= self.failure_threshold => {
                (BreakerState::HalfOpen, None)
            }
            None => (BreakerState::Closed, None),
            Some(opened_at) => {
                let elapsed = opened_at.elapsed();
                if elapsed < self.cooldown {
                    (BreakerState::Open, Some(self.cooldown - elapsed))
                } else {
                    (BreakerState::HalfOpen, None)
                }
            }
        };

        BreakerStatus {
            state,
            consecutive_failures: inner.consecutive_failures,
            retry_in,
            trips: inner.trips,
            last_error: inner.last_error.clone(),
        }
    }

    /// Force the breaker closed (used when the backend is reconfigured).
    pub fn reset(&self) {
        self.record_success();
    }

    /// Configured failure threshold.
    pub fn failure_threshold(&self) -> u32 {
        self.failure_threshold
    }

    /// Configured cooldown.
    pub fn cooldown(&self) -> Duration {
        self.cooldown
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means some other thread panicked while holding it; the
        // counters are plain integers, so the state is still coherent.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Error returned by [`CircuitBreaker::acquire`] while the breaker is open.
#[derive(Debug, Clone)]
pub struct BreakerOpen {
    /// Consecutive transport failures that opened it.
    pub consecutive_failures: u32,
    /// Time remaining before the next probe is allowed.
    pub retry_in: Duration,
    /// The failure that opened it.
    pub last_error: Option<String>,
}

impl std::fmt::Display for BreakerOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "✗  LLM backend is unreachable: circuit breaker open after {} consecutive \
             transport failure(s). Failing fast instead of waiting for another timeout; the \
             next probe is in {}s.{}",
            self.consecutive_failures,
            self.retry_in.as_secs().max(1),
            self.last_error
                .as_deref()
                .map(|e| format!("\n   Last error: {}", e))
                .unwrap_or_default()
        )
    }
}

impl std::error::Error for BreakerOpen {}

/// Whether an error means the backend could not be *reached*, as opposed to answering with
/// an application-level error.
///
/// A reachable backend that rejects the request (unknown model, bad key, rate limit) proves
/// the transport works, so those must not trip the breaker — otherwise a typo'd model name
/// would take the whole LLM path offline for a cooldown.
pub fn is_transport_failure(error: &anyhow::Error) -> bool {
    let text = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
        .to_lowercase();

    // Answered-but-rejected: the backend was reached.
    const APPLICATION_ERRORS: &[&str] = &[
        "model not found",
        "authentication failed",
        "rate limited",
        "failed to parse",
        "agent-queue",
    ];
    if APPLICATION_ERRORS.iter().any(|m| text.contains(m)) {
        return false;
    }

    const TRANSPORT_ERRORS: &[&str] = &[
        "timed out",
        "timeout",
        "connection",
        "refused",
        "cannot connect",
        "failed to connect",
        "error sending request",
        "request failed",
        "dns error",
        "broken pipe",
        "network is unreachable",
        "no route to host",
        "os error 61",  // ECONNREFUSED on macOS
        "os error 111", // ECONNREFUSED on Linux
    ];
    TRANSPORT_ERRORS.iter().any(|m| text.contains(m))
}
