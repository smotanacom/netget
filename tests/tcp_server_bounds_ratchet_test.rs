//! Ratchet: every TCP accept-loop server declares a read deadline and a connection cap.
//!
//! This reads the **source tree**, not the registry, so it holds at any feature set — including
//! the six-protocol CI gate, where a registry-walking test only ever sees what that build
//! compiled. Every `src/server/**/mod.rs` that binds a `TcpListener` must name a timeout
//! constant *and* go through `crate::server::accept_bounded`, somewhere in its protocol
//! directory (nfs declares both in `guard.rs`, which is the right place for it).
//!
//! Measured on 15 September 2026, before the sweep this test guards: of 32 servers with a
//! `TcpListener` accept loop, **18 referenced no read or idle timeout at all**, and across the
//! whole tree exactly **2** had any connection cap. A peer that connects and says nothing holds
//! a socket, a task and an `AppState` entry forever, pre-authentication, on a server that will
//! happily accept a hundred more.
//!
//! Both baselines are **shrink-only**. Adding a protocol to either is not a fix; it is a
//! statement, with a reason, that the bound cannot be expressed there — and the reason has to
//! be good enough to survive review.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test tcp_server_bounds_ratchet -- --test-threads=100

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Protocols allowed to bind a `TcpListener` without naming a read deadline.
///
/// Empty. Every one of the 32 has one.
const TIMEOUT_BASELINE: &[&str] = &[];

/// Protocols allowed to bind a `TcpListener` without a connection cap.
///
/// These reached the sweep with read deadlines already in place from earlier work and were out
/// of its stated boundary, so the cap has not been applied to them yet. Nothing about them
/// makes a cap impossible — this list is work left, not a design decision, and it may only
/// shrink.
const CAP_BASELINE: &[&str] = &[
    "amqp",
    "doh",
    "dot",
    "finger",
    "gopher",
    "ident",
    "llmnr",
    "mongodb",
    "mqtt",
    "proxy",
    "torrent_tracker",
    "webrtc",
    "webrtc_signaling",
    "whois",
];

/// Anything that reads as "this read is bounded in time".
const TIMEOUT_TOKENS: &[&str] = &[
    "READ_TIMEOUT",
    "IDLE_",
    "_TIMEOUT",
    "tokio::time::timeout(",
    "IdleTimeoutReader",
    "watch_idle",
];

fn server_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server")
}

/// Every protocol directory under `src/server` whose `mod.rs` binds a `TcpListener`, with the
/// whole directory's Rust source concatenated — a protocol may put its bounds in a sibling
/// module, and `nfs` does exactly that.
fn tcp_servers() -> Vec<(String, String)> {
    let root = server_root();
    let mut found = Vec::new();

    for entry in walk_dirs(&root) {
        let mod_rs = entry.join("mod.rs");
        let Ok(mod_src) = std::fs::read_to_string(&mod_rs) else {
            continue;
        };
        if !mod_src.contains("TcpListener") {
            continue;
        }
        let name = entry
            .strip_prefix(&root)
            .expect("under src/server")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");

        let mut combined = String::new();
        for file in std::fs::read_dir(&entry)
            .expect("read protocol dir")
            .flatten()
        {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    combined.push_str(&text);
                }
            }
        }
        found.push((name, combined));
    }

    found.sort();
    found
}

/// `src/server/<p>` and `src/server/<family>/<p>` — the USB and Bluetooth families nest one
/// level deeper.
fn walk_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root).expect("read src/server").flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        dirs.push(path.clone());
        for nested in std::fs::read_dir(&path).expect("read nested").flatten() {
            let nested = nested.path();
            if nested.is_dir() {
                dirs.push(nested);
            }
        }
    }
    dirs
}

/// The protocol name a baseline entry uses: the last path segment, so `usb/serial` is `serial`
/// and a top-level `redis` is `redis`.
fn leaf(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

#[test]
fn every_tcp_accept_loop_declares_a_read_deadline() {
    let allowed: BTreeSet<&str> = TIMEOUT_BASELINE.iter().copied().collect();
    let mut missing = Vec::new();

    for (name, source) in tcp_servers() {
        if allowed.contains(leaf(&name)) {
            continue;
        }
        if !TIMEOUT_TOKENS.iter().any(|token| source.contains(token)) {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "these TCP servers bind a listener and never bound a read in time, so a peer that \
         connects and says nothing holds a socket, a task and an AppState entry forever:\n  \
         {}\n\nGive each the pair `src/server/whois/mod.rs` established — one bound on the \
         silence of a peer that has produced nothing, a longer one on a peer already in a \
         session — with the value argued per protocol in the comment beside it. Wrap the \
         read() and nothing else, so a model round-trip and a `manual` rule parking an event \
         for a human sit outside the deadline by construction.",
        missing.join("\n  ")
    );
}

#[test]
fn every_tcp_accept_loop_goes_through_the_shared_connection_cap() {
    let allowed: BTreeSet<&str> = CAP_BASELINE.iter().copied().collect();
    let mut seen_allowed = BTreeSet::new();
    let mut missing = Vec::new();

    for (name, source) in tcp_servers() {
        let has_cap = source.contains("accept_bounded");
        if allowed.contains(leaf(&name)) {
            if has_cap {
                // It grew a cap: the baseline entry is stale and must go.
                seen_allowed.insert(leaf(&name).to_string());
            }
            continue;
        }
        if !has_cap {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "these TCP servers accept without limit, so a read deadline alone still lets an \
         attacker hold `deadline x rate` connections at once:\n  {}\n\nCall \
         `crate::server::accept_bounded::accept_bounded` in place of `listener.accept()`, \
         declare a MAX_CONNECTIONS and a CONNECTION_CAP_REFUSAL in the protocol's own mod.rs, \
         and move the returned permit into the connection task.",
        missing.join("\n  ")
    );

    assert!(
        seen_allowed.is_empty(),
        "these protocols are on CAP_BASELINE but now do call accept_bounded — remove them from \
         the list, which may only shrink: {:?}",
        seen_allowed
    );
}

#[test]
fn the_eighteen_this_sweep_covered_have_both_bounds() {
    // Named explicitly rather than derived, because the point of the list is that it was
    // measured: these are the 18 of 32 that referenced no read or idle timeout at all.
    const SWEPT: &[&str] = &[
        "cassandra",
        "db2",
        "etcd",
        "kafka",
        "m3ua",
        "mcp",
        "memcached",
        "mssql",
        "mysql",
        "nfs",
        "postgresql",
        "redis",
        "smb",
        "svn",
        "tls",
        "tor_relay",
        "torrent_peer",
        "zookeeper",
    ];

    let servers = tcp_servers();
    for protocol in SWEPT {
        let (_, source) = servers
            .iter()
            .find(|(name, _)| leaf(name) == *protocol)
            .unwrap_or_else(|| panic!("{protocol} no longer binds a TcpListener in its mod.rs"));

        assert!(
            TIMEOUT_TOKENS.iter().any(|token| source.contains(token)),
            "{protocol} lost its read deadline"
        );
        assert!(
            source.contains("accept_bounded"),
            "{protocol} lost its connection cap"
        );
        assert!(
            source.contains("MAX_CONNECTIONS") || source.contains("MAX_CONCURRENT_CONNECTIONS"),
            "{protocol} must declare its own cap constant, so the number is documented where \
             the protocol is rather than inherited invisibly"
        );
    }
}
