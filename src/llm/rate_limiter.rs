//! LLM rate limiter with concurrency control and token-based throttling
//!
//! This module provides a rate limiter that controls:
//! 1. **Concurrency**: Maximum number of concurrent LLM requests
//! 2. **Token usage**: Maximum tokens per time window (for API usage control)
//!
//! # Concurrency is a queue, not a filter
//!
//! `max_concurrent` bounds how many LLM calls run *at once*; it does not decide
//! which requests are worth serving. A request that arrives while the limit is
//! saturated **waits for a permit** — including network-sourced ones. This is
//! what `--llm-max-concurrent 1` ("sequential processing") has always claimed to
//! mean, and it is now what it does.
//!
//! Network requests used to `try_acquire` and fail instantly instead, which in
//! the shipped default configuration (`max_concurrent: 1`) meant that a second
//! simultaneous connection had its LLM call refused, its handler returned `Err`,
//! and most protocols write nothing on that path — so the peer hung until its own
//! timeout with nothing on the wire. Two concurrent `curl`s were enough.
//!
//! The wait is bounded in **two** dimensions, because an unbounded queue of
//! network requests is its own denial of service (a slow backend plus a burst of
//! connections becomes unbounded memory, and every peer waits forever):
//!
//! - [`RateLimiterConfig::max_queued`] caps how many network requests may be
//!   waiting at once. Past that the limiter fails fast, so the overload is
//!   reported at the edge instead of being absorbed into RAM.
//! - [`RateLimiterConfig::queue_timeout_secs`] caps how long any one of them
//!   waits. It defaults to one backend request timeout: with a healthy backend a
//!   permit frees within at most one in-flight call, so exceeding it means the
//!   queue ahead is genuinely deeper than the system can drain.
//!
//! Both failures surface as a typed [`RateLimitError`], so a protocol can tell
//! "the system is overloaded" apart from "the model said something unusable" and
//! answer the peer accordingly (HTTP does: 503 rather than 500). Use
//! [`is_overload_error`] to test an [`anyhow::Error`] for it.
//!
//! Token-limit exhaustion is different in kind and still discards network
//! requests: it is a *budget* that will not free up on the timescale of a
//! request/response exchange, so waiting for it would only convert a fast refusal
//! into a slow one.

use crate::utils::clock::Instant;
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{debug, info, warn};

/// Default bound on how long a network-sourced request waits for a concurrency
/// permit.
///
/// Deliberately equal to [`crate::llm::ollama_client::DEFAULT_REQUEST_TIMEOUT`]:
/// the request holding the permit cannot itself run longer than that, so a
/// shorter wait would reject requests that were about to be served, and a longer
/// one would keep queueing behind a backlog the backend is not draining.
///
/// *Derived* from it rather than restated, because the equality is load-bearing and a
/// hand-copied `120` silently stopped being equal the moment the request timeout moved.
pub const DEFAULT_QUEUE_TIMEOUT_SECS: u64 =
    crate::llm::ollama_client::DEFAULT_REQUEST_TIMEOUT.as_secs();

/// Default bound on how many network-sourced requests may wait for a permit at
/// once. Beyond this the limiter fails fast rather than growing the queue.
pub const DEFAULT_MAX_QUEUED: usize = 128;

/// Source of the LLM request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestSource {
    /// User input (waits for capacity, unbounded)
    User,
    /// Network event (waits for a concurrency permit within the configured
    /// bounds; discarded outright only when the token budget is exhausted)
    Network,
}

/// Why the rate limiter refused a request.
///
/// Carried as the source of the [`anyhow::Error`] returned by
/// [`RateLimiter::acquire_permit`] so callers can distinguish an overloaded
/// system from a failed LLM call. See [`is_overload_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitError {
    /// Waited the full [`RateLimiterConfig::queue_timeout_secs`] without a
    /// concurrency permit becoming available.
    QueueTimeout {
        /// How long the request waited, in seconds.
        waited_secs: u64,
    },
    /// Too many network requests were already waiting for a permit.
    QueueFull {
        /// The configured [`RateLimiterConfig::max_queued`].
        max_queued: usize,
    },
    /// The token budget for the current window is exhausted.
    TokenLimit {
        /// The configured token limit.
        limit: u64,
        /// The window the limit applies to, in seconds.
        window_secs: u64,
    },
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueTimeout { waited_secs } => write!(
                f,
                "LLM backend overloaded: waited {}s for a concurrency permit without one becoming available",
                waited_secs
            ),
            Self::QueueFull { max_queued } => write!(
                f,
                "LLM backend overloaded: {} requests already queued for a concurrency permit",
                max_queued
            ),
            Self::TokenLimit { limit, window_secs } => write!(
                f,
                "LLM token budget exhausted: {} tokens per {}s window",
                limit, window_secs
            ),
        }
    }
}

impl std::error::Error for RateLimitError {}

/// Whether `err` (or anything in its context chain) is a rate-limiter refusal.
///
/// Protocols use this to answer a peer with an overload status instead of a
/// generic failure — the distinction matters because an overload is transient
/// and retryable while a malformed model answer is not.
pub fn is_overload_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RateLimitError>().is_some()
}

/// Token usage record with timestamp
#[derive(Debug, Clone)]
struct TokenUsage {
    timestamp: Instant,
    input_tokens: u64,
    output_tokens: u64,
}

/// Rate limiter configuration
#[derive(Debug, Clone)]
pub struct RateLimiterConfig {
    /// Maximum concurrent LLM requests (default: 1)
    pub max_concurrent: usize,

    /// Maximum tokens per time window (None = unlimited)
    /// Default: None for local Ollama, recommended: 10000 for cloud APIs
    pub token_limit: Option<u64>,

    /// Time window for token limiting in seconds (default: 60)
    pub token_window_secs: u64,

    /// How long a network-sourced request waits for a concurrency permit before
    /// giving up (default: [`DEFAULT_QUEUE_TIMEOUT_SECS`]). `0` means wait
    /// forever — only sane when the peer has its own timeout and you would
    /// rather it drove the deadline.
    pub queue_timeout_secs: u64,

    /// How many network-sourced requests may wait for a concurrency permit at
    /// once (default: [`DEFAULT_MAX_QUEUED`]). `0` means unbounded, which trades
    /// the memory bound away; prefer a large number to zero.
    pub max_queued: usize,
}

impl Default for RateLimiterConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            token_limit: None,
            token_window_secs: 60,
            queue_timeout_secs: DEFAULT_QUEUE_TIMEOUT_SECS,
            max_queued: DEFAULT_MAX_QUEUED,
        }
    }
}

/// LLM rate limiter with concurrency and token-based throttling
#[derive(Clone)]
pub struct RateLimiter {
    /// Concurrency control (wrapped in RwLock to allow semaphore replacement)
    semaphore: Arc<RwLock<Arc<Semaphore>>>,

    /// Configuration (can be updated at runtime)
    config: Arc<RwLock<RateLimiterConfig>>,

    /// Token usage history (protected by mutex for interior mutability)
    token_usage: Arc<Mutex<Vec<TokenUsage>>>,

    /// Network-sourced requests currently waiting for a concurrency permit.
    /// Kept outside `stats` so the queue-depth check on the hot path never
    /// contends with the stats mutex.
    queued_network: Arc<AtomicUsize>,

    /// Statistics
    stats: Arc<Mutex<RateLimiterStats>>,
}

/// Decrements the queue-depth counter however the wait ends — permit acquired,
/// timed out, or task cancelled mid-`await`. A plain `fetch_sub` after the await
/// would leak the slot on cancellation, and a leaked slot is permanent: the
/// limiter would refuse ever more requests as `QueueFull` with nothing actually
/// queued.
struct QueueSlot(Arc<AtomicUsize>);

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Rate limiter statistics
#[derive(Debug, Default, Clone)]
pub struct RateLimiterStats {
    /// Total requests attempted
    pub total_requests: u64,

    /// Requests completed
    pub requests_completed: u64,

    /// Requests refused outright (token budget exhausted, or the wait queue was
    /// already full)
    pub requests_discarded: u64,

    /// Requests waiting (user inputs that are queued)
    pub requests_waiting: u64,

    /// Network requests that had to wait for a concurrency permit rather than
    /// getting one immediately
    pub requests_queued: u64,

    /// Network requests that gave up after waiting the full queue timeout
    pub requests_queue_timed_out: u64,

    /// Network requests waiting for a concurrency permit right now
    pub currently_queued: u64,

    /// Total input tokens processed
    pub total_input_tokens: u64,

    /// Total output tokens processed
    pub total_output_tokens: u64,

    /// Current tokens in window
    pub current_window_tokens: u64,
}

impl RateLimiter {
    /// Create a new rate limiter with the given configuration
    pub fn new(config: RateLimiterConfig) -> Self {
        let max_concurrent = config.max_concurrent;

        Self {
            semaphore: Arc::new(RwLock::new(Arc::new(Semaphore::new(max_concurrent)))),
            config: Arc::new(RwLock::new(config)),
            token_usage: Arc::new(Mutex::new(Vec::new())),
            queued_network: Arc::new(AtomicUsize::new(0)),
            stats: Arc::new(Mutex::new(RateLimiterStats::default())),
        }
    }

    /// Update the rate limiter configuration at runtime
    pub async fn update_config(&self, config: RateLimiterConfig) -> Result<()> {
        let mut current_config = self.config.write().await;

        // If max_concurrent changed, recreate the semaphore
        if config.max_concurrent != current_config.max_concurrent {
            info!(
                "Concurrency limit changed from {} to {} - recreating semaphore",
                current_config.max_concurrent, config.max_concurrent
            );

            // Replace the semaphore with a new one
            let mut semaphore = self.semaphore.write().await;
            *semaphore = Arc::new(Semaphore::new(config.max_concurrent));

            debug!("Semaphore recreated with {} permits", config.max_concurrent);
        }

        *current_config = config;
        info!(
            "Rate limiter config updated: max_concurrent={}, token_limit={:?}, window={}s, queue_timeout={}s, max_queued={}",
            current_config.max_concurrent,
            current_config.token_limit,
            current_config.token_window_secs,
            current_config.queue_timeout_secs,
            current_config.max_queued
        );

        Ok(())
    }

    /// Get current configuration
    pub async fn get_config(&self) -> RateLimiterConfig {
        self.config.read().await.clone()
    }

    /// Get current statistics
    pub async fn get_stats(&self) -> RateLimiterStats {
        // Update current window tokens before returning stats
        let config = self.config.read().await;
        let mut stats = self.stats.lock().await;

        // Calculate tokens in current window
        let cutoff = Instant::now() - Duration::from_secs(config.token_window_secs);
        let token_usage = self.token_usage.lock().await;
        let window_tokens: u64 = token_usage
            .iter()
            .filter(|usage| usage.timestamp >= cutoff)
            .map(|usage| usage.input_tokens + usage.output_tokens)
            .sum();

        stats.current_window_tokens = window_tokens;
        stats.currently_queued = self.queued_network.load(Ordering::Acquire) as u64;
        stats.clone()
    }

    /// Drop token-usage records that have fallen out of the current window.
    ///
    /// Called unconditionally rather than only on the token-limited path: the
    /// history is appended to on *every* LLM call, so skipping the prune when
    /// `token_limit` is `None` — the default, and therefore what every long-lived
    /// `netget --mcp` runs — grew the vector for the life of the process and made
    /// `get_stats()` scan all of it.
    async fn prune_token_usage(&self, window_secs: u64) {
        let cutoff = Instant::now() - Duration::from_secs(window_secs);
        self.token_usage
            .lock()
            .await
            .retain(|usage| usage.timestamp >= cutoff);
    }

    /// Check if we have token capacity available
    async fn check_token_capacity(&self) -> Result<bool> {
        let config = self.config.read().await;
        let window_secs = config.token_window_secs;

        // Prune before the early return, not after it.
        self.prune_token_usage(window_secs).await;

        // If no token limit, always have capacity
        let Some(token_limit) = config.token_limit else {
            return Ok(true);
        };

        let token_usage = self.token_usage.lock().await;

        // Calculate total tokens in current window
        let window_tokens: u64 = token_usage
            .iter()
            .map(|usage| usage.input_tokens + usage.output_tokens)
            .sum();

        debug!(
            "Token usage check: {}/{} tokens in {}s window",
            window_tokens, token_limit, config.token_window_secs
        );

        Ok(window_tokens < token_limit)
    }

    /// Record token usage after a successful LLM call
    pub async fn record_token_usage(&self, input_tokens: u64, output_tokens: u64) {
        let usage = TokenUsage {
            timestamp: Instant::now(),
            input_tokens,
            output_tokens,
        };

        // Add to history, then drop anything that has aged out of the window.
        // Pruning here (and not only in `check_token_capacity`) is what actually
        // bounds the vector: with the default `token_limit: None` the capacity
        // check returns before it ever needs the history.
        let window_secs = self.config.read().await.token_window_secs;
        self.token_usage.lock().await.push(usage);
        self.prune_token_usage(window_secs).await;

        // Update stats
        let mut stats = self.stats.lock().await;
        stats.total_input_tokens += input_tokens;
        stats.total_output_tokens += output_tokens;
        stats.requests_completed += 1;

        debug!(
            "Recorded token usage: input={}, output={}, total_requests={}",
            input_tokens, output_tokens, stats.requests_completed
        );
    }

    /// Number of token-usage records currently retained.
    ///
    /// Exposed so tests can pin the pruning behaviour; the history is otherwise
    /// an implementation detail.
    pub async fn token_history_len(&self) -> usize {
        self.token_usage.lock().await.len()
    }

    /// Acquire a permit for an LLM request
    ///
    /// - For `RequestSource::User`: waits until capacity is available, without a
    ///   deadline. A human sitting at the TUI would rather wait than be refused.
    /// - For `RequestSource::Network`: waits for a concurrency permit within the
    ///   configured queue bounds, so `max_concurrent: 1` serializes requests
    ///   instead of dropping them. Refused with a [`RateLimitError`] when the
    ///   queue is full, the wait times out, or the token budget is exhausted.
    ///
    /// Returns a permit guard that must be held for the duration of the LLM call.
    /// The permit is automatically released when the guard is dropped.
    pub async fn acquire_permit(&self, source: RequestSource) -> Result<RateLimiterPermit> {
        // Update stats
        {
            let mut stats = self.stats.lock().await;
            stats.total_requests += 1;
            if source == RequestSource::User {
                stats.requests_waiting += 1;
            }
        }

        debug!("Acquiring rate limiter permit for {:?} request", source);

        // Check token capacity
        let has_token_capacity = self.check_token_capacity().await?;

        if !has_token_capacity {
            match source {
                RequestSource::User => {
                    // For user requests, wait for token capacity
                    info!("Token limit reached, waiting for capacity (user request)");

                    // Poll until we have capacity (with exponential backoff)
                    let mut delay = Duration::from_millis(100);
                    while !self.check_token_capacity().await? {
                        tokio::time::sleep(delay).await;
                        delay = std::cmp::min(delay * 2, Duration::from_secs(5));
                    }

                    info!("Token capacity available, proceeding with user request");
                }
                RequestSource::Network => {
                    // Token exhaustion is a budget, not congestion: it will not
                    // clear on the timescale of one request, so waiting would
                    // only turn a fast refusal into a slow one.
                    let mut stats = self.stats.lock().await;
                    stats.requests_discarded += 1;

                    let config = self.config.read().await;
                    warn!(
                        "Discarding network event: Token limit ({}/{} tokens in {}s window)",
                        self.token_usage
                            .lock()
                            .await
                            .iter()
                            .map(|u| u.input_tokens + u.output_tokens)
                            .sum::<u64>(),
                        config.token_limit.unwrap_or(0),
                        config.token_window_secs
                    );

                    return Err(anyhow::Error::new(RateLimitError::TokenLimit {
                        limit: config.token_limit.unwrap_or(0),
                        window_secs: config.token_window_secs,
                    }));
                }
            }
        }

        // Snapshot the semaphore and the queue bounds, then drop both guards
        // before awaiting: holding the config/semaphore RwLock across the wait
        // would block `update_config` (and every other reader) for the whole
        // duration of an LLM call.
        let semaphore = {
            let guard = self.semaphore.read().await;
            Arc::clone(&guard)
        };
        let (queue_timeout_secs, max_queued, max_concurrent) = {
            let config = self.config.read().await;
            (
                config.queue_timeout_secs,
                config.max_queued,
                config.max_concurrent,
            )
        };

        // Acquire concurrency permit
        let permit = match source {
            RequestSource::User => {
                // For user requests, wait for permit
                debug!("Waiting for concurrency permit (user request)");
                semaphore
                    .acquire_owned()
                    .await
                    .context("Failed to acquire semaphore permit")?
            }
            RequestSource::Network => {
                // Fast path: capacity available right now, nothing to queue.
                match Arc::clone(&semaphore).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        // Saturated. Queue, within bounds.
                        let depth = self.queued_network.fetch_add(1, Ordering::AcqRel) + 1;
                        let _slot = QueueSlot(Arc::clone(&self.queued_network));

                        if max_queued > 0 && depth > max_queued {
                            let mut stats = self.stats.lock().await;
                            stats.requests_discarded += 1;
                            warn!(
                                "Refusing network event: {} requests already waiting for one of {} concurrency permits",
                                max_queued, max_concurrent
                            );
                            return Err(anyhow::Error::new(RateLimitError::QueueFull {
                                max_queued,
                            }));
                        }

                        {
                            let mut stats = self.stats.lock().await;
                            stats.requests_queued += 1;
                        }
                        debug!(
                            "Network event queued for a concurrency permit ({} waiting, max_concurrent={})",
                            depth, max_concurrent
                        );

                        let acquire = semaphore.acquire_owned();
                        let acquired = if queue_timeout_secs == 0 {
                            acquire.await.map(Some).map_err(anyhow::Error::from)
                        } else {
                            match tokio::time::timeout(
                                Duration::from_secs(queue_timeout_secs),
                                acquire,
                            )
                            .await
                            {
                                Ok(result) => result.map(Some).map_err(anyhow::Error::from),
                                Err(_elapsed) => Ok(None),
                            }
                        };

                        match acquired.context("Failed to acquire semaphore permit")? {
                            Some(permit) => permit,
                            None => {
                                let mut stats = self.stats.lock().await;
                                stats.requests_queue_timed_out += 1;
                                warn!(
                                    "Refusing network event: waited {}s for one of {} concurrency permits",
                                    queue_timeout_secs, max_concurrent
                                );
                                return Err(anyhow::Error::new(RateLimitError::QueueTimeout {
                                    waited_secs: queue_timeout_secs,
                                }));
                            }
                        }
                    }
                }
            }
        };

        // Update stats
        {
            let mut stats = self.stats.lock().await;
            if source == RequestSource::User {
                stats.requests_waiting = stats.requests_waiting.saturating_sub(1);
            }
        }

        debug!("Rate limiter permit acquired for {:?} request", source);

        Ok(RateLimiterPermit {
            _permit: permit,
            rate_limiter: self.clone(),
        })
    }
}

/// RAII guard for a rate limiter permit
///
/// Automatically releases the concurrency permit when dropped.
pub struct RateLimiterPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    rate_limiter: RateLimiter,
}

impl RateLimiterPermit {
    /// Record token usage for this request
    pub async fn record_usage(&self, input_tokens: u64, output_tokens: u64) {
        self.rate_limiter
            .record_token_usage(input_tokens, output_tokens)
            .await;
    }
}
