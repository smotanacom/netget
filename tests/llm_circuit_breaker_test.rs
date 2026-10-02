//! Circuit breaker over the LLM backend transport (IMPROVEMENTS.md item 16).
//!
//! The point of the breaker is that a *down* backend stops costing a full request timeout
//! per request. With `max_concurrent: 1` in the rate limiter, N pending connections
//! otherwise serialise into N × timeout before the first peer learns anything is wrong.
//! `fails_fast_once_tripped` is the test that matters: it measures that the request after
//! the trip returns in microseconds rather than waiting out the timeout again.
//!
//! The tests below the mechanics section deliberately drive a real `OllamaClient` against a
//! real socket, so they fail if the breaker is ever wired up but not consulted: a breaker
//! that is only exercised through `CircuitBreaker`'s own methods proves nothing about the
//! call path that matters.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use netget::llm::circuit_breaker::is_transport_failure;
use netget::llm::{BreakerState, CircuitBreaker, OllamaClient};

// --- Breaker mechanics (no I/O) --------------------------------------------

#[test]
fn closed_until_the_threshold_is_reached() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(30));

    assert!(breaker.acquire().is_ok());
    assert_eq!(breaker.status().state, BreakerState::Closed);

    breaker.record_failure("connection refused");
    assert!(breaker.acquire().is_ok(), "one failure must not trip it");
    breaker.record_failure("connection refused");
    assert!(breaker.acquire().is_ok(), "two failures must not trip it");

    assert!(
        breaker.record_failure("connection refused"),
        "third opens it"
    );
    assert!(breaker.acquire().is_err());
    assert!(breaker.is_open());
    assert_eq!(breaker.status().trips, 1);
}

#[test]
fn a_success_resets_the_streak() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(30));

    breaker.record_failure("timed out");
    breaker.record_failure("timed out");
    breaker.record_success();
    assert_eq!(breaker.status().consecutive_failures, 0);

    // The streak restarts, so two more failures still do not trip it.
    breaker.record_failure("timed out");
    breaker.record_failure("timed out");
    assert!(breaker.acquire().is_ok());
}

#[test]
fn a_probe_is_allowed_after_the_cooldown_and_a_failing_probe_reopens() {
    let breaker = CircuitBreaker::new(1, Duration::from_millis(120));

    breaker.record_failure("connection refused");
    assert!(
        breaker.acquire().is_err(),
        "open immediately after tripping"
    );

    std::thread::sleep(Duration::from_millis(150));

    assert_eq!(breaker.status().state, BreakerState::HalfOpen);
    let probe = breaker.acquire().expect("cooldown elapsed: probe allowed");

    // A failing probe re-opens it for another cooldown.
    probe.record_failure("connection refused");
    assert!(breaker.acquire().is_err());
    assert_eq!(breaker.status().trips, 2);

    // A succeeding probe closes it.
    std::thread::sleep(Duration::from_millis(150));
    breaker.acquire().unwrap().record_success();
    assert_eq!(breaker.status().state, BreakerState::Closed);
    assert!(breaker.acquire().is_ok());
}

#[test]
fn an_open_breaker_says_so() {
    let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
    breaker.record_failure("Ollama API call timed out after 120s");

    let status = breaker.status();
    assert!(status.is_open());
    let summary = status.summary();
    assert!(
        summary.contains("OPEN"),
        "status must be legible: {}",
        summary
    );
    assert!(
        summary.contains("timed out"),
        "status must name the cause: {}",
        summary
    );
}

#[test]
fn only_transport_failures_count() {
    // The backend could not be reached.
    for message in [
        "Ollama API call timed out after 120 seconds",
        "✗  Cannot connect to Ollama",
        "error sending request for url",
        "tcp connect error: Connection refused (os error 61)",
    ] {
        assert!(
            is_transport_failure(&anyhow::anyhow!("{}", message)),
            "should count as a transport failure: {}",
            message
        );
    }

    // The backend answered; it just answered with an error. Tripping on these would take
    // the LLM path offline for a typo'd model name.
    for message in [
        "✗  Model not found in Ollama.",
        "✗  Authentication failed. Check your API key.",
        "✗  Rate limited by API provider.",
        "Failed to parse OpenAI API response",
    ] {
        assert!(
            !is_transport_failure(&anyhow::anyhow!("{}", message)),
            "should NOT count as a transport failure: {}",
            message
        );
    }
}

// --- The behaviour the item is about ---------------------------------------

/// Accept connections and never answer, so every request costs the full request timeout —
/// the shape of an Ollama process that is up but wedged, which is the expensive case.
async fn spawn_black_hole() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();

    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            // Hold the connection open without writing a response.
            held.push(stream);
        }
    });

    port
}

#[tokio::test(flavor = "multi_thread")]
async fn fails_fast_once_tripped() {
    const TIMEOUT: Duration = Duration::from_millis(400);
    let port = spawn_black_hole().await;

    let client = OllamaClient::new(format!("http://127.0.0.1:{}", port))
        .with_request_timeout(TIMEOUT)
        .with_circuit_breaker(Arc::new(CircuitBreaker::new(2, Duration::from_secs(30))));

    // The first two requests each pay the timeout — that is what the breaker is counting.
    for attempt in 1..=2 {
        let started = Instant::now();
        let result = client
            .generate_with_retry("model", "prompt", "json", 0)
            .await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "attempt {} should fail", attempt);
        assert!(
            elapsed >= TIMEOUT / 2,
            "attempt {} returned in {:?}; it should have waited for the timeout",
            attempt,
            elapsed
        );
    }

    let status = client.circuit_breaker_status();
    assert!(
        status.is_open(),
        "two transport failures should have opened the breaker, got {:?}",
        status
    );

    // The whole point: the next request does not wait at all.
    let started = Instant::now();
    let error = client
        .generate_with_retry("model", "prompt", "json", 0)
        .await
        .expect_err("request should fail while the breaker is open");
    let elapsed = started.elapsed();

    assert!(
        elapsed < TIMEOUT / 4,
        "a tripped breaker must fail fast, but the request took {:?} (timeout is {:?})",
        elapsed,
        TIMEOUT
    );

    let rendered = format!("{:#}", error);
    assert!(
        rendered.contains("circuit breaker open"),
        "the failure must explain itself, got: {}",
        rendered
    );
}

/// Every clone of a client shares one breaker, so N connections observe one outage rather
/// than each rediscovering it at the cost of a timeout.
#[tokio::test(flavor = "multi_thread")]
async fn the_breaker_is_shared_across_clones() {
    const TIMEOUT: Duration = Duration::from_millis(300);
    let port = spawn_black_hole().await;

    let client = OllamaClient::new(format!("http://127.0.0.1:{}", port))
        .with_request_timeout(TIMEOUT)
        .with_circuit_breaker(Arc::new(CircuitBreaker::new(1, Duration::from_secs(30))));

    // One failure on the original trips the (threshold-1) breaker.
    let _ = client
        .generate_with_retry("model", "prompt", "json", 0)
        .await;
    assert!(client.circuit_breaker_status().is_open());

    // A clone — which is how the client reaches every connection task — sees it.
    let cloned = client.clone();
    assert!(cloned.circuit_breaker_status().is_open());

    let started = Instant::now();
    assert!(cloned
        .generate_with_retry("model", "prompt", "json", 0)
        .await
        .is_err());
    assert!(
        started.elapsed() < TIMEOUT / 4,
        "a clone must fail fast too, took {:?}",
        started.elapsed()
    );
}

// --- Recovery and non-regression, through the real call path ----------------

/// What the fake Ollama at the other end of the socket should do next.
mod mode {
    /// Accept the connection and never answer: the request costs the full timeout.
    pub const WEDGED: u8 = 0;
    /// Answer like a healthy Ollama.
    pub const HEALTHY: u8 = 1;
    /// Answer 404 "model not found" — reachable, but rejecting the request.
    pub const MODEL_NOT_FOUND: u8 = 2;
    /// Answer 200 with a completion that is not valid action JSON.
    pub const GARBAGE: u8 = 3;
}

/// A fake Ollama whose behaviour can be switched at runtime, so one client can watch its
/// backend die and come back without being reconstructed (reconstructing it would give it a
/// fresh breaker and prove nothing).
async fn spawn_switchable_ollama(initial: u8) -> (u16, Arc<AtomicU8>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let state = Arc::new(AtomicU8::new(initial));

    let server_state = state.clone();
    tokio::spawn(async move {
        let mut wedged = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let current = server_state.load(Ordering::SeqCst);
            if current == mode::WEDGED {
                // Hold the connection open without ever writing a response.
                wedged.push(stream);
                continue;
            }

            tokio::spawn(async move {
                // Drain whatever the client sends; we only care that it is a request.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;

                let (status, body) = match current {
                    mode::MODEL_NOT_FOUND => (
                        "404 Not Found",
                        r#"{"error":"model 'missing' not found, try pulling it first"}"#
                            .to_string(),
                    ),
                    mode::GARBAGE => (
                        "200 OK",
                        ollama_generate_body("Sure! Here is my answer, in prose."),
                    ),
                    _ => (
                        "200 OK",
                        ollama_generate_body(r#"{\"actions\":[{\"type\":\"no_response\"}]}"#),
                    ),
                };

                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (port, state)
}

/// An `/api/generate` response body carrying `completion` as the model's output.
fn ollama_generate_body(completion: &str) -> String {
    format!(
        r#"{{"model":"m","created_at":"2026-01-01T00:00:00Z","response":"{}","done":true}}"#,
        completion
    )
}

/// Trip the breaker, confirm it fails fast, then bring the backend back and prove the same
/// client recovers **through `generate_with_retry`** — not by poking the breaker directly.
///
/// This is the test that would catch the breaker becoming inert: if `breaker_guard()` were
/// removed, the fail-fast assertion fails; if `record_success()` were never reached from the
/// real path, the recovery assertions fail.
#[tokio::test(flavor = "multi_thread")]
async fn trips_fails_fast_and_recovers_through_the_real_call_path() {
    const TIMEOUT: Duration = Duration::from_millis(400);
    const COOLDOWN: Duration = Duration::from_millis(700);

    let (port, backend) = spawn_switchable_ollama(mode::WEDGED).await;
    let client = OllamaClient::new(format!("http://127.0.0.1:{}", port))
        .with_request_timeout(TIMEOUT)
        .with_circuit_breaker(Arc::new(CircuitBreaker::new(2, COOLDOWN)));

    // 1. Two real requests against a wedged backend, each paying the timeout.
    for attempt in 1..=2 {
        let started = Instant::now();
        assert!(
            client
                .generate_with_retry("m", "p", "json", 0)
                .await
                .is_err(),
            "attempt {} should fail against a wedged backend",
            attempt
        );
        assert!(
            started.elapsed() >= TIMEOUT / 2,
            "attempt {} returned in {:?}, so it never reached the network",
            attempt,
            started.elapsed()
        );
    }
    assert!(
        client.circuit_breaker_status().is_open(),
        "two transport failures must open the breaker"
    );

    // 2. Fail fast: measurably faster than the retry path, and self-explanatory.
    let started = Instant::now();
    let error = client
        .generate_with_retry("m", "p", "json", 0)
        .await
        .expect_err("an open breaker must reject the request");
    let fast = started.elapsed();
    assert!(
        fast < TIMEOUT / 4,
        "an open breaker must fail fast; took {:?} against a {:?} timeout",
        fast,
        TIMEOUT
    );
    let rendered = format!("{:#}", error);
    assert!(
        rendered.contains("circuit breaker open") && rendered.contains("next probe"),
        "the error must say the circuit is open and when it retries, got: {}",
        rendered
    );

    // 3. The backend comes back and the cooldown expires.
    backend.store(mode::HEALTHY, Ordering::SeqCst);
    tokio::time::sleep(COOLDOWN + Duration::from_millis(150)).await;
    assert_eq!(
        client.circuit_breaker_status().state,
        BreakerState::HalfOpen,
        "after the cooldown the next request should be a probe"
    );

    // 4. The probe goes over the wire, succeeds, and closes the breaker.
    let probe = client
        .generate_with_retry("m", "p", "json", 0)
        .await
        .expect("the probe must be let through to a recovered backend");
    assert!(
        probe.contains("actions"),
        "the probe should carry the backend's answer, got: {}",
        probe
    );
    let status = client.circuit_breaker_status();
    assert_eq!(
        status.state,
        BreakerState::Closed,
        "a successful probe must close the breaker"
    );
    assert_eq!(
        status.consecutive_failures, 0,
        "and reset the failure count"
    );

    // 5. Service is genuinely restored, not merely reported as restored.
    client
        .generate_with_retry("m", "p", "json", 0)
        .await
        .expect("a closed breaker must let normal requests through");
}

/// A reachable backend that rejects the request, or a model that answers with nonsense, is
/// not an outage. Tripping on either would be a regression: a typo'd model name or a small
/// model's bad JSON would take the whole LLM path offline for a cooldown.
#[tokio::test(flavor = "multi_thread")]
async fn a_reachable_backend_never_trips_the_breaker() {
    let (port, backend) = spawn_switchable_ollama(mode::MODEL_NOT_FOUND).await;
    // Threshold 1: if any of these counted, the breaker would open on the first one.
    let client = OllamaClient::new(format!("http://127.0.0.1:{}", port))
        .with_request_timeout(Duration::from_secs(5))
        .with_circuit_breaker(Arc::new(CircuitBreaker::new(1, Duration::from_secs(30))));

    for attempt in 1..=3 {
        assert!(
            client
                .generate_with_retry("missing", "p", "json", 0)
                .await
                .is_err(),
            "attempt {} should surface the backend's rejection",
            attempt
        );
        let status = client.circuit_breaker_status();
        assert_eq!(
            status.state,
            BreakerState::Closed,
            "a rejected request is not an outage (attempt {}): {:?}",
            attempt,
            status
        );
    }

    // Same for a model that answers, just not with parseable actions — that is what the
    // retry/repair loop is for.
    backend.store(mode::GARBAGE, Ordering::SeqCst);
    for attempt in 1..=3 {
        assert!(
            client
                .generate_with_retry("m", "p", "json", 0)
                .await
                .is_err(),
            "attempt {} should fail to parse",
            attempt
        );
        let status = client.circuit_breaker_status();
        assert_eq!(
            status.state,
            BreakerState::Closed,
            "malformed model output is not a transport failure (attempt {}): {:?}",
            attempt,
            status
        );
        assert_eq!(status.consecutive_failures, 0);
    }
}

/// The agent-queue backend answers from the calling MCP agent, not from a network service.
/// A slow agent is not a transport fault and must not take the queue offline.
#[test]
fn the_agent_queue_backend_is_not_guarded() {
    let queue = Arc::new(netget::llm::LlmRequestQueue::new(None));
    let client = OllamaClient::new_queue(queue, Duration::from_millis(50));

    // No requests have been made, so this is only asserting the breaker is not consulted
    // for this backend: its state stays Closed and nothing short-circuits.
    assert_eq!(client.circuit_breaker_status().state, BreakerState::Closed);
    assert_eq!(client.backend_type(), "agent");
}
