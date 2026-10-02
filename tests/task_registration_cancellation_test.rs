//! Dropping registration must cancel an already-started child, including before
//! its future is first polled. No protocol, private lock hook or network required.
use netget::state::{
    app_state::AppState, client::ClientInstance, server::ServerInstance, ClientId, ServerId,
};
use std::time::Duration;
use tokio::{sync::oneshot, task::JoinHandle};

struct NotifyDrop(Option<oneshot::Sender<()>>);
impl Drop for NotifyDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
async fn started_child() -> (JoinHandle<()>, oneshot::Receiver<()>) {
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let _notify = NotifyDrop(Some(dropped_tx));
        started_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started_rx.await.unwrap();
    (handle, dropped_rx)
}

#[tokio::test]
async fn dropping_unpolled_server_registration_aborts_started_child() {
    let state = AppState::new();
    let (handle, dropped) = started_child().await;
    let abort = handle.abort_handle();
    let registration = state.register_server_task(ServerId::new(123), handle);
    drop(registration);
    let result = tokio::time::timeout(Duration::from_millis(500), dropped).await;
    abort.abort(); // also clean up the demonstrably leaked pre-fix child
    assert!(
        matches!(result, Ok(Ok(()))),
        "dropping unpolled registration detached the server child"
    );
}

#[tokio::test]
async fn dropping_unpolled_client_registration_aborts_started_child() {
    let state = AppState::new();
    let (handle, dropped) = started_child().await;
    let abort = handle.abort_handle();
    let registration = state.register_client_task(ClientId::new(123), handle);
    drop(registration);
    let result = tokio::time::timeout(Duration::from_millis(500), dropped).await;
    abort.abort();
    assert!(
        matches!(result, Ok(Ok(()))),
        "dropping unpolled registration detached the client child"
    );
}

#[tokio::test]
async fn successful_server_registration_retains_child_until_owner_removal() {
    let state = AppState::new();
    let id = state
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "test".into(),
            String::new(),
        ))
        .await;
    let (handle, mut dropped) = started_child().await;
    state.register_server_task(id, handle).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        dropped.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(state.server_task_count(id).await, 1);
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(2), dropped)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn successful_client_registration_retains_child_until_owner_removal() {
    let state = AppState::new();
    let id = state
        .add_client(ClientInstance::new(
            ClientId::new(0),
            "127.0.0.1:1".into(),
            "test".into(),
            String::new(),
        ))
        .await;
    let (handle, mut dropped) = started_child().await;
    state.register_client_task(id, handle).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        dropped.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(state.client_task_count(id).await, 1);
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(2), dropped)
        .await
        .unwrap()
        .unwrap();
}
