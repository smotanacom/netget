//! A virtual loopback network.
//!
//! There are no sockets in a browser page. What a NetGet server needs from one is small: bind
//! a port, accept connections, and read and write bytes on each. This module provides exactly
//! that over in-memory pipes:
//!
//! - [`TcpListener::bind`] claims a port in a process-wide table (port `0` picks a free
//!   ephemeral one, as the kernel would).
//! - [`TcpStream::connect`] looks the port up, builds a [`tokio::io::duplex`] pair, hands the
//!   listener's end to its accept queue and returns the other. `ConnectionRefused` if nothing
//!   listens there, as on a real loopback.
//! - [`UdpSocket::bind`] claims a port the same way; `send_to` delivers a datagram to the
//!   socket bound on the destination port, with the sender's address attached.
//!
//! Everything else on the page (the demo's Telnet terminal, the fake browser) is a
//! `TcpStream::connect` away, so the protocol servers are exercised through the same
//! accept-split-read-write path they run on natively. The host part of an address is ignored:
//! there is only one machine.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::{mpsc, Mutex as AsyncMutex};

/// Bytes buffered per direction before a writer has to wait for the reader.
const PIPE_CAPACITY: usize = 256 * 1024;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const FIRST_EPHEMERAL: u16 = 49152;

type Incoming = (TcpStream, SocketAddr);

struct Registry {
    listeners: HashMap<u16, mpsc::UnboundedSender<Incoming>>,
    udp: HashMap<u16, mpsc::UnboundedSender<Datagram>>,
    next_ephemeral: u16,
    next_peer_port: u16,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            listeners: HashMap::new(),
            udp: HashMap::new(),
            next_ephemeral: FIRST_EPHEMERAL,
            next_peer_port: 32768,
        })
    })
}

/// Ports with a listener, for a page that wants to offer them to the user.
pub fn listening_ports() -> Vec<u16> {
    let reg = registry().lock().expect("registry lock");
    let mut ports: Vec<u16> = reg.listeners.keys().copied().collect();
    ports.sort_unstable();
    ports
}

/// Something that can name a socket address. Mirrors tokio's trait for the forms NetGet uses;
/// hostnames resolve to loopback because that is the only host there is.
pub trait ToSocketAddrs {
    fn to_socket_addr(&self) -> io::Result<SocketAddr>;
}

impl ToSocketAddrs for SocketAddr {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(*self)
    }
}

impl<T: ToSocketAddrs + ?Sized> ToSocketAddrs for &T {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        (**self).to_socket_addr()
    }
}

impl ToSocketAddrs for (IpAddr, u16) {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::new(self.0, self.1))
    }
}

impl ToSocketAddrs for (Ipv4Addr, u16) {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::new(IpAddr::V4(self.0), self.1))
    }
}

impl ToSocketAddrs for (Ipv6Addr, u16) {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::new(IpAddr::V6(self.0), self.1))
    }
}

impl ToSocketAddrs for (&str, u16) {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        let ip = self.0.parse::<IpAddr>().unwrap_or(LOOPBACK);
        Ok(SocketAddr::new(ip, self.1))
    }
}

impl ToSocketAddrs for (String, u16) {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        (self.0.as_str(), self.1).to_socket_addr()
    }
}

impl ToSocketAddrs for str {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        if let Ok(addr) = self.parse::<SocketAddr>() {
            return Ok(addr);
        }
        let (host, port) = self
            .rsplit_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket address"))?;
        let port: u16 = port
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid port value"))?;
        (host, port).to_socket_addr()
    }
}

impl ToSocketAddrs for String {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        self.as_str().to_socket_addr()
    }
}

impl ToSocketAddrs for [SocketAddr] {
    fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        self.first()
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no socket address given"))
    }
}

/// Resolve a name. Everything is loopback here.
pub async fn lookup_host<T: ToSocketAddrs>(
    host: T,
) -> io::Result<impl Iterator<Item = SocketAddr>> {
    host.to_socket_addr().map(std::iter::once)
}

/// A listening port on the virtual network.
pub struct TcpListener {
    local: SocketAddr,
    rx: AsyncMutex<mpsc::UnboundedReceiver<Incoming>>,
}

impl TcpListener {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let requested = addr.to_socket_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let port = {
            let mut reg = registry().lock().expect("registry lock");
            let port = if requested.port() == 0 {
                let mut candidate = reg.next_ephemeral;
                let mut tries = 0u32;
                while reg.listeners.contains_key(&candidate) {
                    candidate = candidate.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
                    tries += 1;
                    if tries > u16::MAX as u32 {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "no free ephemeral port",
                        ));
                    }
                }
                reg.next_ephemeral = candidate.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
                candidate
            } else if reg.listeners.contains_key(&requested.port()) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("port {} is already in use", requested.port()),
                ));
            } else {
                requested.port()
            };
            reg.listeners.insert(port, tx);
            port
        };
        let ip = if requested.ip().is_unspecified() {
            LOOPBACK
        } else {
            requested.ip()
        };
        Ok(TcpListener {
            local: SocketAddr::new(ip, port),
            rx: AsyncMutex::new(rx),
        })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let mut rx = self.rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "listener was unregistered"))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        if let Ok(mut reg) = registry().lock() {
            reg.listeners.remove(&self.local.port());
        }
    }
}

impl std::fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpListener")
            .field("local", &self.local)
            .finish()
    }
}

/// One end of a connection on the virtual network.
///
/// Held as a pre-split read and write half so that `split(&mut self)` can hand out both
/// without a second mutable borrow; `AsyncRead`/`AsyncWrite` on the whole stream delegate to
/// the halves.
pub struct TcpStream {
    read: tokio::io::ReadHalf<DuplexStream>,
    write: tokio::io::WriteHalf<DuplexStream>,
    local: SocketAddr,
    peer: SocketAddr,
}

impl TcpStream {
    fn from_duplex(io: DuplexStream, local: SocketAddr, peer: SocketAddr) -> Self {
        let (read, write) = tokio::io::split(io);
        TcpStream {
            read,
            write,
            local,
            peer,
        }
    }

    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let target = addr.to_socket_addr()?;
        let (tx, peer_port) = {
            let mut reg = registry().lock().expect("registry lock");
            let tx = reg.listeners.get(&target.port()).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("nothing is listening on port {}", target.port()),
                )
            })?;
            let port = reg.next_peer_port;
            reg.next_peer_port = if port >= FIRST_EPHEMERAL - 1 {
                32768
            } else {
                port + 1
            };
            (tx, port)
        };
        let server_addr = SocketAddr::new(LOOPBACK, target.port());
        let client_addr = SocketAddr::new(LOOPBACK, peer_port);
        let (client_end, server_end) = tokio::io::duplex(PIPE_CAPACITY);
        let server_stream = TcpStream::from_duplex(server_end, server_addr, client_addr);
        tx.send((server_stream, client_addr)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("listener on port {} is gone", target.port()),
            )
        })?;
        Ok(TcpStream::from_duplex(client_end, client_addr, server_addr))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer)
    }

    pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
        Ok(())
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        Ok(true)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn set_linger(&self, _dur: Option<std::time::Duration>) -> io::Result<()> {
        Ok(())
    }

    pub fn into_split(self) -> (tcp::OwnedReadHalf, tcp::OwnedWriteHalf) {
        (
            tcp::OwnedReadHalf { inner: self.read },
            tcp::OwnedWriteHalf { inner: self.write },
        )
    }

    pub fn split(&mut self) -> (tcp::ReadHalf<'_>, tcp::WriteHalf<'_>) {
        (
            tcp::ReadHalf {
                inner: &mut self.read,
            },
            tcp::WriteHalf {
                inner: &mut self.write,
            },
        )
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

impl std::fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpStream")
            .field("local", &self.local)
            .field("peer", &self.peer)
            .finish()
    }
}

/// Read and write halves of a [`TcpStream`], owned and borrowed.
pub mod tcp {
    use super::*;

    pub struct OwnedReadHalf {
        pub(super) inner: tokio::io::ReadHalf<DuplexStream>,
    }

    pub struct OwnedWriteHalf {
        pub(super) inner: tokio::io::WriteHalf<DuplexStream>,
    }

    pub struct ReadHalf<'a> {
        pub(super) inner: &'a mut tokio::io::ReadHalf<DuplexStream>,
    }

    pub struct WriteHalf<'a> {
        pub(super) inner: &'a mut tokio::io::WriteHalf<DuplexStream>,
    }

    impl AsyncRead for OwnedReadHalf {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for OwnedWriteHalf {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    impl AsyncRead for ReadHalf<'_> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut *self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for WriteHalf<'_> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut *self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut *self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut *self.inner).poll_shutdown(cx)
        }
    }
}

/// A datagram socket on the virtual network.
///
/// Bound ports live in the same table as listeners, keyed separately. `send_to` looks the
/// destination port up and delivers the datagram to that socket's queue with the sender's
/// address attached; nothing is fragmented, reordered or lost. Multicast and broadcast
/// addresses deliver to the socket bound on that port, if any — there is one host, so that is
/// what "everyone on the link" means here. Multicast joins and TTLs are accepted and ignored.
pub struct UdpSocket {
    local: SocketAddr,
    rx: AsyncMutex<mpsc::UnboundedReceiver<Datagram>>,
    peer: Mutex<Option<SocketAddr>>,
    /// A datagram taken off the queue by `readable` and not yet handed out.
    peeked: Mutex<Option<Datagram>>,
}

type Datagram = (Vec<u8>, SocketAddr);

/// Largest datagram the virtual network carries; the wire limit, so a server that sizes
/// its buffer for the wire is never surprised.
const MAX_DATAGRAM: usize = 65_507;

fn udp_unsupported(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{what} is not available on the browser's virtual network"),
    )
}

impl UdpSocket {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let requested = addr.to_socket_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let port = {
            let mut reg = registry().lock().expect("registry lock");
            let port = if requested.port() == 0 {
                let mut candidate = reg.next_ephemeral;
                let mut tries = 0u32;
                while reg.udp.contains_key(&candidate) {
                    candidate = candidate.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
                    tries += 1;
                    if tries > u16::MAX as u32 {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "no free ephemeral port",
                        ));
                    }
                }
                reg.next_ephemeral = candidate.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
                candidate
            } else if reg.udp.contains_key(&requested.port()) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("udp port {} is already in use", requested.port()),
                ));
            } else {
                requested.port()
            };
            reg.udp.insert(port, tx);
            port
        };
        let ip = if requested.ip().is_unspecified() {
            LOOPBACK
        } else {
            requested.ip()
        };
        Ok(UdpSocket {
            local: SocketAddr::new(ip, port),
            rx: AsyncMutex::new(rx),
            peer: Mutex::new(None),
            peeked: Mutex::new(None),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer
            .lock()
            .expect("peer lock")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket is not connected"))
    }

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let target = addr.to_socket_addr()?;
        *self.peer.lock().expect("peer lock") = Some(SocketAddr::new(LOOPBACK, target.port()));
        Ok(())
    }

    fn deliver(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        if buf.len() > MAX_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram larger than the maximum",
            ));
        }
        let tx = registry()
            .lock()
            .expect("registry lock")
            .udp
            .get(&target.port())
            .cloned();
        // As on a real host, a datagram to a port nobody listens on is silently dropped.
        if let Some(tx) = tx {
            let _ = tx.send((buf.to_vec(), self.local));
        }
        Ok(buf.len())
    }

    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        self.deliver(buf, target.to_socket_addr()?)
    }

    pub fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.deliver(buf, target)
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let peer = self.peer_addr()?;
        self.deliver(buf, peer)
    }

    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        let peer = self.peer_addr()?;
        self.deliver(buf, peer)
    }

    async fn next_datagram(&self) -> io::Result<Datagram> {
        if let Some(d) = self.peeked.lock().expect("peek lock").take() {
            return Ok(d);
        }
        let mut rx = self.rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "socket was unregistered"))
    }

    fn copy_out(buf: &mut [u8], data: &[u8]) -> usize {
        // A datagram larger than the buffer is truncated, as recvfrom(2) does.
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        n
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (data, from) = self.next_datagram().await?;
        Ok((Self::copy_out(buf, &data), from))
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let peer = self.peer_addr()?;
        loop {
            let (data, from) = self.next_datagram().await?;
            if from.port() == peer.port() {
                return Ok(Self::copy_out(buf, &data));
            }
        }
    }

    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (data, from) = self.next_datagram().await?;
        let n = Self::copy_out(buf, &data);
        *self.peeked.lock().expect("peek lock") = Some((data, from));
        Ok((n, from))
    }

    /// Waits until a datagram is queued. What `try_recv_from` then returns.
    pub async fn readable(&self) -> io::Result<()> {
        if self.peeked.lock().expect("peek lock").is_some() {
            return Ok(());
        }
        let d = self.next_datagram().await?;
        *self.peeked.lock().expect("peek lock") = Some(d);
        Ok(())
    }

    pub async fn writable(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self.peeked.lock().expect("peek lock").take() {
            Some((data, from)) => Ok((Self::copy_out(buf, &data), from)),
            None => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "no datagram queued",
            )),
        }
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.try_recv_from(buf).map(|(n, _)| n)
    }

    pub fn set_broadcast(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        Ok(true)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }

    pub fn join_multicast_v4(&self, _group: Ipv4Addr, _interface: Ipv4Addr) -> io::Result<()> {
        Ok(())
    }

    pub fn leave_multicast_v4(&self, _group: Ipv4Addr, _interface: Ipv4Addr) -> io::Result<()> {
        Ok(())
    }

    pub fn join_multicast_v6(&self, _group: &Ipv6Addr, _interface: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn leave_multicast_v6(&self, _group: &Ipv6Addr, _interface: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn set_multicast_ttl_v4(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn set_multicast_loop_v4(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    pub fn set_multicast_if_v4(&self, _interface: Ipv4Addr) -> io::Result<()> {
        Ok(())
    }

    pub fn from_std(_socket: std::net::UdpSocket) -> io::Result<UdpSocket> {
        Err(udp_unsupported("adopting an OS socket"))
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        if let Ok(mut reg) = registry().lock() {
            reg.udp.remove(&self.local.port());
        }
    }
}

impl std::fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpSocket")
            .field("local", &self.local)
            .finish()
    }
}

/// UDP ports with a socket bound, for a page that wants to offer them to the user.
pub fn bound_udp_ports() -> Vec<u16> {
    let reg = registry().lock().expect("registry lock");
    let mut ports: Vec<u16> = reg.udp.keys().copied().collect();
    ports.sort_unstable();
    ports
}
