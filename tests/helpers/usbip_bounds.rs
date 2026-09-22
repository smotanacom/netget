//! The read deadlines of the USB/IP servers, driven from a raw socket.
//!
//! Six servers here are one shape — `usb/keyboard`, `usb/mouse`, `usb/msc`, `usb/serial`,
//! `usb/smartcard` and `usb/fido2` all hand their accepted socket to
//! `crate::server::usb::guard::run_guarded_usbip`, which is the only thing in the process that
//! reads from it. So the bounds live there once and are asserted here once, and each protocol's
//! `connection_bounds_test.rs` supplies its registry name and its own argument.
//!
//! # The two bounds are different claims, and the numbers are far apart on purpose
//!
//! **A peer that connects and says nothing must be let go of.** Nothing else will close that
//! socket: it holds a task, an `AppState` row and one of `MAX_USBIP_CONNECTIONS` (32) slots,
//! and USB/IP authenticates nothing, so the server has to give up first. The default is **30
//! seconds**, which is short by the standards of this codebase and is the interesting part of
//! this sweep: `tcp`, `telnet`, `ldap`, `whois` and `redis` all ended at 300 because their peer
//! is frequently NetGet's own client parked at `[ send message ]` waiting for a person. **That
//! peer cannot exist here** — NetGet has no USB/IP client, `src/protocol/dual.rs` deliberately
//! pairs no `USB-*` server with the generic USB client so `[ + client ]` is the disabled
//! button, and no event can park in front of the first message because the attach event hangs
//! off the first admitted `OP_REQ_IMPORT`. A silent peer here is waiting for nobody.
//!
//! **An attached host that is merely quiet must not be.** USB/IP has no keepalive and nothing
//! obliges an imported device's host to say anything — a mass-storage device a host has
//! attached and not mounted issues no URB at all — so the idle default is **1800 seconds** and
//! exists to reap a peer that is gone rather than to police one that is present. Closing those
//! would be the live-transfer eviction the project `CLAUDE.md` records TFTP learning about.
//!
//! # What each check fails without
//!
//! | check | remove | symptom |
//! |---|---|---|
//! | [`silent_peer_is_closed_at_the_first_message_bound`] | the deadline on the first `read_exact` in `relay_one_message` | hangs until its own window expires; the peer holds the slot forever |
//! | [`an_admitted_session_is_governed_by_the_idle_bound`] | the `admitted_one` switch in `run_guarded_usbip` | the connection lives to the 60-second first-message bound this test sets, and the assertion that it did not fires |
//! | [`an_admitted_session_outlives_the_first_message_default`] | nothing — it is the regression for a lazy fix | one bound for both, set to 30s, would close an attached host at 30 seconds |
//!
//! The values the first two drive are overrides, not the defaults: 30 and 1800 seconds are
//! argued where they are declared, and a test that waited 1800 seconds out would be the slowest
//! thing in the tree. What is asserted here is that each declared parameter is read and applied
//! to the read it names. The third does wait, because its claim is about a number larger than
//! 30 and no cheaper evidence for that exists.
//!
//! No mock backend: the LLM endpoint is a dead port, and the servers are created with an empty
//! instruction so nothing consults a model at all. These are assertions about deadlines.
//! Loopback only.

// Every test binary compiles `helpers`; only the USB suites use this one.
#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::common::E2EResult;

/// The first-message bound the first check drives, as `first_byte_timeout_secs`.
pub const SHORT_FIRST_MESSAGE: Duration = Duration::from_secs(6);

/// The idle bound the second check drives, as `idle_timeout_secs`.
pub const SHORT_IDLE: Duration = Duration::from_secs(3);

/// The first-message bound the second check sets, deliberately far longer than [`SHORT_IDLE`]
/// and the wrong way round: if the screen kept using it after admitting a message, that
/// connection would live a minute and the test would say so.
const LONG_FIRST_MESSAGE_SECS: u64 = 60;

/// How long the third check holds an admitted-then-quiet peer against the *defaults*.
///
/// Past the 30-second first-message default by a margin that survives a 100-thread run, and far
/// inside the 1800-second idle one. The claim is about a number larger than 30, so the wait has
/// to be larger than 30 too.
pub const PAST_THE_FIRST_MESSAGE_DEFAULT: Duration = Duration::from_secs(45);

/// How long a close is given to arrive after the deadline it is attributed to.
///
/// Generous: `run_guarded_usbip` gives the crate-side session and the reply relay
/// `SESSION_SHUTDOWN_GRACE` each to wind up before the socket goes, and a `--test-threads=100`
/// run adds scheduling delay on top. What is asserted is that the read ends *at all*, and that
/// it ends on the bound under test rather than on the other one.
const CLOSE_SLACK: Duration = Duration::from_secs(45);

/// `OP_REQ_DEVLIST`: version 0x0111, command 0x8005, status 0. Eight bytes, and the whole of
/// what `usbip list -r` sends. Written by hand rather than through
/// [`crate::helpers::usbip_client::UsbIpClient`] because these checks need the socket back
/// afterwards to watch it close.
const OP_REQ_DEVLIST: [u8; 8] = [0x01, 0x11, 0x80, 0x05, 0x00, 0x00, 0x00, 0x00];

/// `OP_REP_DEVLIST`, the reply code in the answer's second halfword.
const OP_REP_DEVLIST: u16 = 0x0005;

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_port(state: &AppState, id: ServerId) -> E2EResult<u16> {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return Ok(addr.port());
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    Err(format!("USB server #{} never bound a port", id.as_u32()).into())
}

/// Start one of the USB servers, model-free.
///
/// `instruction: Some(String::new())` rather than `None`: `ServerForm::create` substitutes a
/// default instruction for `None`, which makes every event consult the model.
async fn start(
    protocol: &str,
    startup_params: Option<serde_json::Value>,
) -> E2EResult<(AppState, u16)> {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: protocol.to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .map_err(|e| format!("could not create a {protocol} server: {e}"))?;
    let port = wait_for_port(&state, server_id).await?;
    Ok((state, port))
}

/// Send `OP_REQ_DEVLIST` and read the answer's header, leaving the socket open.
///
/// This is the cheapest thing that counts as "the peer has spoken USB/IP": the screen admits
/// and relays it, so the connection switches to the idle bound — and, deliberately, it does
/// *not* fire the attach event, which follows `OP_REQ_IMPORT` alone.
async fn devlist(peer: &mut TcpStream, protocol: &str) -> E2EResult<()> {
    peer.write_all(&OP_REQ_DEVLIST).await?;
    let mut header = [0u8; 12];
    tokio::time::timeout(Duration::from_secs(20), peer.read_exact(&mut header))
        .await
        .map_err(|_| format!("the {protocol} server did not answer OP_REQ_DEVLIST within 20s"))??;
    let reply = u16::from_be_bytes([header[2], header[3]]);
    if reply != OP_REP_DEVLIST {
        return Err(format!(
            "expected OP_REP_DEVLIST from the {protocol} server, got reply code {reply:#06x}; \
             what follows is not the post-admission state these checks are about"
        )
        .into());
    }
    Ok(())
}

/// A peer that connects and sends no USB/IP message is closed at `first_byte_timeout_secs`.
pub async fn silent_peer_is_closed_at_the_first_message_bound(protocol: &str) -> E2EResult<()> {
    let (_state, port) = start(
        protocol,
        Some(serde_json::json!({
            "first_byte_timeout_secs": SHORT_FIRST_MESSAGE.as_secs(),
        })),
    )
    .await?;

    let mut peer = TcpStream::connect(("127.0.0.1", port)).await?;
    let started = std::time::Instant::now();
    let mut sink = Vec::new();
    let read = tokio::time::timeout(
        SHORT_FIRST_MESSAGE + CLOSE_SLACK,
        peer.read_to_end(&mut sink),
    )
    .await;
    let elapsed = started.elapsed();

    if read.is_err() {
        return Err(format!(
            "a peer that connected to the {protocol} server and sent no USB/IP message was still \
             holding the socket, the connection task and one of the 32 connection slots after \
             {}s — either the first-message deadline is not applied at all, or \
             `first_byte_timeout_secs` was declared and never read and the 30-second default is \
             still in force",
            elapsed.as_secs()
        )
        .into());
    }
    read.unwrap()?;
    assert!(
        sink.is_empty(),
        "the {protocol} server wrote {} bytes to a peer that had asked for nothing; USB/IP is \
         client-speaks-first and every server message is a positive assertion about a device",
        sink.len()
    );
    assert!(
        elapsed >= SHORT_FIRST_MESSAGE / 2,
        "closed after only {}ms — that is not the declared {}s bound, it is something else \
         tearing the connection down, and this check would then pass without the bound existing",
        elapsed.as_millis(),
        SHORT_FIRST_MESSAGE.as_secs()
    );
    Ok(())
}

/// Once a message has been admitted, `idle_timeout_secs` governs rather than the first-message
/// bound.
pub async fn an_admitted_session_is_governed_by_the_idle_bound(protocol: &str) -> E2EResult<()> {
    let (_state, port) = start(
        protocol,
        Some(serde_json::json!({
            "first_byte_timeout_secs": LONG_FIRST_MESSAGE_SECS,
            "idle_timeout_secs": SHORT_IDLE.as_secs(),
        })),
    )
    .await?;

    let mut peer = TcpStream::connect(("127.0.0.1", port)).await?;
    devlist(&mut peer, protocol).await?;

    let started = std::time::Instant::now();
    let mut sink = Vec::new();
    let read = tokio::time::timeout(CLOSE_SLACK, peer.read_to_end(&mut sink)).await;
    let elapsed = started.elapsed();

    if read.is_err() {
        return Err(format!(
            "a {protocol} session that had spoken USB/IP and then went quiet was never closed — \
             `idle_timeout_secs` was declared and is not being read"
        )
        .into());
    }
    read.unwrap()?;
    assert!(
        elapsed >= SHORT_IDLE / 2,
        "closed after only {}ms, which is below the declared {}s idle bound",
        elapsed.as_millis(),
        SHORT_IDLE.as_secs()
    );
    assert!(
        elapsed < Duration::from_secs(LONG_FIRST_MESSAGE_SECS - 20),
        "closed after {}s, which is the {LONG_FIRST_MESSAGE_SECS}-second first-message bound \
         rather than the {}s idle one — the screen never switched bounds",
        elapsed.as_secs(),
        SHORT_IDLE.as_secs()
    );
    Ok(())
}

/// With no startup parameters at all, a peer that has spoken USB/IP once survives well past the
/// 30-second first-message default.
///
/// Deliberately slow: proving a peer lives *past* a 30-second number means waiting past it.
pub async fn an_admitted_session_outlives_the_first_message_default(
    protocol: &str,
) -> E2EResult<()> {
    let (_state, port) = start(protocol, None).await?;

    let mut peer = TcpStream::connect(("127.0.0.1", port)).await?;
    devlist(&mut peer, protocol).await?;

    let mut sink = Vec::new();
    match tokio::time::timeout(PAST_THE_FIRST_MESSAGE_DEFAULT, peer.read_to_end(&mut sink)).await {
        // Still open with nothing to read: the passing case.
        Err(_) => Ok(()),
        Ok(Ok(0)) => Err(format!(
            "the {protocol} server hung up on an attached-and-quiet peer within {}s. USB/IP has \
             no keepalive and nothing obliges a host that has imported a device to say anything \
             — a drive nobody has mounted issues no URB at all — so the idle bound must be far \
             longer than the first-message one. See DEFAULT_IDLE_TIMEOUT in \
             src/server/usb/guard.rs",
            PAST_THE_FIRST_MESSAGE_DEFAULT.as_secs()
        )
        .into()),
        Ok(Ok(n)) => Err(format!(
            "the {protocol} server wrote {n} unsolicited bytes to a peer that had asked for \
             nothing further"
        )
        .into()),
        Ok(Err(e)) => Err(format!(
            "read failed on a {protocol} connection that should still be open: {e}"
        )
        .into()),
    }
}
