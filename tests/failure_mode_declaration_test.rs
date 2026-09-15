//! A server's behaviour on LLM failure must be **declared**, not described in prose.
//!
//! The root `CLAUDE.md` carries a list of ~20 "deliberately silent" protocols. That list was
//! wrong in both directions when it was audited: NDP logged its own *transmit* failure as
//! `decision=model_silent`, so `grep decision=fail_closed_` found nothing for a real outage;
//! and several BLE profiles were on it by family membership rather than because anyone had
//! decided. A paragraph in a shared document cannot be checked against 140 servers. A
//! `FailureMode` on the metadata can.
//!
//! The distinction is not stylistic. It turns on one question: **is every reply this protocol
//! can send a positive assertion?** `openvpn` is the case to remember — its only pre-TLS server
//! message is `P_CONTROL_HARD_RESET_SERVER_V2`, and sending it *is* admitting the peer, so
//! "fixing the silence" there would convert a backend outage into an authentication bypass.
//! Where the answer is no — HTTP has a 503, RESP has `-LOADING`, Modbus has exception 0x04 —
//! silence merely makes the peer wait out its own timeout, and answering is strictly better.
//! That is why `FailureMode::Answers` is the default: the August 2026 sweep found 64 of 135
//! servers silent when they should have answered.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Protocols that write nothing on LLM failure and have **not yet declared it**.
///
/// This is a shrink-only migration baseline, not an exemption list. Every entry is a protocol
/// the root `CLAUDE.md` names as deliberately silent, whose `actions.rs` was held by another
/// agent when this test was written — adding `.deliberately_silent()` to a file someone else is
/// mid-edit is how `master` gets a half-landed change.
///
/// **Do not add to this list.** A new protocol that writes nothing on failure declares it.
const UNDECLARED_SILENT_BASELINE: &[&str] = &[
    // Held by the `decision=` tagging sweep, which is adding the log-side distinction to
    // exactly these files at the same time.
    "arp",
    "mdns",
    "stun",
    "syslog",
    "udp",
    "bluetooth_ble_battery",
    "bluetooth_ble_heart_rate",
    "bluetooth_ble_keyboard",
    "bluetooth_ble_mouse",
    "bluetooth_ble_remote",
    // Held by the evidence sweep (maturity promotion touches the same builder chain).
    "radius",
    // usb/keyboard and usb/mouse live one directory deeper and the walk below is flat; they
    // are silent for the HID reason (a fabricated report asserts a keypress) and want the
    // declaration when the nested walk exists.
    "usb_keyboard",
    "usb_mouse",
];

fn server_action_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = Path::new("src/server");
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let actions = entry.path().join("actions.rs");
        if actions.is_file() {
            out.push(actions);
        }
    }
    out.sort();
    out
}

fn protocol_name(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Every protocol the docs call deliberately silent must say so in its metadata.
///
/// Source-reading rather than registry-walking, so it holds at the six-protocol CI gate — a
/// registry-walking version of this check sees 6 of 116 and passes by inspecting almost
/// nothing.
#[test]
fn every_deliberately_silent_protocol_declares_it() {
    // The protocols the root CLAUDE.md names. Kept here rather than parsed out of the prose,
    // because parsing the prose is what this test exists to stop being necessary.
    const DOCUMENTED_SILENT: &[&str] = &[
        "arp",
        "bootp",
        "datalink",
        "dhcp",
        "igmp",
        "ipsec",
        "isis",
        "mdns",
        "openvpn",
        "ospf",
        "radius",
        "rip",
        "rtp",
        "stun",
        "syslog",
        "udp",
        "bluetooth_ble_battery",
        "bluetooth_ble_heart_rate",
        "bluetooth_ble_keyboard",
        "bluetooth_ble_mouse",
        "bluetooth_ble_remote",
        "usb_keyboard",
        "usb_mouse",
    ];

    let baseline: BTreeSet<&str> = UNDECLARED_SILENT_BASELINE.iter().copied().collect();
    let files = server_action_files();
    assert!(
        files.len() > 100,
        "only {} server actions.rs files found; this test is not reaching the source tree",
        files.len()
    );

    let mut missing = Vec::new();
    let mut declared = BTreeSet::new();

    for file in &files {
        let name = protocol_name(file);
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        if src.contains(".deliberately_silent()") {
            declared.insert(name.clone());
            continue;
        }
        if DOCUMENTED_SILENT.contains(&name.as_str()) && !baseline.contains(name.as_str()) {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "these protocols are documented as writing nothing on LLM failure but do not declare \
         FailureMode::DeliberatelySilent: {missing:?}\n\
         Add `.deliberately_silent()` to the metadata builder with the reason in a comment, or \
         - if the protocol does in fact answer - correct the documentation instead."
    );

    // The baseline must shrink. An entry that has since been declared is a line to delete, and
    // leaving it makes the next reader think the work is still outstanding.
    let stale: Vec<_> = baseline.iter().filter(|b| declared.contains(**b)).collect();
    assert!(
        stale.is_empty(),
        "these protocols now declare deliberately_silent and must be removed from \
         UNDECLARED_SILENT_BASELINE: {stale:?}"
    );

    assert!(
        !declared.is_empty(),
        "no protocol declares deliberately_silent, so this test is passing by inspecting \
         nothing - exactly the failure it exists to prevent"
    );
}

/// A protocol that declares silence still owes the **log** the distinction the wire cannot
/// carry.
///
/// This is the half that is easy to get wrong, and NDP got it wrong: it logged its own
/// build-and-transmit failure as `decision=model_silent`, so a genuine outage was recorded as
/// the model choosing to say nothing and `grep decision=fail_closed_` found nothing at all.
/// Silence on the wire is not silence in the log.
#[test]
fn a_silent_protocol_still_tags_its_decisions_in_the_log() {
    let mut untagged = Vec::new();

    for file in server_action_files() {
        let Ok(src) = std::fs::read_to_string(&file) else {
            continue;
        };
        if !src.contains(".deliberately_silent()") {
            continue;
        }
        let name = protocol_name(&file);
        let dir = file.parent().expect("actions.rs has a parent");

        // The tag may live in either file; several protocols decide in `mod.rs` and describe
        // in `actions.rs`.
        let in_actions = src.contains("decision=");
        let in_mod = std::fs::read_to_string(dir.join("mod.rs"))
            .map(|m| m.contains("decision="))
            .unwrap_or(false);

        if !in_actions && !in_mod {
            untagged.push(name);
        }
    }

    assert!(
        untagged.is_empty(),
        "these protocols write nothing on the wire on failure and also carry no `decision=` \
         tag, so a backend outage, a model refusal and a model that answered with nothing are \
         indistinguishable in the log as well: {untagged:?}\n\
         `src/server/radius/` is the worked example."
    );
}
