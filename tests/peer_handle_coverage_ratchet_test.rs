//! A server with a TCP accept loop and no **peer handle** leaves the operator holding a
//! decision they cannot act on.
//!
//! `[ message this peer ]` and `[ disconnect this peer ]` are drawn on every live peer row, and
//! `src/tui/projection.rs` enables them from `AppState::has_peer_handle` alone. Without a
//! handle both stay dim and say "this protocol cannot message a peer from here yet". That is
//! merely an absence right up until the moment it is not: a `manual` rule parks the peer's
//! event for a **human** (`src/state/intercepts.rs`, 300s by default), the dashboard flags the
//! connection `⚠ waiting for YOUR answer` — and the operator is being asked to decide about a
//! connection they cannot inspect, cannot answer out of band, and cannot hang up. Manual-first
//! is the dashboard's whole premise, so the affordance is at its least optional exactly where
//! it is missing.
//!
//! Adopting it is ~40 lines (`src/server/whois/mod.rs` is the reference), so the cost of the
//! gap is not the work. It is that nobody notices: a missing button reports nothing, fails no
//! test, and looks like a design decision.
//!
//! # The rule
//!
//! Over `src/server/*/mod.rs` and `src/server/*/*/mod.rs`, with `//` comments stripped first,
//! a protocol **runs a TCP accept loop** when its `mod.rs` contains any of
//! `TcpListener`, `accept_bounded::accept_bounded(` or `listener.accept().await` — and is not a
//! Unix-domain listener wearing the third of those (see the false-positive note). Such a
//! protocol must either call `peer_support::register_peer_channel` or appear in
//! [`NO_PEER_HANDLE_BASELINE`] with a reason.
//!
//! **The baseline may only shrink.** A protocol that gains a handle and stays listed fails the
//! test as a stale entry, so the list cannot rot in the direction that hides progress.
//!
//! # Reasons are checked, not asserted
//!
//! The usual failure of a baseline-with-comments is that the comment stops being true and
//! nothing notices — this repository has the class recorded several times over. So most
//! reasons here are a **key with a source marker**, and the test asserts the marker is still
//! present in that protocol's `mod.rs`. If `openai` stops serving through hyper, its reason
//! stops being true and the build fails, rather than the file quietly lying. The five entries
//! reviewed by hand carry [`Reason::Reviewed`] and their argument is in the table below, which
//! is the only kind of entry a reader has to take on trust.
//!
//! # Anchoring, and the measurement that made it wider
//!
//! `PROTOCOL_QUALITY.md` measured this as "13 of 32 TCP servers", anchored on the token
//! `TcpListener` in `mod.rs`. That anchor **under-reports by a factor of three**: 62 of the 92
//! TCP accept-loop servers never name the type, because they bind through
//! `accept_bounded`/`socket_helpers` or take a `listener` from a helper and only ever write
//! `listener.accept().await`. `ftp`, `smtp`, `imap`, `tcp` and `telnet` are all in that group,
//! and `tcp` and `telnet` are the two protocols the project `CLAUDE.md` names as *having* the
//! affordance. A scan anchored on one filename or one token is the recurring shape of a wrong
//! number here, so this test uses the wider rule and states the narrower figure only as
//! history.
//!
//! # False positives, measured
//!
//! The naive rule matched **94** directories, of which **2 are not TCP at all**:
//! `socket_file` and `ssh_agent` accept on a `UnixListener` and were caught by
//! `listener.accept().await`. Both are excluded explicitly — a Unix-domain listener with no
//! `TcpListener` and no `accept_bounded` is not a TCP accept loop — which is a 2.1% naive
//! false-positive rate and zero after the exclusion. A build-failing check earns nothing by
//! being approximately right: `startup_param_drift_test.rs` records what happens next, which is
//! that people edit the baseline instead of the code.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp \
//!       --test peer_handle_coverage_ratchet_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Why a TCP accept-loop server has no peer handle.
///
/// Every variant but [`Reason::Reviewed`] and [`Reason::Unreviewed`] names a token that must
/// still be present in the protocol's `mod.rs`, so the reason is re-derived on every run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reason {
    /// hyper owns the socket from `serve_connection` onward. There is no write half to share,
    /// and raw bytes pushed alongside hyper's framing would desynchronise the HTTP/1.1 or
    /// HTTP/2 stream rather than reach the peer as a message.
    HyperOwnsSocket,
    /// `axum::serve` owns its own accept loop, and this server additionally relays the public
    /// socket to a loopback backend — so the handler's peer is the relay, not the client.
    AxumOwnsSocket,
    /// USB/IP: the "peer" is a USB host attaching a device, and the protocol's vocabulary is
    /// URBs against endpoints, not a byte stream anything could be injected into.
    ///
    /// **No protocol carries this reason any more.** All six USB servers adopted a peer handle
    /// in September 2026, and this ratchet is what said so — it failed asking for their six
    /// lines to be deleted, which is the shrink-only half doing its job. The variant is kept
    /// because the reasoning still holds for anything that speaks URBs rather than bytes, and
    /// because deleting it would lose why the six were ever exempt.
    #[allow(dead_code)]
    UsbIp,
    /// A WebSocket: the write side is a `SplitSink` behind a writer task, not an `AsyncWrite`.
    /// `peer_support` writes `ActionResult::Output` bytes straight to the socket, which for a
    /// WebSocket is an unframed payload the peer will reject.
    WebSocketFrames,
    /// A byte-for-byte tunnel: once `copy_bidirectional` starts, the socket is moved into it
    /// for the life of the connection and anything injected would land inside whatever
    /// protocol the tunnel carries.
    Tunnel,
    /// The SSH transport is russh's; NetGet never holds the socket after the handshake.
    RusshOwnsSocket,
    /// Reviewed by hand in the September 2026 peer-handle pass; see the table below.
    Reviewed,
    /// Not reviewed in that pass. Not a claim that a handle is impossible — a claim that
    /// nobody has looked. Shrinking this is the next piece of work.
    Unreviewed,
}

impl Reason {
    /// The token that must still appear in the protocol's `mod.rs` for this reason to hold.
    fn marker(self) -> Option<&'static str> {
        match self {
            Reason::HyperOwnsSocket => Some("hyper::server::conn"),
            Reason::AxumOwnsSocket => Some("axum::serve"),
            Reason::UsbIp => Some("usbip"),
            Reason::WebSocketFrames => Some("tokio_tungstenite"),
            Reason::Tunnel => Some("copy_bidirectional"),
            Reason::RusshOwnsSocket => Some("russh"),
            Reason::Reviewed | Reason::Unreviewed => None,
        }
    }
}

/// TCP accept-loop servers with no peer handle, and why.
///
/// **May only shrink.** Adopting the handle means deleting the line; a line left behind for a
/// protocol that has one fails the test.
///
/// The five [`Reason::Reviewed`] entries were each read in the September 2026 pass and refused
/// for a specific reason, which is recorded here because no marker can carry it:
///
/// * **`dot`** — DNS over TLS frames every message with a two-byte length prefix that the
///   *session* writes, not the action. `DnsProtocol`'s `Output` is a bare DNS message, so
///   `peer_support` writing it verbatim would desynchronise the connection permanently. And an
///   unsolicited DNS response carries a transaction id no resolver is waiting on, so there is
///   nothing useful to send even if the framing were right.
/// * **`llmnr`** — declares `.connectionless()`, so `AppState::cleanup_old_connections` evicts
///   its connection rows after ten seconds of silence *by design*. A handle would be registered
///   against a row the sweep is about to remove. Its TCP reply is length-prefixed by the
///   session too, exactly as `dot`'s is.
/// * **`mysql`** — the write half is moved into `opensrv`'s `AsyncMysqlIntermediary::run_on`,
///   which owns the packet loop and the sequence numbering. Sharing it is not just awkward:
///   MySQL packets carry a sequence id, so an out-of-band write desynchronises the client even
///   if the bytes are valid.
/// * **`nfs`** — NetGet binds the public listener and relays every connection to `nfsserve` on
///   a loopback port (`src/server/nfs/guard.rs`). The handler's peer is the relay, and an
///   injected RPC record would have to carry the xid of a call the client made, which nothing
///   outside the exchange knows.
/// * **`postgresql`** — `pgwire::tokio::process_socket` takes a concrete `TcpStream` and never
///   exposes it again; `src/server/postgresql/mod.rs` already records that this is why the idle
///   watchdog aborts the task rather than sending a FATAL. The same absence of a seam is why
///   there is no write half to hand `peer_support`.
const NO_PEER_HANDLE_BASELINE: &[(&str, Reason)] = &[
    ("couchdb", Reason::HyperOwnsSocket),
    ("doh", Reason::HyperOwnsSocket),
    ("dot", Reason::Reviewed),
    ("dynamo", Reason::HyperOwnsSocket),
    ("elasticsearch", Reason::HyperOwnsSocket),
    ("etcd", Reason::HyperOwnsSocket),
    ("git", Reason::HyperOwnsSocket),
    ("grpc", Reason::HyperOwnsSocket),
    ("hls", Reason::Unreviewed),
    ("http", Reason::HyperOwnsSocket),
    ("ipp", Reason::HyperOwnsSocket),
    ("jsonrpc", Reason::HyperOwnsSocket),
    ("kubernetes", Reason::HyperOwnsSocket),
    ("ldap", Reason::Unreviewed),
    ("llmnr", Reason::Reviewed),
    ("maven", Reason::HyperOwnsSocket),
    ("mcp", Reason::AxumOwnsSocket),
    ("mercurial", Reason::HyperOwnsSocket),
    ("mysql", Reason::Reviewed),
    ("nfc", Reason::Unreviewed),
    ("nfs", Reason::Reviewed),
    ("npm", Reason::HyperOwnsSocket),
    ("oauth2", Reason::HyperOwnsSocket),
    ("oci_registry", Reason::HyperOwnsSocket),
    ("ollama", Reason::HyperOwnsSocket),
    ("openai", Reason::HyperOwnsSocket),
    ("openapi", Reason::HyperOwnsSocket),
    ("openid", Reason::HyperOwnsSocket),
    ("postgresql", Reason::Reviewed),
    ("proxy", Reason::Tunnel),
    ("pypi", Reason::HyperOwnsSocket),
    ("rss", Reason::HyperOwnsSocket),
    ("s3", Reason::HyperOwnsSocket),
    ("saml_idp", Reason::HyperOwnsSocket),
    ("saml_sp", Reason::HyperOwnsSocket),
    ("snowflake", Reason::HyperOwnsSocket),
    ("socks5", Reason::Tunnel),
    ("spark", Reason::HyperOwnsSocket),
    ("sqs", Reason::HyperOwnsSocket),
    ("ssh", Reason::RusshOwnsSocket),
    ("webdav", Reason::HyperOwnsSocket),
    ("webrtc", Reason::WebSocketFrames),
    ("webrtc_signaling", Reason::WebSocketFrames),
    ("websocket", Reason::WebSocketFrames),
    ("xmlrpc", Reason::HyperOwnsSocket),
    ("yarn", Reason::HyperOwnsSocket),
];

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

/// Drop `//` comments so prose *about* a token is not read as the token.
///
/// This repository has hit the matching-prose false positive at least three times, most
/// recently on a ratchet that flagged its own explanatory comment.
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn server_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server")
}

/// `<protocol>` or `<family>/<protocol>` for every `mod.rs` under `src/server`.
fn server_mod_files() -> Vec<(String, PathBuf)> {
    let root = server_root();
    let mut out = Vec::new();
    let mut dirs = vec![(String::new(), root.clone())];
    while let Some((prefix, dir)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let key = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let module = path.join("mod.rs");
            if module.is_file() {
                out.push((key.clone(), module));
            }
            // One level of nesting is all the tree has (`usb/serial`, `bluetooth_ble_*` are
            // flat), and recursing further would walk into nothing.
            if prefix.is_empty() {
                dirs.push((key, path));
            }
        }
    }
    out.sort();
    out
}

struct ServerSource {
    name: String,
    body: String,
}

impl ServerSource {
    fn has(&self, needle: &str) -> bool {
        self.body.contains(needle)
    }

    /// Does this server run a TCP accept loop of its own?
    ///
    /// Three spellings, because the tree has three. `listener.accept().await` is the one that
    /// catches a server whose listener came from a helper — which is 62 of the 92, including
    /// both protocols the project `CLAUDE.md` cites as having a peer handle.
    fn runs_tcp_accept_loop(&self) -> bool {
        let matched = self.has("TcpListener")
            || self.has("accept_bounded::accept_bounded(")
            || self.has("listener.accept().await");
        if !matched {
            return false;
        }
        // The one false positive of the rule above: a Unix-domain listener also spells its
        // accept `listener.accept().await`. `socket_file` and `ssh_agent` are the two, and a
        // peer handle over an `AF_UNIX` stream is a separate question from this one.
        let unix_only =
            self.has("UnixListener") && !self.has("TcpListener") && !self.has("accept_bounded(");
        !unix_only
    }

    fn registers_peer_handle(&self) -> bool {
        self.has("register_peer_channel")
    }
}

fn load_servers() -> Vec<ServerSource> {
    server_mod_files()
        .into_iter()
        .map(|(name, path)| {
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            ServerSource {
                name,
                body: strip_comments(&raw),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn every_tcp_accept_loop_server_has_a_peer_handle_or_a_reason() {
    let servers = load_servers();
    let baselined: BTreeSet<&str> = NO_PEER_HANDLE_BASELINE.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        baselined.len(),
        NO_PEER_HANDLE_BASELINE.len(),
        "NO_PEER_HANDLE_BASELINE lists a protocol twice"
    );

    let mut undeclared = Vec::new();
    for server in &servers {
        if !server.runs_tcp_accept_loop() || server.registers_peer_handle() {
            continue;
        }
        if !baselined.contains(server.name.as_str()) {
            undeclared.push(server.name.clone());
        }
    }

    assert!(
        undeclared.is_empty(),
        "these servers run a TCP accept loop and register no peer handle, so the dashboard's\n\
         [ message this peer ] / [ disconnect this peer ] are dim on every one of their peers —\n\
         including a peer whose event is parked waiting for a human's answer:\n\n  {}\n\n\
         Adopt the handle (`src/server/whois/mod.rs` is the ~40-line reference), or add an entry\n\
         to NO_PEER_HANDLE_BASELINE in {} with a reason. Adding a line is not how you pass this\n\
         test unless the protocol genuinely cannot carry an injected action.",
        undeclared.join("\n  "),
        file!(),
    );
}

#[test]
fn the_baseline_only_shrinks() {
    let servers = load_servers();
    let mut stale = Vec::new();
    let mut vanished = Vec::new();

    for (name, _) in NO_PEER_HANDLE_BASELINE {
        match servers.iter().find(|s| s.name == *name) {
            None => vanished.push(*name),
            Some(server) => {
                if server.registers_peer_handle() {
                    stale.push(*name);
                } else if !server.runs_tcp_accept_loop() {
                    // No longer a TCP accept loop either: still an entry that has stopped
                    // describing anything.
                    stale.push(*name);
                }
            }
        }
    }

    assert!(
        stale.is_empty(),
        "these protocols now register a peer handle (or no longer run a TCP accept loop) but are\n\
         still listed in NO_PEER_HANDLE_BASELINE — delete their lines:\n\n  {}",
        stale.join("\n  "),
    );
    assert!(
        vanished.is_empty(),
        "NO_PEER_HANDLE_BASELINE names protocols that no longer exist under src/server:\n\n  {}",
        vanished.join("\n  "),
    );
}

#[test]
fn every_declared_reason_is_still_true_of_the_source() {
    let servers = load_servers();
    let mut wrong = Vec::new();

    for (name, reason) in NO_PEER_HANDLE_BASELINE {
        let Some(marker) = reason.marker() else {
            continue;
        };
        let Some(server) = servers.iter().find(|s| s.name == *name) else {
            continue; // reported by the shrink test
        };
        if !server.has(marker) {
            wrong.push(format!(
                "{name}: declared {reason:?}, but `{marker}` is gone"
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "a baselined reason has stopped being true of the code it describes. That is the failure\n\
         mode a plain comment has and this table exists to avoid: re-read the protocol and either\n\
         adopt the peer handle or give it the reason that is now correct.\n\n  {}",
        wrong.join("\n  "),
    );
}

/// The count this pass left behind, so a regression is visible as a number and not only as a
/// named protocol.
///
/// Both figures may only move in one direction: more servers with a handle, fewer without.
#[test]
fn peer_handle_coverage_does_not_regress() {
    let servers = load_servers();
    let tcp: Vec<&ServerSource> = servers
        .iter()
        .filter(|s| s.runs_tcp_accept_loop())
        .collect();
    let with_handle = tcp.iter().filter(|s| s.registers_peer_handle()).count();

    // 92 TCP accept-loop servers, 40 of them with a peer handle after the September 2026
    // adoption pass — 32 before it. The floor is what matters; the total is printed for
    // context and is allowed to grow as protocols are added.
    const MIN_WITH_HANDLE: usize = 40;
    assert!(
        with_handle >= MIN_WITH_HANDLE,
        "peer-handle coverage went backwards: {with_handle} of {} TCP accept-loop servers \
         register one, and {MIN_WITH_HANDLE} did at the last measurement. A protocol lost its \
         handle, or the detection stopped seeing it.",
        tcp.len(),
    );
}
