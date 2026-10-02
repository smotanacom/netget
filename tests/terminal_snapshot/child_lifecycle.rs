//! PTY guard regressions using sleep, without starting the application or a model.
use super::{open_pty_retrying, NetGetChild};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

#[path = "../helpers/wrapper_lifecycle.rs"]
mod fixture;

fn spawn_fixture() -> (pty_process::blocking::Pty, NetGetChild, u32) {
    let pair = open_pty_retrying();
    let mut pty = unsafe { pty_process::blocking::Pty::from_fd(pair.master) };
    let pts = unsafe { pty_process::blocking::Pts::from_fd(pair.slave) };
    // Ignore terminal hangup before announcing readiness. Otherwise closing the
    // dead parent's PTY could make this pass even without a death tie.
    let child = pty_process::blocking::Command::new("/bin/sh")
        .args(["-c", "trap '' HUP; printf 'READY\\n'; exec /bin/sleep 600"])
        .spawn(pts)
        .unwrap();
    let pid = child.id();
    let child = NetGetChild::new(child).unwrap();
    let fd = pty.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let mut buffer = [0; 64];
    while !output.windows(5).any(|part| part == b"READY") {
        match pty.read(&mut buffer) {
            Ok(n) if n > 0 => output.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("PTY fixture failed before readiness: {other:?}"),
        }
        assert!(Instant::now() < deadline, "PTY fixture never became ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    (pty, child, pid)
}

#[test]
fn hard_killed_pty_parent_cannot_leave_child() {
    if let Some(marker) = fixture::worker_state() {
        let (_pty, _guard, pid) = spawn_fixture();
        fixture::announce_and_wait(&marker, pid);
    }
    fixture::assert_parent_death_cleans_child(
        "terminal_snapshot::child_lifecycle::hard_killed_pty_parent_cannot_leave_child",
    );
}

#[test]
fn normal_pty_teardown_is_bounded_and_reaps_child() {
    let (_pty, child, pid) = spawn_fixture();
    let started = Instant::now();
    drop(child);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        !fixture::pid_exists(pid),
        "PTY child must be reaped before guard returns"
    );
}

#[test]
fn already_reaped_pty_child_does_not_break_teardown() {
    let (_pty, mut child, pid) = spawn_fixture();
    let process = child.0.as_mut().unwrap();
    process.kill().unwrap();
    process.wait().unwrap();
    drop(child);
    assert!(!fixture::pid_exists(pid));
}

#[test]
fn failed_pty_death_tie_arming_kills_and_reaps_child() {
    let pair = open_pty_retrying();
    let _pty = unsafe { pty_process::blocking::Pty::from_fd(pair.master) };
    let pts = unsafe { pty_process::blocking::Pts::from_fd(pair.slave) };
    let child = pty_process::blocking::Command::new("/bin/sleep")
        .arg("600")
        .spawn(pts)
        .unwrap();
    let pid = child.id();
    assert!(NetGetChild::with_armer(child, |_| None).is_err());
    assert!(
        !fixture::pid_exists(pid),
        "failed arming cannot leak the PTY child"
    );
}
