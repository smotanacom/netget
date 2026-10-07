//! Cancellation reaches russh's internal driver without duplicating the socket.
use std::{future::Future, io, pin::Pin, task::{Context, Poll}};
use tokio::{io::{AsyncRead, AsyncWrite, ReadBuf}, net::TcpStream};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

pub struct Owner(CancellationToken);
impl Owner {
    pub fn token(&self) -> CancellationToken { self.0.clone() }
}
impl Drop for Owner {
    fn drop(&mut self) { self.0.cancel(); }
}
pub struct OwnedStream {
    inner: TcpStream,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
}
impl OwnedStream {
    pub fn new(inner: TcpStream) -> (Self, Owner) {
        let token = CancellationToken::new();
        (Self { inner, cancelled: Box::pin(token.clone().cancelled_owned()) }, Owner(token))
    }
    fn cancellation(&mut self, cx: &mut Context<'_>) -> Option<io::Error> {
        // Polling registers the internal driver task's waker even if the native
        // socket stays silent. Owner drop wakes it and turns I/O into an error.
        self.cancelled.as_mut().poll(cx).is_ready().then(|| io::Error::new(io::ErrorKind::ConnectionAborted, "NETCONF owned I/O cancelled"))
    }
}
impl AsyncRead for OwnedStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Some(error) = self.cancellation(cx) { return Poll::Ready(Err(error)) }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}
impl AsyncWrite for OwnedStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if let Some(error) = self.cancellation(cx) { return Poll::Ready(Err(error)) }
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(error) = self.cancellation(cx) { return Poll::Ready(Err(error)) }
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(error) = self.cancellation(cx) { return Poll::Ready(Err(error)) }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

