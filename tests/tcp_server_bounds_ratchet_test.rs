//! Ratchet: every TCP accept-loop server declares a read deadline and a connection cap.
//!
//! This reads the **source tree**, not the registry, so it holds at any feature set — including
//! the six-protocol CI gate, where a registry-walking test only ever sees what that build
//! compiled. Every protocol directory under `src/server` whose `mod.rs` opens a TCP accept loop
//! must name a timeout constant *and* go through `crate::server::accept_bounded`, somewhere in
//! that directory (nfs declares both in `guard.rs`, which is the right place for it).
//!
//! # This test could not see two thirds of the tree, and that is the part worth reading
//!
//! Until 16 September 2026 the derivation was `mod_src.contains("TcpListener")`. Most servers
//! here do not write that: they call `crate::server::socket_helpers::create_reusable_tcp_listener`,
//! whose *return type* is a `TcpListener` but whose call site never names one. So this ratchet
//! walked **32** servers while **92** open a TCP accept loop, and the 60 it could not see
//! included `http`, `tcp`, `telnet`, `ssh`, `ldap`, `imap`, `grpc`, `modbus`, `git` and
//! `kubernetes`.
//!
//! **47 of those 60 had neither bound** — no cap and no deadline. A peer that connects and says
//! nothing holds a socket, a task and an `AppState` entry forever, pre-authentication, on a
//! server that will happily accept a hundred more; that is the free denial of service this test
//! exists to prevent, and it was sitting behind the test's own blind spot.
//!
//! The lesson is the recurring one in this repository: **the test counted a token, not the
//! thing.** A source-reading check must match every way the thing is actually written, and a
//! green result over an unmeasured population is worse than no check, because it is trusted.
//!
//! # The baselines are large on purpose
//!
//! Both are **shrink-only**, and they now record real, measured debt rather than an artefact of
//! what the derivation happened to match. Adding a protocol to either is not a fix; it is a
//! statement, with a reason, that the bound cannot be expressed there — and the reason has to be
//! good enough to survive review. Removing one is the work.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test tcp_server_bounds_ratchet -- --test-threads=100

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Protocols that open a TCP accept loop without naming a read deadline.
///
/// Measured 16 September 2026 across all 92, once the derivation stopped missing the servers
/// that bind through `create_reusable_tcp_listener`. The previous value of this list was empty,
/// which was true of the 32 it could see and false of the tree.
///
/// Entries are **leaf names** (`serial`, not `usb/serial`) — see `leaf()`.
///
/// Nothing here makes a deadline impossible. This is work left.
///
/// **Empty as of 22 September 2026, and it may only stay that way.** Every one of the 92 TCP
/// accept loops now names a read deadline. The last five were `nfc`, `ollama`, `openai`, `rtsp`
/// and `xmpp`; the two hyper servers among them (`ollama`, `openai`) took the `peek` +
/// `ConnectionActivity` shape `etcd` and `s3` established rather than a deadline inside hyper's
/// own reads, because hyper keeps polling a connection while a request is being answered and
/// such a deadline fires in the middle of a model round-trip.
const TIMEOUT_BASELINE: &[&str] = &[];

/// Protocols that open a TCP accept loop without a connection cap.
///
/// Measured 16 September 2026 across all 92. The list held 14 then — the ones the old
/// derivation could see — and reached 25 once the derivation stopped missing two thirds of the
/// tree.
///
/// **Empty as of 22 September 2026, and it may only stay that way.** Every one of the 92 TCP
/// accept loops now goes through `crate::server::accept_bounded`. The last ten were `amqp`,
/// `bgp`, `doh`, `dot`, `llmnr`, `mongodb`, `socks5`, `webrtc`, `webrtc_signaling` and
/// `websocket`; five of them answer in a protocol vocabulary of their own (SOCKS5's
/// `NO ACCEPTABLE METHODS`, AMQP's `Connection.Close` 320, BGP's Cease/Out-of-Resources
/// NOTIFICATION, and an HTTP 503 for the two WebSocket-carried ones), and five refuse with a
/// plain close because every message they could send would have to echo something the refused
/// peer never sent — or, for `doh` and `dot`, would be a malformed TLS record rather than a
/// refusal at all.
///
/// Nothing here makes a cap impossible. This is work left.
const CAP_BASELINE: &[&str] = &[];

/// Ways a read is actually bounded in time — **mechanisms, not names**.
///
/// This list used to include `READ_TIMEOUT`, `IDLE_` and `_TIMEOUT`, which match a *constant's
/// name* rather than anything that enforces a deadline. `_TIMEOUT` over-matched badly:
/// `snowflake` was absent from `TIMEOUT_BASELINE` while having no read bound at all, because it
/// declares `CODE_REQUEST_TIMEOUT = "000629"` — a Snowflake error *code*, a string. The protocol
/// was silently exempted by its own error table.
///
/// **A loose list fails in the dangerous direction.** A protocol that has no bound but happens
/// to contain a matching word passes and nobody looks again; a protocol that has one but spells
/// it unusually gets flagged, which costs a reader five minutes. Prefer the second. Re-measured
/// across all 92 when this was tightened: it flags nothing that is not already on the baseline,
/// so the loose version was buying no coverage at all — only the snowflake-shaped hole.
///
/// `timeout(` is bare rather than `tokio::time::timeout(` because most of the tree imports it.
/// `Instant::now() +` catches the hand-rolled deadline loop that `hls`, `ipp` and the
/// `accept_bounded` helpers use.
const TIMEOUT_TOKENS: &[&str] = &[
    "timeout(",
    "IdleTimeoutReader",
    "watch_idle",
    "Instant::now() +",
    "sleep_until",
    "set_read_timeout",
];

fn server_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server")
}

/// Every protocol directory under `src/server` whose `mod.rs` opens a TCP accept loop, with the
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
        // Both ways a server opens a TCP accept loop. Matching only the first is what hid 60
        // of 92 protocols from this test: `create_reusable_tcp_listener` returns a
        // `TcpListener` without its caller ever writing the type.
        if !mod_src.contains("TcpListener") && !mod_src.contains("create_reusable_tcp_listener") {
            continue;
        }
        let name = entry
            .strip_prefix(&root)
            .expect("under src/server")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");

        let mut combined = String::new();
        // A protocol's own directory, plus — for one that nests a level deeper — its family
        // directory. `nfs` declares both its bounds in `nfs/guard.rs`, a sibling module, and
        // this walk has always picked that up because it is in the same directory. The USB
        // family is the same arrangement one level out: all six protocols hand their socket to
        // `usb/guard.rs`, which is the only thing in the process that reads from it, so
        // `usb/keyboard` declaring the bound in its own `mod.rs` would mean six copies of one
        // number. Without this, the check measured a token's *location* rather than whether a
        // read is bounded — the same class of blind spot as the `TcpListener` one above.
        for dir in [Some(entry.as_path()), family_dir(&root, &entry)]
            .into_iter()
            .flatten()
        {
            for file in std::fs::read_dir(dir).expect("read protocol dir").flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        combined.push_str(&text);
                    }
                }
            }
        }
        found.push((name, combined));
    }

    found.sort();
    found
}

/// The family directory of a protocol that nests one level deeper (`src/server/usb` for
/// `src/server/usb/keyboard`), or `None` for a top-level protocol.
///
/// Only the family's own `.rs` files are read, never a sibling protocol's, so `usb/keyboard`
/// cannot be exempted by something `usb/msc` declares.
fn family_dir<'a>(root: &Path, entry: &'a Path) -> Option<&'a Path> {
    let parent = entry.parent()?;
    (parent != root).then_some(parent)
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
        if !bounds_a_read_in_time(&source) {
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
fn the_protocols_these_sweeps_covered_have_both_bounds() {
    // Named explicitly rather than derived, because the point of the list is that each entry
    // was measured by hand and has a wire-driven test of its own.
    //
    // The first eighteen are the original sweep: the 18 of 32 that referenced no read or idle
    // timeout at all. The twenty after them are the two connection-cap batches of
    // September 2026 — each has a `tests/server/<p>/connection_bounds_test.rs` that fills the
    // cap from the wire, asserts how the next peer is refused, and asserts that closing one
    // admitted connection frees exactly one slot.
    //
    // A protocol belongs here only when **both** bounds are real, which is why this list is
    // shorter than "everything that has been touched": the assertions below are the pin, so an
    // entry that has a cap and no deadline would fail rather than record a gap. Both batches
    // were checked against these three conditions before being added.
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
        // Connection-cap batch one (September 2026).
        "finger",
        "gopher",
        "hls",
        "ident",
        "ipp",
        "mqtt",
        "proxy",
        "smtp",
        "torrent_tracker",
        "whois",
        // Connection-cap batch two, which emptied CAP_BASELINE.
        "amqp",
        "bgp",
        "doh",
        "dot",
        "llmnr",
        "mongodb",
        "socks5",
        "webrtc",
        "webrtc_signaling",
        "websocket",
        // New protocols, born with both bounds (Programme 4).
        "dict",
        "gemini",
        "beanstalkd",
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

/// Does this protocol's source actually bound a read in time?
///
/// A direct mechanism from [`TIMEOUT_TOKENS`], **or** the `select!`-arm idiom: a
/// `tokio::time::sleep` racing the read, with a timeout or deadline binding to sleep for.
///
/// That second form is why this is a function rather than a flat list. `nats` writes
///
/// ```ignore
/// let read_deadline = if spoke_once { IDLE_BETWEEN_FRAMES_TIMEOUT } else { FIRST_FRAME_READ_TIMEOUT };
/// tokio::select! {
///     n = read_half.read(&mut buf) => n?,
///     _ = tokio::time::sleep(read_deadline) => { /* close */ }
/// }
/// ```
///
/// which is a real deadline on a real read and matches none of the direct tokens. Flagging it
/// was a false positive — in the safe direction, but still wrong.
///
/// **The conjunct is what keeps this from sliding back into name-matching.** `time::sleep(`
/// alone would pass any protocol with a retry backoff or a keepalive timer; a `_TIMEOUT`
/// identifier alone is what silently exempted `snowflake`, whose only match was
/// `CODE_REQUEST_TIMEOUT = "000629"` — a Snowflake error code, a string, with no timing code
/// anywhere in the directory. Requiring **both** a real sleep call and a deadline to sleep for
/// still catches that: a protocol that bounds nothing has no `sleep` to pair with the name.
fn bounds_a_read_in_time(source: &str) -> bool {
    if TIMEOUT_TOKENS.iter().any(|token| source.contains(token)) {
        return true;
    }
    let sleeps = source.contains("time::sleep(");
    let has_deadline = source.contains("_TIMEOUT") || source.contains("_deadline");
    sleeps && has_deadline
}
