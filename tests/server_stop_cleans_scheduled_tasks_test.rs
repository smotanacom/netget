//! Regression test: stopping a server must cancel the scheduled tasks scoped to it.
//!
//! `AppState::remove_server()` used to drop the map entry and nothing else, leaving
//! the cleanup to callers. The TUI called `cleanup_server_tasks()` alongside it; the
//! MCP `stop_server` / `stop_all` tools did not. Every orphaned server- and
//! connection-scoped task then kept firing on its interval, each tick producing an
//! LLM prompt for a server that no longer existed.
//!
//! The cleanup now lives inside `remove_server` (and inside the `cleanup_old_servers`
//! reaper), so no caller can forget it. These tests assert that from the state API
//! directly — no protocol feature, no network peer and no Ollama involved.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --test server_stop_cleans_scheduled_tasks_test -- --test-threads=100

use netget::server::connection::ConnectionId;
use netget::state::app_state::AppState;
use netget::state::server::{ServerInstance, ServerStatus};
use netget::state::task::{ScheduledTask, TaskId, TaskScope};
use netget::state::ServerId;

async fn add_server(state: &AppState, port: u16) -> ServerId {
    let mut server = ServerInstance::new(
        ServerId::new(0),
        port,
        "test".to_string(),
        "Do nothing".to_string(),
    );
    server.status = ServerStatus::Running;
    state.add_server(server).await
}

async fn add_recurring(state: &AppState, name: &str, scope: TaskScope) {
    state
        .add_task(
            ScheduledTask::new_recurring(
                TaskId::new(0),
                name.to_string(),
                scope,
                60,
                None,
                "tick".to_string(),
                None,
            )
            .unwrap(),
        )
        .await;
}

async fn task_names(state: &AppState) -> Vec<String> {
    let mut names: Vec<String> = state
        .get_all_tasks()
        .await
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn remove_server_cancels_server_and_connection_scoped_tasks() {
    let state = AppState::new();
    let doomed = add_server(&state, 1111).await;
    let survivor = add_server(&state, 2222).await;
    let conn = ConnectionId::new(77);

    add_recurring(&state, "doomed-server", TaskScope::Server(doomed)).await;
    add_recurring(
        &state,
        "doomed-connection",
        TaskScope::Connection(doomed, conn),
    )
    .await;
    add_recurring(&state, "other-server", TaskScope::Server(survivor)).await;
    add_recurring(&state, "global", TaskScope::Global).await;

    assert_eq!(state.get_all_tasks().await.len(), 4);

    assert!(state.remove_server(doomed).await.is_some());

    assert_eq!(
        task_names(&state).await,
        vec!["global".to_string(), "other-server".to_string()],
        "removing a server must cancel its own tasks AND its connections' tasks, \
         and must not touch anyone else's"
    );

    // The name → id mapping has to go too, or the name can never be reused.
    assert!(state.get_task("doomed-server").await.is_none());
    assert!(state.get_task("doomed-connection").await.is_none());
}

#[tokio::test]
async fn reaping_a_stopped_server_cancels_its_tasks() {
    let state = AppState::new();
    let server_id = add_server(&state, 3333).await;
    add_recurring(&state, "reaped", TaskScope::Server(server_id)).await;

    state
        .update_server_status(server_id, ServerStatus::Stopped)
        .await;

    // max_age_secs = 0: anything already Stopped is due for reaping.
    state.cleanup_old_servers(0).await;

    assert!(state.get_server(server_id).await.is_none());
    assert!(
        state.get_all_tasks().await.is_empty(),
        "the reaper must run the same teardown as remove_server"
    );
}

#[tokio::test]
async fn reaper_leaves_running_servers_and_their_tasks_alone() {
    let state = AppState::new();
    let server_id = add_server(&state, 4444).await;
    add_recurring(&state, "kept", TaskScope::Server(server_id)).await;

    state.cleanup_old_servers(0).await;

    assert!(state.get_server(server_id).await.is_some());
    assert_eq!(task_names(&state).await, vec!["kept".to_string()]);
}
