//! CPU-only regressions for shared-core follow-up findings.
use netget::llm::{BreakerState, CircuitBreaker, RateLimiter, RateLimiterConfig, RequestSource};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn half_open_has_one_probe_and_cancellation_releases_the_lease() {
    let breaker = Arc::new(CircuitBreaker::new(1, Duration::ZERO));
    breaker.record_failure("connection refused");
    let probe = breaker.acquire().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let contenders: Vec<_> = (0..16)
        .map(|_| {
            let breaker = breaker.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                assert!(breaker.acquire().is_err());
            })
        })
        .collect();
    barrier.wait();
    for handle in contenders {
        handle.join().unwrap();
    }
    assert_eq!(breaker.status().state, BreakerState::HalfOpen);
    drop(probe);
    breaker.acquire().unwrap().record_success();
    assert_eq!(breaker.status().state, BreakerState::Closed);
}

#[test]
fn outcomes_from_before_a_trip_or_reset_do_not_override_new_state() {
    let breaker = CircuitBreaker::new(1, Duration::ZERO);
    let stale_success = breaker.acquire().unwrap();
    breaker
        .acquire()
        .unwrap()
        .record_failure("connection refused");
    stale_success.record_success();
    assert_eq!(breaker.status().state, BreakerState::HalfOpen);
    let stale_failure = breaker.acquire().unwrap();
    breaker.reset();
    stale_failure.record_failure("connection refused");
    assert_eq!(breaker.status().state, BreakerState::Closed);
    assert_eq!(breaker.status().trips, 1);
}

async fn wait_queued(limiter: &RateLimiter, count: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while limiter.get_stats().await.currently_queued != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn resized_concurrency_counts_old_permits_and_preserves_queued_requests() {
    let limiter = RateLimiter::new(RateLimiterConfig {
        max_concurrent: 2,
        ..Default::default()
    });
    let a = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .unwrap();
    let b = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .unwrap();
    let queued = limiter.clone();
    let task =
        tokio::spawn(async move { queued.acquire_permit(RequestSource::Network).await.unwrap() });
    wait_queued(&limiter, 1).await;
    limiter
        .update_config(RateLimiterConfig {
            max_concurrent: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    drop(a);
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "one old request still occupies the new limit"
    );
    drop(b);
    let permit = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    let waiting = limiter.clone();
    let task = tokio::spawn(async move {
        waiting
            .acquire_permit(RequestSource::Network)
            .await
            .unwrap()
    });
    wait_queued(&limiter, 1).await;
    limiter
        .update_config(RateLimiterConfig {
            max_concurrent: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    drop((permit, second));
}

#[tokio::test]
async fn cancelled_user_wait_does_not_leak_statistics_or_queue_position() {
    let limiter = RateLimiter::new(RateLimiterConfig::default());
    let permit = limiter
        .acquire_permit(RequestSource::Network)
        .await
        .unwrap();
    let waiting = limiter.clone();
    let task =
        tokio::spawn(async move { waiting.acquire_permit(RequestSource::User).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), async {
        while limiter.get_stats().await.requests_waiting == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(limiter.get_stats().await.requests_waiting, 0);
    drop(permit);
    tokio::time::timeout(
        Duration::from_secs(2),
        limiter.acquire_permit(RequestSource::Network),
    )
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn conversation_metadata_is_bounded_and_deep_values_are_rejected_without_recursive_drop() {
    use netget::llm::conversation_state::{ConversationState, MessageType};
    use serde_json::json;
    let mut state = ConversationState::new(8000);
    state.add_llm_response("ok".into(), Some(json!({"huge": "x".repeat(100_000)})));
    assert!(matches!(
        &state.messages[0].message_type,
        MessageType::LLMResponse {
            action_json: None,
            ..
        }
    ));
    let mut deep = serde_json::Value::Null;
    for _ in 0..2000 {
        deep = serde_json::Value::Array(vec![deep]);
    }
    state.add_llm_response("deep".into(), Some(deep));
    state.add_tool_call("x".repeat(100_000), "description".into());
    match &state.messages.back().unwrap().message_type {
        MessageType::ToolCall { tool_name, .. } => assert!(tool_name.len() <= 128),
        _ => panic!("tool metadata expected"),
    }
    state.mark_server_protocols_documented(&["tcp".into(), "<fake>".into(), "x".repeat(200)]);
    assert_eq!(state.get_documented_server_protocols().len(), 1);
    for n in 0..2000 {
        state.mark_client_protocols_documented(&[format!("p{n}")]);
    }
    assert_eq!(
        state.get_documented_client_protocols().len(),
        ConversationState::MAX_DOCUMENTED_PROTOCOLS
    );
}

#[test]
fn settings_migrate_with_an_exact_backup_and_do_not_conflict_with_config() {
    use netget::settings::Settings;
    use netget::utils::file_io::{ensure_config_directory, write_atomic};
    let dir = tempfile::tempdir().unwrap();
    let original = br#"{"model":"saved-model","web_search_enabled":false}"#;
    std::fs::write(dir.path().join(".netget"), original).unwrap();
    let settings = Settings::load_at(dir.path()).unwrap();
    assert_eq!(settings.model.as_deref(), Some("saved-model"));
    assert_eq!(settings.web_search_mode, "off");
    assert!(
        dir.path().join(".netget").is_file(),
        "reading must not mutate configuration"
    );
    settings.save_at(dir.path()).unwrap();
    assert!(dir.path().join(".netget").is_dir());
    assert_eq!(
        std::fs::read(dir.path().join(".netget-legacy.json")).unwrap(),
        original
    );
    let config_dir = ensure_config_directory(dir.path()).unwrap();
    write_atomic(
        &config_dir.join("config.toml"),
        b"last_backend = 'ollama'\n",
    )
    .unwrap();
    assert_eq!(Settings::load_at(dir.path()).unwrap().model, settings.model);
    assert_eq!(
        std::fs::read(config_dir.join("config.toml")).unwrap(),
        b"last_backend = 'ollama'\n"
    );
}

#[test]
fn migration_rejects_conflicting_backups_and_recovers_interrupted_installation() {
    use netget::settings::Settings;
    use netget::utils::file_io::ensure_config_directory;
    let dir = tempfile::tempdir().unwrap();
    let old = br#"{"model":"old"}"#;
    let newer = br#"{"model":"newer"}"#;
    std::fs::write(dir.path().join(".netget"), newer).unwrap();
    std::fs::write(dir.path().join(".netget-legacy.json"), old).unwrap();
    assert!(ensure_config_directory(dir.path()).is_err());
    assert_eq!(std::fs::read(dir.path().join(".netget")).unwrap(), newer);
    std::fs::remove_file(dir.path().join(".netget")).unwrap();
    assert_eq!(
        Settings::load_at(dir.path()).unwrap().model.as_deref(),
        Some("old")
    );
    ensure_config_directory(dir.path()).unwrap();
    assert_eq!(
        Settings::load_at(dir.path()).unwrap().model.as_deref(),
        Some("old")
    );
}

#[test]
fn concurrent_migration_preserves_settings_once() {
    use netget::utils::file_io::ensure_config_directory;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".netget"), br#"{"model":"shared"}"#).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = dir.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ensure_config_directory(&path).unwrap();
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        netget::settings::Settings::load_at(dir.path())
            .unwrap()
            .model
            .as_deref(),
        Some("shared")
    );
}

#[test]
fn bounded_reads_and_failed_atomic_replacement_keep_existing_data() {
    use netget::utils::file_io::{read_regular_file, write_atomic};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    write_atomic(&path, b"original").unwrap();
    assert!(read_regular_file(&path, 7).is_err());
    assert_eq!(read_regular_file(&path, 8).unwrap(), b"original");
    let target = dir.path().join("directory");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep"), b"safe").unwrap();
    assert!(write_atomic(&target, b"new").is_err());
    assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"safe");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        2,
        "staging file cleaned on failure"
    );
}
