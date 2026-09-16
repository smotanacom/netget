//! Every USB/IP server caps its accept loop, and none of them pays the model on `accept()`.
//!
//! # Why a source-reading ratchet rather than six behavioural tests
//!
//! `tests/server/usb_keyboard/{connection_cap,attach_on_import}_test.rs` prove both properties
//! from the wire, but only for the one protocol whose feature the run happens to enable. The
//! blocking CI job compiles six protocols out of ~116 and none of them is a USB one, so a
//! behavioural test is the right proof and the wrong *coverage*: the other five servers are
//! copies of each other, which is exactly how all six came to share the same two defects.
//!
//! This reads the source instead, so it holds at any feature set — the shape
//! `tests/detached_task_drift_test.rs` and `tests/event_emit_sites_test.rs` already use here.
//!
//! # The two properties
//!
//! **A capped accept loop.** A bare `listener.accept()` in an accept loop is an unbounded
//! server: USB/IP authenticates nothing and each admitted connection is a whole emulated device.
//! `src/server/accept_bounded.rs` is the shared answer and every USB server must go through it.
//!
//! **The attach event on `OP_REQ_IMPORT`, not on `accept()`.** A TCP handshake is not an
//! attachment. `run_guarded_usbip` fires a `oneshot` when the screen admits an `OP_REQ_IMPORT`,
//! and each server hangs its attach event on that; passing `None` would silently restore the
//! old behaviour, since nothing else would ever fire.
//!
//! Both checks are deliberately crude — they look for the call, not for its correctness. A
//! source scan cannot tell a well-placed `accept_bounded` from a badly-placed one. What it can
//! tell is that a **new** USB protocol, or a revert of one of these six, did not quietly go back
//! to the unbounded, model-spending shape; and that is the failure mode that actually happened.

use std::path::{Path, PathBuf};

/// The six USB/IP servers. Named explicitly rather than globbed so that adding a seventh is a
/// deliberate edit here — which is the moment to ask whether it has both properties.
const USB_SERVERS: &[&str] = &["keyboard", "mouse", "serial", "msc", "fido2", "smartcard"];

fn server_mod(protocol: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/server/usb")
        .join(protocol)
        .join("mod.rs")
}

fn read(protocol: &str) -> String {
    let path = server_mod(protocol);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn every_usb_server_caps_its_accept_loop() {
    let mut offenders = Vec::new();

    for protocol in USB_SERVERS {
        let source = read(protocol);

        if !source.contains("accept_bounded(") {
            offenders.push(format!(
                "usb/{protocol}: its accept loop does not call `accept_bounded`, so it admits \
                 unlimited concurrent connections"
            ));
        }
        if !source.contains("MAX_USBIP_CONNECTIONS") {
            offenders.push(format!(
                "usb/{protocol}: no `MAX_USBIP_CONNECTIONS` — the cap must be the family's \
                 shared one in `src/server/usb/guard.rs`, not a local number"
            ));
        }
        // The bare form is what every one of these used to be, and what a copy-paste from an
        // older protocol would reintroduce.
        if source.contains("listener.accept().await") {
            offenders.push(format!(
                "usb/{protocol}: still contains a bare `listener.accept().await`. Every accepted \
                 connection is an emulated USB device on a protocol with no authentication; the \
                 accept loop must go through `accept_bounded`"
            ));
        }
        // A permit dropped before the connection task starts releases the slot immediately and
        // un-caps the server without changing a single visible behaviour.
        if !source.contains("let _permit = permit;") {
            offenders.push(format!(
                "usb/{protocol}: the `ConnectionPermit` is not moved into the connection task \
                 (`let _permit = permit;`). Dropping it early releases the slot while the device \
                 is still exported, which silently un-caps the server"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "USB/IP accept loops must be bounded:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn no_usb_server_pays_the_model_on_a_bare_tcp_accept() {
    let mut offenders = Vec::new();

    for protocol in USB_SERVERS {
        let source = read(protocol);

        if !source.contains("run_guarded_usbip") {
            offenders.push(format!(
                "usb/{protocol}: does not run the `usbip` crate behind `run_guarded_usbip`, so \
                 neither the pre-auth allocation screen nor the import signal applies"
            ));
            continue;
        }
        if !source.contains("Some(import_tx)") {
            offenders.push(format!(
                "usb/{protocol}: passes no import sender to `run_guarded_usbip`. Without it the \
                 attach event has nothing to hang off, and the only place left to raise it is \
                 the accept — which is a model call bought by a TCP handshake on a protocol that \
                 authenticates nothing"
            ));
        }
        if !source.contains("if import_pending") {
            offenders.push(format!(
                "usb/{protocol}: has an import sender but no `if import_pending` guard on the \
                 receiving `select!` arm. A `oneshot::Receiver` polled after it has resolved \
                 panics, and the panic would be swallowed by the spawned connection task"
            ));
        }
        if !source.contains("if imported") {
            offenders.push(format!(
                "usb/{protocol}: raises its detach event unconditionally. Detach is the other \
                 half of an attachment — a peer that opened a socket and closed it again \
                 detached nothing, and must cost nothing"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "the USB attach event must follow OP_REQ_IMPORT, not accept():\n  {}",
        offenders.join("\n  ")
    );
}
