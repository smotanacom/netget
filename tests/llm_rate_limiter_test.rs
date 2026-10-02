//! Rate limiter semantics.
//!
//! These pin the *shipped default* configuration, which is the configuration no
//! E2E test exercises: `tests/helpers/netget.rs` passes
//! `--llm-max-concurrent 1000` to every spawned binary, so for a long time
//! `max_concurrent: 1` — what users actually run — was covered by nothing.
//!
//! No Ollama, no network, no binary: the limiter is driven directly.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp \
//!       --test llm_rate_limiter_test -- --test-threads=100

use netget::llm::{
    is_overload_error, RateLimitError, RateLimiter, RateLimiterConfig, RequestSource,
    DEFAULT_MAX_QUEUED, DEFAULT_QUEUE_TIMEOUT_SECS,
};
use std::time::Duration;

#[tokio::test]
async fn queued_requests_recheck_the_budget_after_the_preceding_call_finishes() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        token_limit: Some(10),
        ..Default::default()
    });
    let first = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .unwrap();
    let waiting_limiter = limiter.clone();
    let waiter = tokio::spawn(async move {
        waiting_limiter
            .acquire_permit(RequestSource::Network)
            .await
            .map(|_| ())
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while limiter.get_stats().await.currently_queued == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    first.record_usage(10, 0).await;
    drop(first);
    let error = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap()
        .expect_err("the previous call exhausted the budget");
    assert!(matches!(
        error.downcast_ref::<RateLimitError>(),
        Some(RateLimitError::TokenLimit { .. })
    ));
    assert_eq!(limiter.get_stats().await.currently_queued, 0);
    assert_eq!(limiter.get_stats().await.requests_discarded, 1);
}

#[tokio::test]
async fn extreme_backend_usage_and_window_values_do_not_overflow_or_reopen_budget() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        token_limit: Some(u64::MAX),
        token_window_secs: u64::MAX,
        ..Default::default()
    });
    limiter.record_token_usage(u64::MAX, u64::MAX).await;
    limiter.record_token_usage(1, 1).await;
    let stats = limiter.get_stats().await;
    assert_eq!(stats.current_window_tokens, u64::MAX);
    assert_eq!(stats.total_input_tokens, u64::MAX);
    assert_eq!(stats.total_output_tokens, u64::MAX);
    assert!(limiter
        .acquire_permit(RequestSource::Network)
        .await
        .is_err());
}

/// The default is what ships; assert it explicitly so a change to it is a
/// deliberate edit to this test rather than a silent behaviour change.
#[tokio::test]
async fn shipped_default_is_one_concurrent_request_with_a_bounded_queue() {
    let config = RateLimiterConfig::default();
    assert_eq!(config.max_concurrent, 1, "shipped concurrency default");
    assert_eq!(config.queue_timeout_secs, DEFAULT_QUEUE_TIMEOUT_SECS);
    assert_eq!(config.max_queued, DEFAULT_MAX_QUEUED);
    assert!(config.max_queued > 0, "the queue must be bounded");
}

/// The regression that matters: at `max_concurrent: 1` a second *network*
/// request must wait for the first to finish, not be refused.
///
/// Before the fix this test failed at the `is_err()` assertion below — the
/// second request came back instantly with "Rate limit exceeded: max concurrent
/// requests (network event discarded)".
#[tokio::test]
async fn a_second_network_request_queues_instead_of_being_dropped() {
    let limiter = RateLimiter::new(RateLimiterConfig::default());

    let first = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .expect("first network request gets the only permit");

    let waiter = {
        let limiter = limiter.clone();
        tokio::spawn(async move {
            limiter
                .acquire_permit(RequestSource::Network)
                .await
                .map(|_permit| ())
        })
    };

    // Give the waiter time to be refused, if it were going to be.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !waiter.is_finished(),
        "second network request must wait for a permit, not resolve immediately"
    );

    let stats = limiter.get_stats().await;
    assert_eq!(stats.currently_queued, 1, "it should be counted as queued");
    assert_eq!(
        stats.requests_discarded, 0,
        "waiting is not discarding — nothing should have been dropped"
    );

    drop(first);

    let result = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("queued request must be served once the permit frees")
        .expect("waiter task panicked");
    assert!(
        result.is_ok(),
        "queued network request must succeed, got {:?}",
        result.err()
    );

    let stats = limiter.get_stats().await;
    assert_eq!(stats.requests_queued, 1);
    assert_eq!(stats.currently_queued, 0);
}

/// Sequential means every request eventually runs. Six network requests against
/// one permit must all be served, in some order, with none refused.
#[tokio::test]
async fn every_concurrent_network_request_is_eventually_served() {
    let limiter = RateLimiter::new(RateLimiterConfig::default());

    let mut handles = Vec::new();
    for _ in 0..6 {
        let limiter = limiter.clone();
        handles.push(tokio::spawn(async move {
            let permit = limiter.acquire_permit(RequestSource::Network).await?;
            // Stand in for the LLM round-trip so the permits genuinely overlap.
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(permit);
            anyhow::Ok(())
        }));
    }

    for (idx, handle) in handles.into_iter().enumerate() {
        let result = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap_or_else(|_| panic!("request {idx} never completed"))
            .expect("task panicked");
        assert!(
            result.is_ok(),
            "request {idx} was refused: {:?}",
            result.err()
        );
    }

    let stats = limiter.get_stats().await;
    assert_eq!(stats.requests_discarded, 0);
    assert_eq!(stats.requests_queue_timed_out, 0);
}

/// The wait is bounded: a permit that never frees produces a typed overload
/// error, so the protocol can answer the peer rather than hanging it forever.
#[tokio::test]
async fn the_queue_wait_is_bounded_by_queue_timeout() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        max_concurrent: 1,
        queue_timeout_secs: 1,
        ..Default::default()
    });

    let _held = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .expect("first permit");

    let started = std::time::Instant::now();
    let err = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .map(|_| ())
        .expect_err("must give up rather than wait forever");
    let waited = started.elapsed();

    assert!(
        waited >= Duration::from_secs(1),
        "must actually wait the timeout, waited {waited:?}"
    );
    assert!(is_overload_error(&err), "must be a typed overload: {err:#}");
    assert!(
        matches!(
            err.downcast_ref::<RateLimitError>(),
            Some(RateLimitError::QueueTimeout { .. })
        ),
        "expected QueueTimeout, got {err:#}"
    );
    assert_eq!(limiter.get_stats().await.requests_queue_timed_out, 1);
}

/// The queue depth is bounded too: an unbounded queue of network requests is
/// unbounded memory, so past `max_queued` the limiter fails fast.
#[tokio::test]
async fn the_queue_depth_is_bounded_by_max_queued() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        max_concurrent: 1,
        max_queued: 1,
        queue_timeout_secs: 30,
        ..Default::default()
    });

    let _held = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .expect("first permit");

    let _waiter = {
        let limiter = limiter.clone();
        tokio::spawn(async move {
            limiter
                .acquire_permit(RequestSource::Network)
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(limiter.get_stats().await.currently_queued, 1);

    let started = std::time::Instant::now();
    let err = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .map(|_| ())
        .expect_err("queue is full");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a full queue must be refused immediately, not after the timeout"
    );
    assert!(is_overload_error(&err), "must be a typed overload: {err:#}");
    assert!(
        matches!(
            err.downcast_ref::<RateLimitError>(),
            Some(RateLimitError::QueueFull { .. })
        ),
        "expected QueueFull, got {err:#}"
    );
    assert_eq!(limiter.get_stats().await.requests_discarded, 1);
}

/// A refused request must not leak its queue slot, or the limiter would refuse
/// ever more requests as `QueueFull` with nothing actually queued.
#[tokio::test]
async fn refused_requests_release_their_queue_slot() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        max_concurrent: 1,
        max_queued: 4,
        queue_timeout_secs: 1,
        ..Default::default()
    });

    let held = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .expect("first permit");

    for _ in 0..3 {
        let err = limiter
            .acquire_permit(RequestSource::Network)
            .await
            .map(|_| ())
            .expect_err("times out while the permit is held");
        assert!(is_overload_error(&err));
    }

    assert_eq!(
        limiter.get_stats().await.currently_queued,
        0,
        "timed-out waiters must not stay counted as queued"
    );

    drop(held);
    limiter
        .acquire_permit(RequestSource::Network)
        .await
        .expect("limiter still works after three refusals");
}

/// User requests are unchanged: they wait, without a deadline.
#[tokio::test]
async fn user_requests_still_wait_without_a_deadline() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        max_concurrent: 1,
        queue_timeout_secs: 1,
        ..Default::default()
    });

    let first = limiter
        .acquire_permit(RequestSource::User)
        .await
        .expect("first permit");

    let waiter = {
        let limiter = limiter.clone();
        tokio::spawn(async move {
            limiter
                .acquire_permit(RequestSource::User)
                .await
                .map(|_| ())
        })
    };

    // Well past the network queue timeout — a user request ignores it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!waiter.is_finished(), "user request must keep waiting");

    drop(first);
    let result = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("user request served after the permit frees")
        .expect("task panicked");
    assert!(result.is_ok(), "user request refused: {:?}", result.err());
}

/// Token exhaustion is a budget, not congestion: it is still an immediate
/// refusal for network requests, and still a typed one.
#[tokio::test]
async fn an_exhausted_token_budget_refuses_network_requests_immediately() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        max_concurrent: 4,
        token_limit: Some(10),
        token_window_secs: 600,
        ..Default::default()
    });

    limiter.record_token_usage(100, 100).await;

    let started = std::time::Instant::now();
    let err = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .map(|_| ())
        .expect_err("budget exhausted");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "budget refusal must be immediate"
    );
    assert!(matches!(
        err.downcast_ref::<RateLimitError>(),
        Some(RateLimitError::TokenLimit { .. })
    ));
}

/// The token-usage history must not grow for the life of the process.
///
/// The default configuration is `token_limit: None`, and the only prune used to
/// live *after* the early return that case takes — so a long-running
/// `netget --mcp` accumulated one record per LLM call forever, and every
/// `get_stats()` scanned all of them.
#[tokio::test]
async fn token_history_is_pruned_even_with_no_token_limit() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        token_limit: None, // the default, and the case that used to skip pruning
        token_window_secs: 1,
        ..Default::default()
    });

    for _ in 0..5 {
        limiter.record_token_usage(10, 20).await;
    }
    assert_eq!(
        limiter.token_history_len().await,
        5,
        "records inside the window are kept"
    );

    tokio::time::sleep(Duration::from_millis(1200)).await;

    limiter.record_token_usage(10, 20).await;
    assert_eq!(
        limiter.token_history_len().await,
        1,
        "records older than the window must be dropped"
    );

    // Totals are cumulative and unaffected by pruning.
    let stats = limiter.get_stats().await;
    assert_eq!(stats.total_input_tokens, 60);
    assert_eq!(stats.total_output_tokens, 120);
    assert_eq!(stats.current_window_tokens, 30);
}
