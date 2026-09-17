//! The handshake a *current* MySQL client can complete — and a statement of what it proves.
//!
//! # This server authenticates nothing
//!
//! Read that first, because "offers `caching_sha2_password`" reads like "checks a password"
//! and it is not. Nothing here verifies anything: no password is stored, none is compared,
//! the model is never asked, and every connection is admitted whatever it sends. What this
//! module changes is the *plugin name in the greeting* and the packet that completes the
//! plugin's exchange. A client that would previously have been refused by its own plugin
//! loader now gets as far as sending a query; that is the whole of it.
//!
//! The nonce comes from `opensrv-mysql`'s `AsyncMysqlShim::salt()`, which is a **fixed
//! string**, identical on every connection. That is harmless precisely because nothing is
//! verified against it, and it would be a real defect the moment anything were.
//!
//! # Why the plugin had to change
//!
//! `opensrv-mysql` offers `mysql_native_password` by default. The client plugin of that name
//! was removed in MySQL 9.0, so the shipping `mysql` 9.x CLI cannot load it and refuses before
//! it ever reaches the query phase:
//!
//! ```text
//! ERROR 2059 (HY000): Authentication plugin 'mysql_native_password' cannot be loaded:
//! dlopen(/usr/local/Cellar/mysql/9.3.0/lib/plugin/mysql_native_password.so, 0x0002) …
//! ```
//!
//! So the only client that could reach this server's query path was one lenient enough to
//! keep a plugin the vendor had deleted — which is exactly the "one client agreeing with one
//! bug" shape the project `CLAUDE.md` warns about.
//!
//! **It is not the greeting that kills the connection, it is the auth switch**, and that is
//! worth knowing because it decides which line the fix goes on. Captured from the wire, 9.3
//! CLI against the old server:
//!
//! ```text
//! S->C  …\0mysql_native_password\0          the greeting names a plugin the client lacks
//! C->S  …root\0\0caching_sha2_password\0    the client answers anyway, with its own plugin
//! S->C  \xfe mysql_native_password\0 <nonce>   AuthSwitchRequest — ERROR 2059 is raised here
//! ```
//!
//! The client survives a greeting it cannot honour: it names the plugin it *does* have and
//! waits to be told otherwise. `opensrv-mysql` sends an `AuthSwitchRequest` whenever
//! `auth_plugin_for_username` disagrees with the plugin the client named, and being told
//! "load mysql_native_password" is the one thing a 9.x client cannot do. Hence the empty
//! expectation below: a server that verifies nothing has no business insisting on a plugin.
//! Reverting only the greeting, and leaving that empty expectation in place, does **not**
//! reproduce the failure — which is exactly why the regression test asserts on the client's
//! output rather than on the name in the greeting.
//!
//! # The exchange, and the one packet that is easy to get wrong
//!
//! `caching_sha2_password` ends in one of two ways, and **which one depends on what the
//! client sent**:
//!
//! - **Empty password.** The client sends a zero-length auth response and then expects the
//!   *next* packet to be the OK packet. An `AuthMoreData` here is read as a failure, because
//!   the client checks that the first byte of the packet ending the connection phase is
//!   `0x00`. So: OK, and nothing before it.
//! - **A password was given.** The client sends a 32-byte SHA-256 scramble and then **reads a
//!   packet before it will accept anything else**: `AuthMoreData` (`0x01`) carrying
//!   `fast_auth_success` (`0x03`) — the cache hit — or `perform_full_authentication`
//!   (`0x04`). The OK packet follows the `0x01 0x03`; it does not replace it.
//!
//! `mysql_async`'s own client is an independent statement of the same two cases
//! (`continue_caching_sha2_password_auth`: `0x00` is commented "ok packet for empty password";
//! `0x01 0x03` is followed by a `drop_packet()` for the OK behind it).
//!
//! Which case applies is decided here from the **length of the auth response**, not from the
//! plugin name the client claimed, and that is deliberate: it is also what keeps an old
//! `mysql_native_password` client working. Its scramble is SHA-1 and 20 bytes long, so it
//! takes the plain-OK path it has always taken, and never sees a packet its plugin cannot
//! parse. 32 bytes means caching_sha2; anything else means "answer with OK".
//!
//! # Where the packet is injected
//!
//! `opensrv-mysql` writes the OK packet itself, from inside `init_after_ssl`, and the
//! `authenticate` hook it calls one line earlier takes `&self` and no writer — so there is no
//! seam *inside* the crate for a packet that must precede that OK. There is one *beneath* it:
//! `run_on`'s `W` is any `AsyncWrite`, the same seam the packet bound uses on the read side.
//! [`FastAuthWriter`] sits there, and when [`FastAuthGate`] has been armed by `authenticate`
//! it emits the `0x01 0x03` packet ahead of the next packet the crate writes, taking that
//! packet's sequence id for its own and handing the packet back the next one.
//!
//! **The sequence id is not a detail.** MySQL numbers packets within a phase and a conforming
//! client discards a reply that arrives out of order, so an injected packet that did not
//! renumber the OK behind it would turn a working handshake into a hang.

use std::io::IoSlice;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use tokio::io::AsyncWrite;

/// The plugin this server names in its greeting.
pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";

/// Length of the client's `caching_sha2_password` scramble: a SHA-256 digest.
///
/// This is the discriminator. `mysql_native_password` sends 20 (SHA-1) and an empty password
/// sends 0, and neither of those clients expects — or can parse — the fast-auth packet.
const CACHING_SHA2_SCRAMBLE_LEN: usize = 32;

/// First byte of an `AuthMoreData` packet.
const AUTH_MORE_DATA: u8 = 0x01;

/// `AuthMoreData` payload meaning "the cache had this password; you are in".
const FAST_AUTH_SUCCESS: u8 = 0x03;

/// The `0x01 0x03` packet, without its 4-byte header.
const FAST_AUTH_SUCCESS_PAYLOAD: [u8; 2] = [AUTH_MORE_DATA, FAST_AUTH_SUCCESS];

/// Whether a client that sent `auth_data` is waiting to be told the fast path succeeded.
///
/// See the module docs: this is decided by length rather than by the plugin name the client
/// claimed, so that a `mysql_native_password` client keeps the plain-OK ending it expects.
pub fn awaits_fast_auth_success(auth_data: &[u8]) -> bool {
    auth_data.len() == CACHING_SHA2_SCRAMBLE_LEN
}

/// One bit shared between the shim's `authenticate` hook and the writer beneath the crate.
///
/// `AsyncMysqlShim::authenticate` takes `&self` and returns a `bool`; it cannot write. This is
/// how it says "one more packet, before whatever you write next".
#[derive(Debug, Default)]
pub struct FastAuthGate {
    armed: AtomicBool,
}

impl FastAuthGate {
    /// Ask for a `fast_auth_success` packet ahead of the next packet written.
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Consume the request, if there is one.
    fn take(&self) -> bool {
        self.armed.swap(false, Ordering::SeqCst)
    }

    /// Whether a request is outstanding, without consuming it.
    fn peek(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }
}

/// What the writer is doing with the bytes passing through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The ordinary case, and the only one after the connection phase: bytes go straight out.
    Passthrough,
    /// The gate was armed; collecting the next packet's 4-byte header so it can be renumbered.
    Header,
}

/// An `AsyncWrite` that can put one `AuthMoreData` packet in front of the crate's OK packet.
///
/// It is inert — a plain forwarder, no buffering, no parsing — until [`FastAuthGate::arm`] is
/// called, which happens at most once per connection and only during the connection phase.
#[derive(Debug)]
pub struct FastAuthWriter<W> {
    inner: W,
    gate: Arc<FastAuthGate>,
    stage: Stage,
    /// The next packet's header, while it is being collected a fragment at a time.
    header: [u8; 4],
    header_len: usize,
    /// Bytes owed to `inner`: the injected packet, then the renumbered header behind it.
    queued: Vec<u8>,
    queued_pos: usize,
}

impl<W: AsyncWrite + Unpin> FastAuthWriter<W> {
    pub fn new(inner: W, gate: Arc<FastAuthGate>) -> Self {
        Self {
            inner,
            gate,
            stage: Stage::Passthrough,
            header: [0; 4],
            header_len: 0,
            queued: Vec::new(),
            queued_pos: 0,
        }
    }

    /// Push whatever this writer has accepted but not yet handed to `inner`.
    ///
    /// Everything else — `poll_write`, `poll_flush`, `poll_shutdown` — goes through here
    /// first, so bytes reported as written are always delivered in order.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        while self.queued_pos < self.queued.len() {
            let n =
                ready!(Pin::new(&mut self.inner).poll_write(cx, &self.queued[self.queued_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "MySQL connection closed while writing the caching_sha2_password reply",
                )));
            }
            self.queued_pos += n;
        }
        self.queued.clear();
        self.queued_pos = 0;
        Poll::Ready(Ok(()))
    }

    /// The byte path, shared by `poll_write` and the armed case of `poll_write_vectored`.
    fn poll_write_bytes(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let me = self;
        ready!(me.poll_drain(cx))?;

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if me.stage == Stage::Passthrough {
            if me.gate.take() {
                me.stage = Stage::Header;
                me.header_len = 0;
            } else {
                return Pin::new(&mut me.inner).poll_write(cx, buf);
            }
        }

        // Collecting the header of the packet the fast-auth packet must precede. It arrives in
        // one `write_vectored` from `PacketWriter::end_packet` in practice, but a short write
        // is legal, so take it a byte at a time and only decide once all four are in hand.
        let take = (4 - me.header_len).min(buf.len());
        me.header[me.header_len..me.header_len + take].copy_from_slice(&buf[..take]);
        me.header_len += take;

        if me.header_len == 4 {
            // The injected packet takes this packet's sequence id, and this packet takes the
            // next one. MySQL numbers within a phase and the client checks it.
            let seq = me.header[3];
            me.queued.clear();
            me.queued_pos = 0;
            me.queued
                .extend_from_slice(&[FAST_AUTH_SUCCESS_PAYLOAD.len() as u8, 0x00, 0x00, seq]);
            me.queued.extend_from_slice(&FAST_AUTH_SUCCESS_PAYLOAD);
            me.queued.extend_from_slice(&[
                me.header[0],
                me.header[1],
                me.header[2],
                seq.wrapping_add(1),
            ]);
            me.header_len = 0;
            me.stage = Stage::Passthrough;

            // Try to get it out now; if the socket is full the bytes stay queued and the next
            // poll — a write, a flush or the shutdown — drains them. An error here is this
            // call's error, not a later one's.
            if let Poll::Ready(res) = me.poll_drain(cx) {
                res?;
            }
        }

        Poll::Ready(Ok(take))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for FastAuthWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().poll_write_bytes(cx, buf)
    }

    /// Vectored writes go straight through, and that is not an optimisation.
    ///
    /// `PacketWriter::end_packet` hands the header and the payload to one `write_vectored`.
    /// Tokio's *default* implementation of this method forwards only the first slice, so a
    /// writer that does not implement it turns every MySQL packet into two writes and two
    /// segments — the initial greeting then arrives as a 4-byte read followed by the rest.
    /// `tests/server/mysql/packet_limit_test.rs` reads that greeting from a raw socket and
    /// fails on it, which is how this was found.
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;

        if me.stage == Stage::Passthrough && !me.gate.peek() {
            return Pin::new(&mut me.inner).poll_write_vectored(cx, bufs);
        }

        // One packet in the life of a connection needs its header inspected, and it goes
        // through the byte path a slice at a time. A short write is what `write_vectored`
        // promises anyway, and `end_packet` writes the remainder with `write_all`.
        match bufs.iter().find(|b| !b.is_empty()) {
            Some(first) => me.poll_write_bytes(cx, first),
            None => Poll::Ready(Ok(0)),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;
        Pin::new(&mut me.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;
        Pin::new(&mut me.inner).poll_shutdown(cx)
    }
}
