//! Server teardown must cancel a peer command already blocked in its writer.

use netget::server::peer_support::spawn_peer_command_task;
use netget::server::tcp::actions::TcpProtocol;
use netget::state::client_handles::ClientCommand;
use netget::state::{AppState, ServerId, ServerInstance};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot, Mutex};

struct BlockedWriter {
    entered: Option<oneshot::Sender<()>>,
    dropped: Option<oneshot::Sender<()>>,
}

impl AsyncWrite for BlockedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Drop for BlockedWriter {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}

#[tokio::test]
async fn server_removal_cancels_a_blocked_peer_write() {
    cancellation_check(true).await;
}

#[tokio::test]
async fn individual_peer_close_cancels_a_blocked_write() {
    cancellation_check(false).await;
}

async fn cancellation_check(remove_server: bool) {
    let state = Arc::new(AppState::new_with_options(
        false,
        "http://127.0.0.1:1".into(),
    ));
    let id = state
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "tcp".into(),
            String::new(),
        ))
        .await;
    let (commands, receiver) = mpsc::channel(1);
    state
        .register_peer_handle(
            id,
            1,
            netget::state::client_handles::ClientHandle {
                command_tx: commands.clone(),
            },
        )
        .await;
    let (status_tx, _status_rx) = mpsc::unbounded_channel();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    spawn_peer_command_task(
        receiver,
        Arc::new(TcpProtocol::new()),
        state.clone(),
        id,
        1,
        Arc::new(Mutex::new(BlockedWriter {
            entered: Some(entered_tx),
            dropped: Some(dropped_tx),
        })),
        status_tx,
    );
    commands
        .send(ClientCommand {
            action: serde_json::json!({"type": "send_tcp_data", "data": "pending"}),
            reply_tx,
        })
        .await
        .expect("queue command");
    tokio::time::timeout(Duration::from_secs(2), entered_rx)
        .await
        .expect("writer was never polled")
        .expect("writer notification");
    if remove_server {
        state.remove_server(id).await.expect("remove server");
    } else {
        state.remove_peer_handle(id, 1).await;
        assert!(
            state.get_server(id).await.is_some(),
            "closing a peer must preserve its listener"
        );
    }
    // Retaining the command sender makes this assert task cancellation, independently
    // of whether teardown dropped the state's handle or a receiver observed channel EOF.
    tokio::time::timeout(Duration::from_secs(2), dropped_rx)
        .await
        .expect("blocked peer writer outlived server teardown")
        .expect("writer drop notification");
    assert!(
        reply_rx.await.is_err(),
        "aborted command retained its reply sender"
    );
    drop(commands);
}
