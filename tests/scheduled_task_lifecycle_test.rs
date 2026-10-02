//! Pure state/future regressions: no sockets, scripts, or model requests.
use netget::llm::actions::common::ServerTaskDefinition;
use netget::server::connection::ConnectionId;
use netget::state::task::{ScheduledTask, TaskId, TaskScope, TaskStatus};
use netget::state::{AppState, ClientId, ClientInstance, ServerId, ServerInstance};
use std::time::Duration;
use tokio::sync::oneshot;

fn definition() -> ServerTaskDefinition {
    ServerTaskDefinition {
        task_id: "validation".into(),
        recurring: true,
        delay_secs: Some(45),
        interval_secs: Some(60),
        max_executions: Some(2),
        instruction: "noop".into(),
        context: None,
    }
}

#[test]
fn invalid_delays_and_zero_recurrence_return_errors() {
    assert!(ScheduledTask::new_one_shot(
        TaskId::new(0),
        "bad".into(),
        TaskScope::Global,
        u64::MAX,
        "noop".into(),
        None
    )
    .is_err());
    for (interval, max) in [(u64::MAX, None), (0, None), (1, Some(0))] {
        assert!(ScheduledTask::new_recurring(
            TaskId::new(0),
            "bad".into(),
            TaskScope::Global,
            interval,
            max,
            "noop".into(),
            None
        )
        .is_err());
    }
    let mut def = definition();
    def.delay_secs = Some(u64::MAX);
    assert!(ScheduledTask::from_definition(&def, TaskScope::Global).is_err());
    def.delay_secs = Some(1);
    def.interval_secs = Some(u64::MAX);
    assert!(ScheduledTask::from_definition(&def, TaskScope::Global).is_err());
}

#[test]
fn nested_recurring_tasks_honor_initial_delay() {
    let task = ScheduledTask::from_definition(&definition(), TaskScope::Global).unwrap();
    let remaining = task
        .next_execution
        .saturating_duration_since(netget::utils::clock::Instant::now());
    assert!(remaining > Duration::from_secs(44) && remaining <= Duration::from_secs(45));
    assert_eq!(task.interval_secs(), Some(60));
}

struct Dropped(Option<oneshot::Sender<()>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test]
async fn removal_of_each_scope_cancels_in_flight_work() {
    for removal in ["task", "server", "connection", "client"] {
        let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
        let server = state
            .add_server(ServerInstance::new(
                ServerId::new(0),
                0,
                "test".into(),
                "noop".into(),
            ))
            .await;
        let client = state
            .add_client(ClientInstance::new(
                ClientId::new(0),
                "unused".into(),
                "test".into(),
                "noop".into(),
            ))
            .await;
        let connection = ConnectionId::new(7);
        let scope = match removal {
            "server" => TaskScope::Server(server),
            "connection" => TaskScope::Connection(server, connection),
            "client" => TaskScope::Client(client),
            _ => TaskScope::Global,
        };
        let id = state
            .add_task(
                ScheduledTask::new_one_shot(
                    TaskId::new(0),
                    "running".into(),
                    scope,
                    0,
                    "noop".into(),
                    None,
                )
                .unwrap(),
            )
            .await;
        assert_eq!(state.claim_due_tasks().await.len(), 1);
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let guard = Dropped(Some(dropped_tx));
        assert!(
            state
                .spawn_scheduled_task(id, async move {
                    let _guard = guard;
                    let _ = started_tx.send(());
                    std::future::pending::<()>().await;
                })
                .await
        );
        started_rx.await.unwrap();
        match removal {
            "server" => {
                state.remove_server(server).await;
            }
            "connection" => state.close_connection_on_server(server, connection).await,
            "client" => {
                state.remove_client(client).await;
            }
            _ => {
                state.remove_task(id).await;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .expect(removal)
            .unwrap();
        assert!(state.get_task(&id.to_string()).await.is_none(), "{removal}");
    }
}

#[tokio::test]
async fn removed_claim_cannot_start_and_finishing_execution_cannot_be_reclaimed() {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let task = ScheduledTask::new_one_shot(
        TaskId::new(0),
        "race".into(),
        TaskScope::Global,
        0,
        "noop".into(),
        None,
    )
    .unwrap();
    let id = state.add_task(task.clone()).await;
    state.claim_due_tasks().await;
    state.remove_task(id).await;
    assert!(
        !state
            .spawn_scheduled_task(id, async { panic!("removed task ran") })
            .await
    );
    let id = state.add_task(task).await;
    state.claim_due_tasks().await;
    let (finish_tx, finish_rx) = oneshot::channel::<()>();
    assert!(
        state
            .spawn_scheduled_task(id, async move {
                let _ = finish_rx.await;
            })
            .await
    );
    state.update_task_status(id, TaskStatus::Scheduled).await;
    assert!(
        state.claim_due_tasks().await.is_empty(),
        "old execution is still alive"
    );
    state.remove_task(id).await;
    drop(finish_tx);
}
