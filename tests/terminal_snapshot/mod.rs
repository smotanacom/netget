//! Terminal snapshot tests for NetGet TUI
//!
//! These tests spawn the actual NetGet binary in a virtual PTY, send keystrokes, and capture
//! terminal snapshots for verification.
//!
//! # The snapshots in `snapshots/` are STALE and must be regenerated
//!
//! They were captured from the **rolling TUI** (`src/cli/rolling_tui.rs`, now behind
//! `--legacy-tui`) — the NETGET block-letter banner and a `Model:… | Log:… |` footer. The
//! default TUI is the ratatui dashboard (`src/tui/`), which renders a chat pane, an INSTANCES
//! rail and a different footer. Every test here therefore fails on content, and will until the
//! snapshots are regenerated against the dashboard:
//!
//! ```bash
//! rm -f tests/terminal_snapshot/snapshots/*.actual.snap.md
//! ./cargo-isolated.sh test --no-default-features --features tcp,terminal-snapshot \
//!     --test terminal_snapshot -- --test-threads=4
//! # review each .actual.snap.md, then:
//! for f in tests/terminal_snapshot/snapshots/*.actual.snap.md; do
//!     mv "$f" "${f%.actual.snap.md}.snap.md"
//! done
//! ```
//!
//! **Review the diffs before promoting them** — that is the whole point of a snapshot, and
//! these went a full UI generation without anyone looking.
//!
//! # Three things were wrong with the harness itself, and are fixed
//!
//! 1. **The PTY had no window size**, so the slave reported 0x0 and the dashboard rendered a
//!    0x0 frame: the child entered the alternate screen, cleared, and painted nothing at all.
//!    Every capture was blank, which made the content staleness above invisible behind a more
//!    confusing symptom. `spawn_netget_with_args` now sets the size before spawning.
//! 2. **The capture read on a fixed timer** and stopped as soon as it had any bytes, so it
//!    routinely fired before the first frame under load. It now waits for the output to go
//!    quiet, bounded.
//! 3. **The snapshots contained the configured model name**, so they could never match on a
//!    machine whose `~/.netget` names a different one. `normalize_screen` replaces it.

use nix::libc;
use std::io::{Read, Write};
use std::process::Child;
use std::time::Duration;
use vt100::Parser;

#[path = "../snapshot_util.rs"]
mod snapshot_util;

const TERMINAL_WIDTH: u16 = 80;
const TERMINAL_HEIGHT: u16 = 24;
const SNAPSHOT_DIR: &str = "tests/terminal_snapshot/snapshots";

/// Owns a `netget` this file spawned and guarantees it is terminated and reaped.
///
/// Every test here used to bind the child to `let _child` and return. `Child`'s `Drop` neither
/// kills nor waits, so each of the seventeen tests left a live `netget` behind — and the project
/// rule is that netget processes are never killed, so nothing collected them afterwards either.
/// `clippy::zombie_processes` flags this, but only at `--all-features`, which CI cannot build.
///
/// Killing here is safe and is not the process the rule protects: this guard holds the exact pid
/// this test spawned, into a PTY it created, and never touches any other process.
struct NetGetChild(Option<Child>);

impl Drop for NetGetChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Already exited (Ctrl-C, PTY hangup) is the common case; kill() then errors
            // harmlessly. wait() is what actually reaps it.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Helper to spawn NetGet in a PTY and return the PTY handle and child process
fn spawn_netget() -> (pty_process::blocking::Pty, NetGetChild) {
    spawn_netget_with_args(&[])
}

/// Helper to spawn NetGet with arguments in a PTY
fn spawn_netget_with_args(args: &[&str]) -> (pty_process::blocking::Pty, NetGetChild) {
    // Use cargo's env variable to get the actual binary path
    let binary_path = env!("CARGO_BIN_EXE_netget");

    // Create a PTY using openpty (simpler than posix_openpt + manual slave open)
    use nix::pty::openpty;

    // Open a new PTY (returns both master and slave as OwnedFds)
    let pty_result = openpty(None, None).expect("Failed to open PTY");

    // openpty already returns OwnedFds, use them directly
    let master_owned = pty_result.master;
    let slave_owned = pty_result.slave;

    // Create PTY from file descriptor
    let pty = unsafe { pty_process::blocking::Pty::from_fd(master_owned) };
    let pts = unsafe { pty_process::blocking::Pts::from_fd(slave_owned) };

    // Give the PTY a window size before spawning.
    //
    // Without this the slave reports 0x0, and the ratatui dashboard renders a 0x0 frame: the
    // child enters the alternate screen, clears, and then paints nothing at all, forever. Every
    // test here captured a blank screen for that reason. The note that used to sit here said
    // NetGet "handles this gracefully by defaulting to 80x24 in rolling_tui.rs" — that is the
    // *legacy* TUI, kept behind `--legacy-tui`; the default is `src/tui/`, which had no such
    // fallback.
    pty.resize(pty_process::Size::new(TERMINAL_HEIGHT, TERMINAL_WIDTH))
        .expect("set PTY window size");

    let mut cmd = pty_process::blocking::Command::new(binary_path);
    for arg in args {
        cmd = cmd.arg(arg);
    }
    let child = cmd.spawn(pts).expect("Failed to spawn netget in PTY");

    (pty, NetGetChild(Some(child)))
}

/// Replace anything in a captured screen that differs between machines.
///
/// The footer renders the *configured* model, so these snapshots baked in
/// `Model:qwen3-coder:30b` and could never match on a developer whose `~/.netget` names a
/// different one. That is not a rendering property, and nothing here is testing it.
fn normalize_screen(screen: &str) -> String {
    screen
        .lines()
        .map(|line| {
            // The dashboard footer is `<model> | log:INFO ^L | web:ON ^W | ...`, so the model
            // is everything before the first ` | log:`. The legacy TUI wrote `Model:<name> |`.
            if let Some(i) = line.find(" \u{2502} log:").or_else(|| line.find(" | log:")) {
                return format!("<MODEL>{}", &line[i..]);
            }
            if let Some(start) = line.find("Model:") {
                let after = start + "Model:".len();
                return match line[after..].find(" |") {
                    Some(i) => format!("{}Model:<MODEL>{}", &line[..start], &line[after + i..]),
                    None => format!("{}Model:<MODEL>", &line[..start]),
                };
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Capture terminal output and parse it with vt100
fn capture_screen(pty: &mut pty_process::blocking::Pty) -> String {
    use std::os::unix::io::AsRawFd;

    // Set PTY to non-blocking mode
    let fd = pty.as_raw_fd();
    unsafe {
        let mut flags = libc::fcntl(fd, libc::F_GETFL);
        flags |= libc::O_NONBLOCK;
        libc::fcntl(fd, libc::F_SETFL, flags);
    }

    // Create a parser to accumulate all terminal output
    let mut parser = Parser::new(TERMINAL_HEIGHT, TERMINAL_WIDTH, 0);

    // Accumulate all bytes
    let mut all_bytes = Vec::new();
    let mut buf = vec![0u8; 4096];

    // Read until the screen stops changing, rather than for a fixed number of ticks.
    //
    // This used to read 10 x 100ms and stop as soon as it had *any* bytes after the fourth
    // tick - about a second, and less if the first frame arrived early. NetGet's startup does
    // scripting-environment detection, privilege probing and registry construction before the
    // TUI paints, so under `--test-threads=100` the capture regularly happened before the
    // first frame and every one of these tests compared a blank screen against a full one.
    // Waiting for quiescence makes the capture describe what the terminal settled on, at
    // whatever speed the machine manages.
    // Bounded at 2.5s. The dashboard repaints on a timer, so "no new bytes" is not guaranteed
    // to arrive at all — a long deadline then costs the full wait on every single capture, and
    // several tests capture three or four times. 2.5s is comfortably more than the ~300ms a
    // frame takes here while keeping the whole suite in the tens of seconds.
    let deadline = std::time::Instant::now() + Duration::from_millis(2500);
    let mut quiet_since = std::time::Instant::now();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));

        let mut read_any = false;
        loop {
            match pty.read(&mut buf) {
                Ok(n) if n > 0 => {
                    all_bytes.extend_from_slice(&buf[..n]);
                    parser.process(&buf[..n]);
                    read_any = true;
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        if read_any {
            quiet_since = std::time::Instant::now();
        } else if !all_bytes.is_empty() && quiet_since.elapsed() >= Duration::from_millis(400) {
            break;
        }
    }

    if all_bytes.is_empty() {
        return String::from("(no output captured)");
    }

    let screen = parser.screen();

    // Extract text content line by line
    let mut lines = Vec::new();
    for row in 0..TERMINAL_HEIGHT {
        let mut line = String::new();
        for col in 0..TERMINAL_WIDTH {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(&cell.contents());
            }
        }
        lines.push(line.trim_end().to_string());
    }

    normalize_screen(&lines.join("\n"))
}

/// Capture terminal output with custom height
fn capture_screen_with_height(pty: &mut pty_process::blocking::Pty, height: u16) -> String {
    use std::os::unix::io::AsRawFd;

    // Set PTY to non-blocking mode
    let fd = pty.as_raw_fd();
    unsafe {
        let mut flags = libc::fcntl(fd, libc::F_GETFL);
        flags |= libc::O_NONBLOCK;
        libc::fcntl(fd, libc::F_SETFL, flags);
    }

    // Create a parser with custom height
    let mut parser = Parser::new(height, TERMINAL_WIDTH, 0);

    // Accumulate all bytes
    let mut all_bytes = Vec::new();
    let mut buf = vec![0u8; 4096];

    // Try to read multiple times to get all output
    for attempt in 0..10 {
        std::thread::sleep(Duration::from_millis(100));

        loop {
            match pty.read(&mut buf) {
                Ok(n) if n > 0 => {
                    all_bytes.extend_from_slice(&buf[..n]);
                    parser.process(&buf[..n]);
                }
                Ok(_) => break, // No more data
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // If we got some data, give it a bit more time then stop
        if !all_bytes.is_empty() && attempt > 3 {
            break;
        }
    }

    if all_bytes.is_empty() {
        return String::from("(no output captured)");
    }

    let screen = parser.screen();

    // Extract text content line by line
    let mut lines = Vec::new();
    for row in 0..height {
        let mut line = String::new();
        for col in 0..TERMINAL_WIDTH {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(&cell.contents());
            }
        }
        lines.push(line.trim_end().to_string());
    }

    normalize_screen(&lines.join("\n"))
}

/// Send input to the PTY
fn send_input(pty: &mut pty_process::blocking::Pty, input: &str) {
    pty.write_all(input.as_bytes())
        .expect("Failed to write to PTY");
    std::thread::sleep(Duration::from_millis(150));
}

/// Send a control character (e.g., 'c' for Ctrl+C)
fn send_ctrl(pty: &mut pty_process::blocking::Pty, ch: char) {
    let ctrl_byte = (ch.to_ascii_uppercase() as u8) - b'A' + 1;
    pty.write_all(&[ctrl_byte])
        .expect("Failed to send control code");
    std::thread::sleep(Duration::from_millis(100));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_tui_render() {
        let (mut pty, _child) = spawn_netget();

        // Give TUI plenty of time to render
        std::thread::sleep(Duration::from_millis(2000));

        // Capture the initial screen
        let screen = capture_screen(&mut pty);

        println!("=== Initial TUI Render ===");
        println!("{}", screen);
        println!("===========================");

        // Verify we got SOME output (TUI is rendering)
        assert!(
            !screen.is_empty() && screen != "(no output captured)",
            "Expected terminal output from NetGet"
        );

        // Create snapshot
        snapshot_util::assert_snapshot("initial_tui", SNAPSHOT_DIR, &screen);

        // Cleanup
        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_typing_simple_input() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));

        // Clear initial output
        let _ = capture_screen(&mut pty);

        // Type some text
        send_input(&mut pty, "listen on port 8080");

        // Capture screen after typing
        let screen = capture_screen(&mut pty);

        println!("=== After Typing ===");
        println!("{}", screen);
        println!("====================");

        // Note: Input may not immediately appear due to TUI rendering
        // Just capture the snapshot to see what we got
        snapshot_util::assert_snapshot("typed_simple_input", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_cursor_navigation_ctrl_a_ctrl_e() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Type text
        send_input(&mut pty, "hello world");
        std::thread::sleep(Duration::from_millis(200));

        // Move cursor to beginning (Ctrl+A)
        send_ctrl(&mut pty, 'a');
        std::thread::sleep(Duration::from_millis(100));

        // Type at beginning
        send_input(&mut pty, "start ");

        // Capture screen
        let screen = capture_screen(&mut pty);

        println!("=== After Cursor Navigation ===");
        println!("{}", screen);
        println!("================================");

        snapshot_util::assert_snapshot("cursor_navigation", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_ctrl_k_delete_to_end() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Type text
        send_input(&mut pty, "delete this text");
        std::thread::sleep(Duration::from_millis(200));

        // Move to beginning
        send_ctrl(&mut pty, 'a');
        std::thread::sleep(Duration::from_millis(100));

        // Delete to end (Ctrl+K)
        send_ctrl(&mut pty, 'k');
        std::thread::sleep(Duration::from_millis(200));

        // Capture screen - should show empty or cleared input
        let screen = capture_screen(&mut pty);

        println!("=== After Ctrl+K ===");
        println!("{}", screen);
        println!("====================");

        snapshot_util::assert_snapshot("ctrl_k_delete", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_multiline_input_with_shift_enter() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Type first line
        send_input(&mut pty, "listen on port 21");

        // TODO: Shift+Enter is difficult to simulate in PTY
        // For now, just verify single line works
        std::thread::sleep(Duration::from_millis(200));

        let screen = capture_screen(&mut pty);

        println!("=== Input Line ===");
        println!("{}", screen);
        println!("==================");

        snapshot_util::assert_snapshot("input_line", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_idle_footer_state() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));

        // Capture the footer area
        let screen = capture_screen(&mut pty);

        println!("=== Idle Footer State ===");
        println!("{}", screen);
        println!("=========================");

        // Should show idle state or no servers
        let has_idle_state =
            screen.contains("Idle") || screen.contains("No server") || screen.contains("Input");

        assert!(has_idle_state, "Expected idle/no server state in footer");

        snapshot_util::assert_snapshot("idle_footer", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_overflow_small() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Send /test 5 command to generate 5 lines of output
        send_input(&mut pty, "/test 5");
        // Press Enter to submit (PTY uses \r for Enter)
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800)); // More time for rendering

        // Capture screen
        let screen = capture_screen(&mut pty);

        println!("=== After 5 Test Lines ===");
        println!("{}", screen);
        println!("===========================");

        // Verify we see test output
        assert!(screen.contains("Test line"), "Expected to see test output");

        snapshot_util::assert_snapshot("overflow_small", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_overflow_medium() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Send /test 15 command to generate 15 lines of output
        send_input(&mut pty, "/test 15");
        // Press Enter to submit (PTY uses \r for Enter)
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Capture screen - should show scrolling behavior
        let screen = capture_screen(&mut pty);

        println!("=== After 15 Test Lines ===");
        println!("{}", screen);
        println!("============================");

        // Verify we see test output (later lines should be visible, earlier ones scrolled off)
        assert!(screen.contains("Test line"), "Expected to see test output");

        snapshot_util::assert_snapshot("overflow_medium", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_overflow_heavy() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Send /test 20 command to generate 20 lines of output
        send_input(&mut pty, "/test 20");
        // Press Enter to submit (PTY uses \r for Enter)
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(1000));

        // Capture screen - many lines should have scrolled off
        let screen = capture_screen(&mut pty);

        println!("=== After 20 Test Lines ===");
        println!("{}", screen);
        println!("============================");

        // Verify we see test output (only the most recent lines should be visible)
        assert!(screen.contains("Test line"), "Expected to see test output");
        // Footer should still be visible and sticky
        assert!(
            screen.contains("Input:"),
            "Expected footer to remain visible"
        );

        snapshot_util::assert_snapshot("overflow_heavy", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_dynamic_footer_growing() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Generate 10 test lines
        send_input(&mut pty, "/test 10");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Set multi-line footer status (3 lines)
        send_input(
            &mut pty,
            "/footer_status Line 1 of status\\nLine 2 of status\\nLine 3 of status",
        );
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Capture screen - footer should be taller, scroll region smaller
        let screen = capture_screen(&mut pty);

        println!("=== After Multi-line Footer ===");
        println!("{}", screen);
        println!("================================");

        // Verify we see multi-line footer
        // NOTE: With simplified expansion logic, test output gets cleared during footer operations
        assert!(
            screen.contains("Line 1 of status"),
            "Expected to see line 1 of status"
        );
        assert!(
            screen.contains("Line 2 of status"),
            "Expected to see line 2 of status"
        );
        assert!(
            screen.contains("Line 3 of status"),
            "Expected to see line 3 of status"
        );
        // Verify no double status line (the main bug we're fixing)
        let status_count = screen
            .matches(" Idle | - | no connection | qwen3-coder:30b |")
            .count();
        assert_eq!(
            status_count, 1,
            "Should have exactly one status line, found {}",
            status_count
        );

        snapshot_util::assert_snapshot("dynamic_footer_growing", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_dynamic_footer_shrinking() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Set multi-line footer status first
        send_input(
            &mut pty,
            "/footer_status Multi\\nLine\\nStatus\\nMany\\nLines",
        );
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(500));

        // Generate 10 test lines
        send_input(&mut pty, "/test 10");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Now reduce back to single line
        send_input(&mut pty, "/footer_status Single line status");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Capture screen - footer should be shorter, scroll region larger
        let screen = capture_screen(&mut pty);

        println!("=== After Single-line Footer ===");
        println!("{}", screen);
        println!("=================================");

        // Verify we see test output and single-line footer
        assert!(
            screen.contains("Test line 1 of 10"),
            "Expected to see test line 1 of output"
        );
        assert!(
            screen.contains("Test line 10 of 10"),
            "Expected to see test line 10 of output"
        );
        assert!(
            screen.contains("Single line status"),
            "Expected to see single line status"
        );
        // Should NOT contain the multi-line status anymore
        assert!(!screen.contains("Multi"), "Should not see 'Multi' anymore");

        snapshot_util::assert_snapshot("dynamic_footer_shrinking", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_dynamic_footer_expand_shrink_expand() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Generate 10 test lines with default footer (5 lines)
        send_input(&mut pty, "/test 10");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // EXPAND: Set multi-line footer (7 lines total - grows by 2)
        send_input(&mut pty, "/footer_status A\\nB\\nC");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // SHRINK: Reduce to 2 lines (shrinks by 1)
        send_input(&mut pty, "/footer_status X");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // EXPAND: Grow back to 4 lines (grows by 2)
        send_input(&mut pty, "/footer_status Y\\nZ");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Capture screen
        let screen = capture_screen(&mut pty);

        println!("=== After Expand-Shrink-Expand ===");
        println!("{}", screen);
        println!("===================================");

        // Buffer optimization test: The key is that second expansion consumed from buffer
        // Debug log shows: expand(+2) pushed 2, shrink(-2) created buffer=2, expand(+1) consumed 1 from buffer (pushed 0)
        // So only the FIRST expansion caused pushing, the second reused the buffer
        // NOTE: With simplified expansion logic, test output gets cleared during footer operations
        // but the footer itself renders correctly and buffer management works
        // The important part is verifying no double status lines
        assert!(screen.contains("Y"), "Should see Y in footer");
        assert!(screen.contains("Z"), "Should see Z in footer");
        // Verify no double status line
        let status_count = screen
            .matches(" Idle | - | no connection | qwen3-coder:30b |")
            .count();
        assert_eq!(
            status_count, 1,
            "Should have exactly one status line, found {}",
            status_count
        );

        snapshot_util::assert_snapshot(
            "dynamic_footer_expand_shrink_expand",
            SNAPSHOT_DIR,
            &screen,
        );

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_dynamic_footer_set_same_value_twice() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Set footer status
        send_input(&mut pty, "/footer_status Test Status");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Capture screen after first set
        let screen1 = capture_screen(&mut pty);
        println!("=== After First Set ===");
        println!("{}", screen1);
        println!("========================");

        // Set the SAME footer status again
        send_input(&mut pty, "/footer_status Test Status");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        // Capture screen after second set
        let screen2 = capture_screen(&mut pty);
        println!("=== After Second Set (Same Value) ===");
        println!("{}", screen2);
        println!("=======================================");

        // Count blank lines at the top
        let blank_lines = screen2
            .lines()
            .take_while(|line| line.trim().is_empty())
            .count();
        println!("Blank lines at top: {}", blank_lines);

        // When setting footer to same value twice, command echoes are suppressed for SetFooterStatus
        // We just verify the footer content is correct
        assert!(
            screen2.contains("Test Status"),
            "Footer should show Test Status"
        );

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_footer_changes_with_output_lines() {
        let (mut pty, _child) = spawn_netget();

        // Wait for initial render
        std::thread::sleep(Duration::from_millis(1000));
        let _ = capture_screen(&mut pty);

        // Step 1: Generate initial output (5 lines)
        send_input(&mut pty, "/test 5");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        let screen1 = capture_screen(&mut pty);
        println!("=== After Initial Output (5 lines) ===");
        println!("{}", screen1);
        println!("========================================");
        snapshot_util::assert_snapshot("footer_output_step1_initial", SNAPSHOT_DIR, &screen1);

        // Step 2: EXPAND footer (default 5 lines → 7 lines, grows by 2)
        // NOTE: When footer expands without sufficient buffer, content at bottom may be overwritten.
        // The key fix is ensuring NO DOUBLE STATUS LINES, not necessarily preserving all content.
        send_input(&mut pty, "/footer_status Expanded\\nFooter\\nStatus");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        let screen2 = capture_screen(&mut pty);
        println!("=== After Footer Expansion ===");
        println!("{}", screen2);
        println!("================================");
        snapshot_util::assert_snapshot("footer_output_step2_expand", SNAPSHOT_DIR, &screen2);

        // Step 3: Generate more output (3 lines)
        send_input(&mut pty, "/test 3");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        let screen3 = capture_screen(&mut pty);
        println!("=== After Output with Expanded Footer ===");
        println!("{}", screen3);
        println!("==========================================");
        snapshot_util::assert_snapshot("footer_output_step3_output", SNAPSHOT_DIR, &screen3);

        // Step 4: SHRINK footer (7 lines → 6 lines, shrinks by 1)
        // Content should remain visible, buffer increases
        send_input(&mut pty, "/footer_status Shrunk");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        let screen4 = capture_screen(&mut pty);
        println!("=== After Footer Shrink ===");
        println!("{}", screen4);
        println!("=============================");
        snapshot_util::assert_snapshot("footer_output_step4_shrink", SNAPSHOT_DIR, &screen4);

        // Step 5: Generate final output (4 lines)
        send_input(&mut pty, "/test 4");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(800));

        let screen5 = capture_screen(&mut pty);
        println!("=== After Final Output ===");
        println!("{}", screen5);
        println!("===========================");
        snapshot_util::assert_snapshot("footer_output_step5_final", SNAPSHOT_DIR, &screen5);

        // Verify no double status lines in any snapshot
        assert!(
            !screen1.contains(
                " Idle | - | no connection | qwen3-coder:30b | ↑0 ↓0\n Idle | - | no connection"
            ),
            "Step 1: Found double status line"
        );
        assert!(
            !screen2.contains(
                " Idle | - | no connection | qwen3-coder:30b | ↑0 ↓0\n Idle | - | no connection"
            ),
            "Step 2: Found double status line"
        );
        assert!(
            !screen3.contains(
                " Idle | - | no connection | qwen3-coder:30b | ↑0 ↓0\n Idle | - | no connection"
            ),
            "Step 3: Found double status line"
        );
        assert!(
            !screen4.contains(
                " Idle | - | no connection | qwen3-coder:30b | ↑0 ↓0\n Idle | - | no connection"
            ),
            "Step 4: Found double status line"
        );
        assert!(
            !screen5.contains(
                " Idle | - | no connection | qwen3-coder:30b | ↑0 ↓0\n Idle | - | no connection"
            ),
            "Step 5: Found double status line"
        );

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_pre_existing_content_preserved() {
        use nix::libc::TIOCSWINSZ;
        use nix::pty::Winsize;
        use std::os::unix::io::AsRawFd;

        // Use a taller terminal (100 lines) to fit welcome message + pre-existing content
        const TALL_TERMINAL_HEIGHT: u16 = 100;

        // Create PTY using openpty (simpler than posix_openpt + manual slave open)
        use nix::pty::openpty;

        // Open a new PTY (returns both master and slave as OwnedFds)
        let pty_result = openpty(None, None).expect("Failed to open PTY");

        // openpty already returns OwnedFds, use them directly
        let master_owned = pty_result.master;
        let slave_owned = pty_result.slave;

        // Create PTY from file descriptor
        let mut pty = unsafe { pty_process::blocking::Pty::from_fd(master_owned) };
        let pts = unsafe { pty_process::blocking::Pts::from_fd(slave_owned) };

        // Set PTY window size to 80x100 so terminal::size() can detect it correctly
        let winsize = Winsize {
            ws_row: TALL_TERMINAL_HEIGHT,
            ws_col: TERMINAL_WIDTH,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            let fd = pty.as_raw_fd();
            let ret = libc::ioctl(fd, TIOCSWINSZ as _, &winsize as *const _);
            assert!(ret == 0, "Failed to set PTY window size");
        }

        // Write 10 lines of pre-existing content BEFORE starting netget
        // Use ANSI escape codes to position content at specific lines (0-10)
        pty.write_all(b"\x1b[1;1H=== Pre-existing terminal content (10 lines) ===\r\n")
            .expect("Failed to write");
        for i in 1..=10 {
            pty.write_all(format!("\x1b[{};1HPre-existing line {}\r\n", i + 1, i).as_bytes())
                .expect("Failed to write");
        }
        std::thread::sleep(Duration::from_millis(300));

        // Now spawn netget in the PTY that already has content
        let binary_path = env!("CARGO_BIN_EXE_netget");
        let mut cmd = pty_process::blocking::Command::new(binary_path);
        let _child = NetGetChild(Some(cmd.spawn(pts).expect("Failed to spawn netget in PTY")));

        // Give TUI time to start and render
        std::thread::sleep(Duration::from_millis(2000));

        // Capture the screen with taller height
        let screen = capture_screen_with_height(&mut pty, TALL_TERMINAL_HEIGHT);

        println!("=== Screen with Pre-existing Content (100 lines) ===");
        println!("{}", screen);
        println!("=====================================================");

        // Verify pre-existing content is still visible ABOVE netget output
        assert!(
            screen.contains("Pre-existing terminal content"),
            "Expected to see pre-existing content header"
        );
        assert!(
            screen.contains("Pre-existing line 1"),
            "Expected to see line 1"
        );
        assert!(
            screen.contains("Pre-existing line 10"),
            "Expected to see line 10"
        );

        // Verify NetGet welcome message is present
        assert!(
            screen.contains("TUI initialized") || screen.contains("NetGet"),
            "Expected to see NetGet output"
        );

        // Verify footer is present at bottom
        assert!(screen.contains("Input:"), "Expected footer to be present");

        snapshot_util::assert_snapshot("pre_existing_content", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }

    #[test]
    fn test_usage_command_display() {
        let (mut pty, _child) = spawn_netget();

        // Wait longer for initial render and welcome messages
        std::thread::sleep(Duration::from_millis(2500));
        let initial_screen = capture_screen(&mut pty);

        println!("=== Initial Screen (before /usage) ===");
        println!("{}", initial_screen);
        println!("=======================================");

        // First test if ANY command output works
        send_input(&mut pty, "/test 2");
        pty.write_all(b"\r").expect("Failed to send Enter");
        std::thread::sleep(Duration::from_millis(1000));
        let test_screen = capture_screen(&mut pty);
        println!("=== After /test 2 command ===");
        println!("{}", test_screen);
        println!("==============================");

        // Now send /usage command to toggle usage stats
        send_input(&mut pty, "/usage");
        // Press Enter to submit (PTY uses \r for Enter)
        pty.write_all(b"\r").expect("Failed to send Enter");

        // Capture immediately after sending command
        std::thread::sleep(Duration::from_millis(500));
        let screen_immediate = capture_screen(&mut pty);
        println!("=== Immediately After Sending /usage ===");
        println!("{}", screen_immediate);
        println!("=========================================");

        // Wait longer for stats to update and render
        std::thread::sleep(Duration::from_millis(2000));

        // Capture screen after /usage command
        let screen = capture_screen(&mut pty);

        println!("=== After /usage Command (2.5s later) ===");
        println!("{}", screen);
        println!("==========================================");

        // The /usage command should toggle usage stats in the footer
        // However, in the PTY test environment, command output may not appear
        // in the scrollback (similar to /test command behavior).
        // Instead, we'll just verify the snapshot is created and the footer is valid.

        // Verify no double status lines (similar to other footer tests)
        assert!(
            !screen.contains(
                " Model:None | Log:INFO <^l> | WebSearch:ON <^w> | Handler:ANY <^h>\n Model:None"
            ),
            "Found double status line"
        );

        snapshot_util::assert_snapshot("usage_command_enabled", SNAPSHOT_DIR, &screen);

        send_ctrl(&mut pty, 'c');
    }
}
