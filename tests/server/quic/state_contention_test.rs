//! Accepting a stream must not suspend the futures which already wait for AppState.
use crate::helpers::{
    mock_builder::MockLlmBuilder, mock_ollama::MockOllamaServer, quic_peer::Certificate,
};
use netget::{
    cli::management::ServerForm,
    llm::{actions::protocol_trait::Protocol, OllamaClient},
    protocol::StartupParams,
    state::AppState,
};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tracing_subscriber::{layer::SubscriberExt, Layer};

// The trace rendezvous creates contention at the actual model-selection boundary,
// without changing shared state APIs or relying on sleeps to queue the first writer.
struct ContentionLayer {
    selected: AtomicBool,
    accepted: AtomicUsize,
    acquire: mpsc::Sender<()>,
    held: Mutex<mpsc::Receiver<()>>,
    second_accepted: Arc<Notify>,
}

#[derive(Default)]
struct Message(String);
impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for ContentionLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut message = Message::default();
        event.record(&mut message);
        if message.0.starts_with("Model already selected:")
            && !self.selected.swap(true, Ordering::SeqCst)
        {
            self.acquire.send(()).unwrap();
            self.held
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("the independent state holder must acquire the lock");
        }
        if message.0 == "QUIC stream accepted" && self.accepted.fetch_add(1, Ordering::SeqCst) == 1
        {
            self.second_accepted.notify_one();
        }
    }
}

struct LockHolder {
    release: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for LockHolder {
    fn drop(&mut self) {
        // Release the lock on assertion failure too, so no background holder leaks.
        let _ = self.release.send(());
        let joined = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            joined.unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn accepting_a_second_stream_keeps_the_older_state_waiter_polled() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("quic_stream_opened")
            .respond_with_actions(json!([]))
            .expect_calls(2)
            .and()
            .build(),
    )
    .await
    .unwrap();
    let state = AppState::new_with_options(false, mock.base_url());
    state.set_ollama_model(Some("mock-model".into())).await;
    state
        .set_llm_client(OllamaClient::new(mock.base_url()))
        .await;
    let cert = Certificate::new();
    let (status, _) = tokio::sync::mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "quic".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        startup_params: Some(json!({"cert_path":cert.cert(),"key_path":cert.key()})),
        event_handlers: Some(vec![
            json!({"event_pattern":"quic_connection_opened","handler":{"type":"static","actions":[]}}),
            json!({"event_pattern":"quic_data_received","handler":{"type":"static","actions":[{"type":"send_quic_data","data":"progress"}]}}),
        ]),
        ..Default::default()
    }
    .create(&state, status)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(addr) = state.get_server(id).await.unwrap().local_addr {
                break addr;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let (acquire, acquire_rx) = mpsc::channel();
    let (held_tx, held) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let holder_ready = Arc::new(Notify::new());
    let second_accepted = Arc::new(Notify::new());
    let holder_state = state.clone();
    let ready = holder_ready.clone();
    let holder = LockHolder {
        release,
        thread: Some(std::thread::spawn(move || {
            acquire_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(holder_state.with_server_mut(id, |_| {
                    held_tx.send(()).unwrap();
                    ready.notify_one();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
                .unwrap();
        })),
    };
    let subscriber = tracing_subscriber::registry().with(ContentionLayer {
        selected: AtomicBool::new(false),
        accepted: AtomicUsize::new(0),
        acquire,
        held: Mutex::new(held),
        second_accepted: second_accepted.clone(),
    });
    let _tracing = tracing::subscriber::set_default(subscriber);
    let params = StartupParams::new(
        cert.trust(),
        netget::client::quic::actions::QuicClientProtocol.get_startup_parameters(),
    )
    .unwrap();
    let (_endpoint, connection) =
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"netget-quic", 0)
            .await
            .unwrap();
    let (mut first_send, mut first_recv) = connection.0.open_bi().await.unwrap();
    first_send.write_all(b"first").await.unwrap();
    first_send.finish().unwrap();
    tokio::time::timeout(Duration::from_secs(5), holder_ready.notified())
        .await
        .expect("the first stream must reach the controlled state wait");
    // This runtime has one thread: the test resumes only after that stream has
    // returned Pending on its feedback write while the other thread holds state.
    let (mut second_send, mut second_recv) = connection.0.open_bi().await.unwrap();
    second_send.write_all(b"second").await.unwrap();
    second_send.finish().unwrap();
    tokio::time::timeout(Duration::from_secs(5), second_accepted.notified())
        .await
        .expect("the server must accept the second stream before releasing state");
    holder.release.send(()).unwrap();

    // The original accept branch queues its ID write behind the first stream's
    // write, then stops polling that older future even after the holder releases.
    // Keep the existing multiplexing test's five-second response deadline.
    let responses = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(first_recv.read_to_end(1024), second_recv.read_to_end(1024))
    })
    .await
    .expect("both stream futures must progress after state is released")
    .unwrap();
    assert_eq!(responses.0, b"progress");
    assert_eq!(responses.1, b"progress");
    mock.verify_calls().await.unwrap();
    drop(holder);
    state.remove_server(id).await;
}
