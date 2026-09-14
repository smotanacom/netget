//! `tokio::process` shapes with no processes behind them.
//!
//! A browser page cannot spawn interpreters, so NetGet's script handlers (Python/JS/Go/Perl
//! subprocesses) cannot run here. `Command::spawn` reports `Unsupported`; the `Child` and pipe
//! types exist only so the code that names them compiles.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub use std::process::{ExitStatus, Output, Stdio};

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "spawning a process is not available in the browser",
    )
}

#[derive(Debug)]
pub struct Command {
    program: String,
    args: Vec<String>,
}

impl Command {
    pub fn new<S: AsRef<OsStr>>(program: S) -> Command {
        Command {
            program: program.as_ref().to_string_lossy().into_owned(),
            args: Vec::new(),
        }
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Command {
        self.args.push(arg.as_ref().to_string_lossy().into_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self.arg(a);
        }
        self
    }

    pub fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, _key: K, _val: V) -> &mut Command {
        self
    }

    pub fn envs<I, K, V>(&mut self, _vars: I) -> &mut Command
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self
    }

    pub fn env_clear(&mut self) -> &mut Command {
        self
    }

    pub fn current_dir<P: AsRef<Path>>(&mut self, _dir: P) -> &mut Command {
        self
    }

    pub fn stdin<T: Into<Stdio>>(&mut self, _cfg: T) -> &mut Command {
        self
    }

    pub fn stdout<T: Into<Stdio>>(&mut self, _cfg: T) -> &mut Command {
        self
    }

    pub fn stderr<T: Into<Stdio>>(&mut self, _cfg: T) -> &mut Command {
        self
    }

    pub fn kill_on_drop(&mut self, _kill: bool) -> &mut Command {
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        Err(unsupported())
    }

    pub async fn output(&mut self) -> io::Result<Output> {
        Err(unsupported())
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        Err(unsupported())
    }

    pub fn program(&self) -> &str {
        &self.program
    }
}

/// Never constructed: [`Command::spawn`] always fails.
#[derive(Debug)]
pub struct Child {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Child {
    pub fn id(&self) -> Option<u32> {
        None
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        Err(unsupported())
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Err(unsupported())
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn start_kill(&mut self) -> io::Result<()> {
        Err(unsupported())
    }

    pub async fn wait_with_output(self) -> io::Result<Output> {
        Err(unsupported())
    }
}

#[derive(Debug)]
pub struct ChildStdin {
    _private: (),
}

#[derive(Debug)]
pub struct ChildStdout {
    _private: (),
}

#[derive(Debug)]
pub struct ChildStderr {
    _private: (),
}

impl AsyncWrite for ChildStdin {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(unsupported()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(unsupported()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(unsupported()))
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(unsupported()))
    }
}

impl AsyncRead for ChildStderr {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(unsupported()))
    }
}
