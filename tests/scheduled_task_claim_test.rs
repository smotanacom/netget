//! Scheduler ownership, IDs and completion. Only the completion test uses a local
//! mocked Ollama HTTP server; no real model or external endpoint is contacted.

#[allow(dead_code, unused_imports)]
mod helpers;

use netget::state::task::{ScheduledTask, TaskId, TaskScope, TaskStatus};
use netget::state::AppState;
use std::sync::Arc;

fn task(name: &str, delay: u64) -> ScheduledTask {
    ScheduledTask::new_one_shot(
        TaskId::new(987654321),
        name.into(),
        TaskScope::Global,
        delay,
        "do nothing".into(),
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn allocated_task_id_controls_lookup_status_and_removal() {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let allocated = state.add_task(task("identity", 0)).await;
    let stored = state.get_task("identity").await.expect("task by name");
    assert_eq!(stored.id, allocated, "stored task kept its placeholder ID");
    assert_eq!(
        state
            .get_task(&allocated.to_string())
            .await
            .expect("task by allocated ID")
            .id,
        allocated
    );
    state
        .update_task_status(stored.id, TaskStatus::Completed)
        .await;
    assert_eq!(
        state
            .get_task("identity")
            .await
            .expect("updated task")
            .status,
        TaskStatus::Completed
    );
    state
        .remove_task(stored.id)
        .await
        .expect("remove through stored ID");
    assert!(state.get_task("identity").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_scheduler_ticks_claim_due_tasks_once() {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let due = state.add_task(task("due", 0)).await;
    state.add_task(task("later", 3600)).await;
    let done = state.add_task(task("done", 0)).await;
    state.update_task_status(done, TaskStatus::Completed).await;
    let gate = Arc::new(tokio::sync::Barrier::new(16));
    let mut ticks = Vec::new();
    for _ in 0..16 {
        let state = state.clone();
        let gate = gate.clone();
        ticks.push(tokio::spawn(async move {
            gate.wait().await;
            state.claim_due_tasks().await
        }));
    }
    let mut claimed = Vec::new();
    for tick in ticks {
        claimed.extend(tick.await.expect("scheduler tick"));
    }
    assert_eq!(
        claimed.len(),
        1,
        "one due task must be owned by exactly one tick"
    );
    assert_eq!(claimed[0].id, due);
    assert_eq!(claimed[0].status, TaskStatus::Executing);
    assert!(
        state.claim_due_tasks().await.is_empty(),
        "executing task was claimed again"
    );
    assert_eq!(
        state.get_task("later").await.expect("future task").status,
        TaskStatus::Scheduled
    );
}

#[tokio::test]
async fn recurring_task_stops_after_its_first_allowed_execution() -> helpers::E2EResult<()> {
    use helpers::mock_builder::MockLlmBuilder;
    use helpers::mock_ollama::MockOllamaServer;
    use netget::state::app_state::ScriptingMode;
    use std::time::Duration;

    let config = MockLlmBuilder::new()
        .on_any()
        .respond_with_actions(serde_json::json!([{"type":"show_message","message":"one task run"}]))
        .expect_calls(1)
        .and()
        .build();
    let mock = MockOllamaServer::start(config).await?;
    let state = AppState::new_with_options(false, mock.base_url());
    state.set_ollama_model(Some("mock-model".into())).await;
    state.set_selected_scripting_mode(ScriptingMode::Off).await;
    let llm = netget::llm::OllamaClient::new(mock.base_url());
    let mut scheduled = ScheduledTask::new_recurring(
        TaskId::new(0),
        "run-once".into(),
        TaskScope::Global,
        3600,
        Some(1),
        "Show one task run".into(),
        None,
    )
    .unwrap();
    scheduled.next_execution = netget::utils::clock::Instant::now();
    let id = state.add_task(scheduled).await;
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel();
    netget::cli::execute_due_tasks_public(&state, &llm, &status_tx).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = status_rx.recv().await {
            if message.contains("reached max executions (1) and removed") {
                return;
            }
        }
        panic!("scheduler status channel closed before completion");
    })
    .await
    .expect("first execution did not remove the recurring task at its limit");
    assert!(state.get_task(&id.to_string()).await.is_none());
    netget::cli::execute_due_tasks_public(&state, &llm, &status_tx).await;
    mock.wait_for_expectations(10).await;
    mock.verify_calls().await?;
    Ok(())
}
