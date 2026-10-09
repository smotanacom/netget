//! Peer text is made terminal-safe where it enters the operator's view.
//!
//! `src/logging/emit.rs` fans a message out to `netget.log` and the status channel the TUI
//! and the headless runner print; `src/tui/cards.rs::payload_summary` is the one row a
//! request gets on a card. Both carry a peer's own bytes, and neither filtered ESC, C1 or
//! BEL — so an unauthenticated telnet line could write the operator's clipboard (OSC 52),
//! retitle the window, clear the screen to hide itself, or send a DSR query whose answer
//! the terminal types back into the chat box as keystrokes. The TUI frame half of this is
//! `tests/dashboard_frame_test.rs::peer_escape_sequences_are_not_painted_as_terminal_cells`.

use netget::logging::emit::{Level, Log, Sink};
use netget::tui::cards::payload_summary;

const HOSTILE: &str = "user\u{1b}]52;c;QUJD\u{7}\u{9b}31m\u{1b}[2J\u{1b}[6n\u{85}name";

fn has_controls_other_than_newline_and_tab(s: &str) -> bool {
    s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
}

#[test]
fn a_status_line_reaches_the_channel_with_no_escape_sequence_in_it() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    Log::new(Some(&tx)).with_payloads(true).emit(
        Level::Warn,
        Sink::Both,
        format!("SSH auth for '{HOSTILE}': decision=model_reject"),
    );
    let line = rx.try_recv().expect("the TUI copy");
    assert!(line.starts_with("[WARN] "), "{line:?}");
    assert!(
        !has_controls_other_than_newline_and_tab(&line),
        "escape bytes survived into the status line: {line:?}"
    );
    assert!(
        line.contains("user") && line.contains("name") && line.contains("decision=model_reject"),
        "the text around them must survive: {line:?}"
    );
}

#[test]
fn a_request_row_is_one_line_of_plain_text() {
    let row = payload_summary(&serde_json::json!({"message": HOSTILE, "connection_id": "c"}));
    assert!(!row.chars().any(char::is_control), "{row:?}");
    assert!(row.starts_with("user") && row.ends_with("name"), "{row:?}");

    // What it did before is kept: CR dropped, LF shown as ⏎, a request line as one.
    assert_eq!(
        payload_summary(&serde_json::json!({"data": "a\r\nb"})),
        "a⏎b"
    );
    assert_eq!(
        payload_summary(&serde_json::json!({"method": "GET", "path": "/x\u{1b}[31m"})),
        "GET /x [31m"
    );
}
