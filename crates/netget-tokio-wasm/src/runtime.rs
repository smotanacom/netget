//! `tokio::runtime` shapes. The JS event loop is the only executor, so there is nothing to
//! build: `Builder::build` reports `Unsupported`, `Handle::current` hands back a token whose
//! `spawn` is [`crate::spawn`], and `block_on` is impossible on a thread that must return to
//! the browser to make progress.

use std::future::Future;
use std::io;

use crate::task::JoinHandle;

#[derive(Clone, Debug, Default)]
pub struct Handle {
    _private: (),
}

#[derive(Debug)]
pub struct TryCurrentError(());

impl std::fmt::Display for TryCurrentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no runtime")
    }
}

impl std::error::Error for TryCurrentError {}

pub struct EnterGuard<'a>(std::marker::PhantomData<&'a Handle>);

impl Handle {
    pub fn current() -> Handle {
        Handle::default()
    }

    pub fn try_current() -> Result<Handle, TryCurrentError> {
        Ok(Handle::default())
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        crate::task::spawn(future)
    }

    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + 'static,
        R: 'static,
    {
        crate::task::spawn_blocking(f)
    }

    pub fn block_on<F: Future>(&self, _future: F) -> F::Output {
        panic!("block_on is not possible in the browser: the page's event loop is the executor")
    }

    pub fn enter(&self) -> EnterGuard<'_> {
        EnterGuard(std::marker::PhantomData)
    }
}

#[derive(Debug, Default)]
pub struct Builder {
    _private: (),
}

impl Builder {
    pub fn new_current_thread() -> Builder {
        Builder::default()
    }

    pub fn new_multi_thread() -> Builder {
        Builder::default()
    }

    pub fn enable_all(&mut self) -> &mut Builder {
        self
    }

    pub fn enable_io(&mut self) -> &mut Builder {
        self
    }

    pub fn enable_time(&mut self) -> &mut Builder {
        self
    }

    pub fn worker_threads(&mut self, _n: usize) -> &mut Builder {
        self
    }

    pub fn thread_name(&mut self, _name: impl Into<String>) -> &mut Builder {
        self
    }

    pub fn build(&mut self) -> io::Result<Runtime> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a tokio runtime cannot be built in the browser; the page's event loop is the executor",
        ))
    }
}

/// Never constructed; see [`Builder::build`].
#[derive(Debug)]
pub struct Runtime {
    _private: (),
}

impl Runtime {
    pub fn block_on<F: Future>(&self, _future: F) -> F::Output {
        panic!("block_on is not possible in the browser")
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        crate::task::spawn(future)
    }

    pub fn handle(&self) -> &Handle {
        static HANDLE: Handle = Handle { _private: () };
        &HANDLE
    }
}
