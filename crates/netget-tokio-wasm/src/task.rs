//! Tasks on the JS event loop.
//!
//! `spawn` hands the future to `wasm_bindgen_futures::spawn_local`, which polls it from the
//! browser's microtask queue. There is one thread, so the `Send` bound tokio puts on spawned
//! futures is dropped here; the futures NetGet spawns are `Send` anyway, since they compile
//! for native tokio.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::{AbortHandle as RawAbortHandle, Abortable};
use tokio::sync::oneshot;

/// A handle to a spawned task. Awaiting it yields the task's output; `abort` cancels it.
pub struct JoinHandle<T> {
    rx: oneshot::Receiver<T>,
    abort: RawAbortHandle,
    done: Arc<AtomicBool>,
}

impl<T> JoinHandle<T> {
    /// Cancel the task. The next time it would be polled it is dropped instead, and awaiting
    /// the handle yields a cancelled [`JoinError`].
    pub fn abort(&self) {
        self.abort.abort();
    }

    /// True once the task has completed or been aborted.
    pub fn is_finished(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// A clonable handle that can abort the task without owning the join handle.
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            inner: self.abort.clone(),
            done: self.done.clone(),
        }
    }
}

impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
            // The sender is dropped without sending exactly when the task was aborted (a
            // panic aborts the whole wasm instance, so it never gets here).
            Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError { cancelled: true })),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

/// Aborts a task without owning its [`JoinHandle`].
#[derive(Clone, Debug)]
pub struct AbortHandle {
    inner: RawAbortHandle,
    done: Arc<AtomicBool>,
}

impl AbortHandle {
    pub fn abort(&self) {
        self.inner.abort();
    }

    pub fn is_finished(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// Why a task did not produce its output. On wasm the only cause is cancellation.
#[derive(Debug)]
pub struct JoinError {
    cancelled: bool,
}

impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn is_panic(&self) -> bool {
        false
    }

    pub fn into_panic(self) -> Box<dyn Any + Send + 'static> {
        panic!("JoinError::into_panic on a cancelled task")
    }

    pub fn try_into_panic(self) -> Result<Box<dyn Any + Send + 'static>, JoinError> {
        Err(self)
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task was cancelled")
    }
}

impl std::error::Error for JoinError {}

impl From<JoinError> for std::io::Error {
    fn from(e: JoinError) -> Self {
        std::io::Error::new(std::io::ErrorKind::Other, e)
    }
}

/// Run `future` on the JS event loop.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (tx, rx) = oneshot::channel();
    let (abort, registration) = RawAbortHandle::new_pair();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = Abortable::new(future, registration).await;
        finished.store(true, Ordering::SeqCst);
        if let Ok(value) = outcome {
            let _ = tx.send(value);
        }
    });
    JoinHandle { rx, abort, done }
}

/// Same as [`spawn`]; there is only one thread.
pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    spawn(future)
}

/// Run a closure. There is no thread pool in the browser, so it runs inline on the event
/// loop, inside a task; anything that blocks for long blocks the page for that long.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + 'static,
    R: 'static,
{
    spawn(async move { f() })
}

/// Yield once to let other tasks run.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
