//! `tokio::fs` over `std::fs`.
//!
//! `std::fs` compiles on `wasm32-unknown-unknown` and fails every call at runtime with
//! `Unsupported`, which is the right answer in a browser page. These wrappers keep the async
//! signatures NetGet calls.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub async fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    std::fs::write(path, contents)
}

pub async fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    std::fs::read_to_string(path)
}

pub async fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

pub async fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_file(path)
}

pub async fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

pub async fn metadata<P: AsRef<Path>>(path: P) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

#[derive(Debug, Clone)]
pub struct OpenOptions(std::fs::OpenOptions);

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions(std::fs::OpenOptions::new())
    }

    pub fn read(&mut self, on: bool) -> &mut OpenOptions {
        self.0.read(on);
        self
    }

    pub fn write(&mut self, on: bool) -> &mut OpenOptions {
        self.0.write(on);
        self
    }

    pub fn append(&mut self, on: bool) -> &mut OpenOptions {
        self.0.append(on);
        self
    }

    pub fn truncate(&mut self, on: bool) -> &mut OpenOptions {
        self.0.truncate(on);
        self
    }

    pub fn create(&mut self, on: bool) -> &mut OpenOptions {
        self.0.create(on);
        self
    }

    pub fn create_new(&mut self, on: bool) -> &mut OpenOptions {
        self.0.create_new(on);
        self
    }

    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        self.0.open(path).map(File)
    }
}

/// A file handle. Reads and writes are synchronous `std::fs` calls, which is what a
/// single-threaded target permits.
#[derive(Debug)]
pub struct File(std::fs::File);

impl File {
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        std::fs::File::open(path).map(File)
    }

    pub async fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
        std::fs::File::create(path).map(File)
    }

    pub async fn sync_all(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::io::Read;
        let mut tmp = vec![0u8; buf.remaining()];
        match self.0.read(&mut tmp) {
            Ok(n) => {
                buf.put_slice(&tmp[..n]);
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write;
        Poll::Ready(self.0.write(data))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        use std::io::Write;
        Poll::Ready(self.0.flush())
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
