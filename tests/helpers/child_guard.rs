//! Reaping the `netget` subprocesses the test suite spawns.
//!
//! # The problem
//!
//! Measured 16 September 2026: **78 orphaned `netget` processes** alive on one
//! developer machine, every one of them at PPID 1, accumulated over ten hours.
//! 73 came from `/tmp/vfull/debug/netget` and five from
//! `/tmp/netget-eval-target/debug/netget` — ordinary `CARGO_TARGET_DIR`s, so
//! ordinary test runs. Every test that spawns the binary and is then killed
//! before it can clean up — a timeout, a `cargo test` interrupt, a panic that
//! aborts, `--no-fail-fast` moving on after a hang — left its child running, and
//! nothing collected them.
//!
//! They are not merely untidy. They hold loopback ports, memory and file
//! descriptors, and they make every later measurement dishonest: a whole day of
//! "load-sensitive flakes" was investigated on a machine quietly carrying dozens
//! of them, and at least one such investigation reached the wrong conclusion.
//!
//! # Two halves, because one is not enough
//!
//! **`Drop` is necessary and not sufficient.** It runs on the normal path and on
//! an unwinding panic. It does not run when the test binary is `SIGKILL`ed, when
//! it aborts, or when it calls `std::process::exit`. Those are exactly the cases
//! that produced the 78.
//!
//! So there are two mechanisms here:
//!
//! 1. [`DeathTie`], an **OS-level tie** that survives a hard-killed parent, and
//! 2. [`sweep_orphaned_netget`], a sweeper for what is already loose.
//!
//! ## The OS-level tie, and why it is shaped like this on macOS
//!
//! Linux has `PR_SET_PDEATHSIG`: the kernel signals the child when its parent
//! dies. **macOS has no equivalent**, and the alternatives each have a defect:
//!
//! - *A process group the harness kills.* Useless here — the harness is the
//!   thing that died.
//! - *A watchdog thread inside netget itself,* comparing `getppid()` against the
//!   pid it was spawned under. This works, but it puts test-only machinery into
//!   the shipped binary and has to be wired through startup. Rejected for
//!   footprint, not for correctness.
//! - *`kqueue`'s `NOTE_EXIT`.* It notifies a **watcher**, and the watcher here
//!   would be the test binary, which is dead. It only helps if the watcher is a
//!   third process — at which point the third process is the mechanism and
//!   `kqueue` is an implementation detail of how it waits.
//!
//! So: a third process, waiting. The cheap way to make it wait — and the reason
//! this needs no polling at all — is a **pipe**. The harness creates a pipe,
//! keeps the write end (marked `FD_CLOEXEC`, so no spawned child inherits it and
//! nothing but this process can hold it open), and hands the read end to a
//! `/bin/sh` that blocks in `read`. When this process dies *for any reason
//! whatsoever*, the kernel closes its descriptors, the last writer is gone, the
//! `read` returns EOF and the shell `SIGKILL`s the pid it was given. No polling,
//! no timer, no fork per tick, and the latency is however long it takes the
//! kernel to schedule one shell.
//!
//! **What this covers:** the parent `SIGKILL`ed; the parent aborting (a Rust
//! stack overflow is a `SIGSEGV`, which `Drop` never sees); `std::process::exit`;
//! a `cargo test` interrupt; a harness timeout that kills the test binary; and,
//! as a backstop, the ordinary paths that `Drop` already handles.
//!
//! **What it does not cover:** the reaper shell itself being killed (by a
//! process-group-wide `kill -9`, or by an OOM killer), the machine losing power,
//! and a child that has already been reaped and its pid recycled — which is why
//! [`DeathTie::disarm`] exists and why the normal teardown path calls it after it
//! has sent its own `SIGKILL`.
//!
//! One `/bin/sh` per live netget instance is the cost. It is blocked in `read`,
//! so it burns no CPU, and it exits the instant its pipe closes — including when
//! the whole run ends normally.

#![allow(dead_code)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

/// An armed tie between this process and `pid`.
///
/// While this value is alive, a reaper shell is blocked on a pipe whose only
/// writer is this process. Dropping it without calling [`disarm`](Self::disarm)
/// lets the reaper fire; calling `disarm` first kills the reaper instead.
///
/// The ordering matters on teardown: send the child `SIGKILL` **first**, then
/// disarm. Disarming before the kill leaves nothing watching; killing before the
/// disarm means the only window in which the reaper could act on a recycled pid
/// is one where the child is already dead by our own hand.
pub struct DeathTie {
    /// The write end. Held, never written to. Its closure *is* the signal.
    write_end: Option<OwnedFd>,
    reaper: Option<std::process::Child>,
    /// The pid this tie will kill, kept for diagnostics.
    pid: u32,
}

impl DeathTie {
    /// The pid this tie is armed against.
    pub fn target_pid(&self) -> u32 {
        self.pid
    }

    /// Stop the reaper without letting it fire.
    ///
    /// Call this only after the child has been signalled, and reap the reaper
    /// here so it does not become a zombie of a still-running test binary.
    pub fn disarm(&mut self) {
        if let Some(mut reaper) = self.reaper.take() {
            let _ = reaper.kill();
            let _ = reaper.wait();
        }
        self.write_end.take();
    }
}

impl Drop for DeathTie {
    fn drop(&mut self) {
        // No disarm: closing the write end is what makes the reaper act, and a
        // tie dropped without an explicit disarm is one whose owner did not
        // confirm the child was dealt with.
        self.write_end.take();
        if let Some(mut reaper) = self.reaper.take() {
            // Do not block: the reaper has work to do. It is reparented to init
            // when this process exits, which reaps it.
            let _ = reaper.try_wait();
        }
    }
}

/// Arm an OS-level tie so `pid` dies when **this** process does, however it dies.
///
/// Returns `None` if the pipe or the reaper could not be created; the caller's
/// own `Drop` guard is then the only protection, which is the status quo rather
/// than a regression.
///
/// See the module docs for what this covers and what it does not.
pub fn arm_death_tie(pid: u32) -> Option<DeathTie> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a two-element array of the right type, which is exactly
    // what `pipe(2)` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: both descriptors were just returned by `pipe(2)` and are open.
    //
    // `FD_CLOEXEC` on the *write* end is load-bearing, not hygiene: if a spawned
    // child inherited it, that child would be a writer, the pipe would never
    // reach EOF while it lived, and the reaper would wait forever for the death
    // of a process it was supposed to cause.
    unsafe {
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
    }
    // SAFETY: fresh descriptors from `pipe(2)`, each owned exactly once here.
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // `read` blocks until EOF. Then: the process group first (harmless when the
    // child is not a group leader — a group id equals its leader's pid, so the
    // only process that group could belong to is this very child), then the pid.
    let script = format!(
        "read _netget_reaper_eof 2>/dev/null; kill -9 -{pid} 2>/dev/null; kill -9 {pid} 2>/dev/null; exit 0"
    );

    let reaper = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        // `Stdio::from(OwnedFd)` dup2s onto fd 0, which clears `FD_CLOEXEC`, so
        // the reaper really does hold the read end.
        .stdin(Stdio::from(read_end))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    Some(DeathTie {
        write_end: Some(write_end),
        reaper: Some(reaper),
        pid,
    })
}

// ---------------------------------------------------------------------------
// A process-wide registry, keyed by pid
// ---------------------------------------------------------------------------
//
// The harness does not keep one owner per child. `start_netget` builds a
// `NetGetInstance`, and `start_netget_server` / `start_netget_client` then wrap
// it in `ManuallyDrop` and `ptr::read` the child out into a different struct
// with a different `Drop`. A tie stored *in* the instance would be either left
// behind by that move or duplicated by it.
//
// The pid survives the move, so the pid is the key. `tie_child` arms; whichever
// teardown path ends up owning the child calls `untie_child` after it has sent
// its own kill.

type TieRegistry = std::sync::Mutex<std::collections::HashMap<u32, DeathTie>>;

fn registry() -> &'static TieRegistry {
    static REGISTRY: std::sync::OnceLock<TieRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Arm a tie for `pid` and remember it, so any teardown path can release it.
///
/// Call immediately after `spawn()`. A failure to arm is logged and ignored:
/// the caller's `Drop` guard still covers the ordinary cases, so an unarmed tie
/// is the pre-existing behaviour rather than a new defect.
pub fn tie_child(pid: u32) {
    match arm_death_tie(pid) {
        Some(tie) => {
            if let Ok(mut map) = registry().lock() {
                if let Some(mut stale) = map.insert(pid, tie) {
                    // A tie for this pid was never released, and the kernel has
                    // handed the pid out again. **Disarm** the old one rather
                    // than letting it drop armed: dropping it fires its reaper,
                    // and the process that pid now names is the child we are in
                    // the middle of arming. Arming would kill it.
                    stale.disarm();
                }
            }
        }
        None => eprintln!("[reap] WARNING: could not arm a death tie for pid {pid}"),
    }
}

/// Release the tie for `pid`. Call **after** signalling the child, never before.
pub fn untie_child(pid: u32) {
    let tie = registry().lock().ok().and_then(|mut map| map.remove(&pid));
    if let Some(mut tie) = tie {
        tie.disarm();
    }
}

/// How many ties are currently armed. For tests.
pub fn armed_tie_count() -> usize {
    registry().lock().map(|map| map.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The sweeper
// ---------------------------------------------------------------------------

/// One line of `ps` output, already split.
#[derive(Debug, Clone)]
pub struct ProcRow {
    pub pid: i32,
    pub ppid: i32,
    /// The full command line, `argv` joined by spaces, untruncated (`ps -ww`).
    pub cmdline: String,
}

/// Does this process look like an orphaned **test** netget, safe to kill?
///
/// `CLAUDE.md`'s standing rule is that netget processes are never killed. This is
/// the one narrow exception, so the predicate is deliberately a conjunction of
/// three independent facts, any one of which failing means "leave it alone":
///
/// 1. **`ppid == 1`.** A netget with a live parent belongs to that parent —
///    another agent's test run, a shell, the editor. Only a process whose parent
///    is gone is an orphan.
/// 2. **The executable lives under a build target directory.** Either a path
///    component is literally `target`, or the containing directory is a cargo
///    profile directory (`debug`/`release`) — which is what
///    `CARGO_TARGET_DIR=/tmp/vfull` produces, and where 73 of the 78 came from.
///    An installed binary (`/Users/…/bin/netget`, `/usr/local/bin/netget`) never
///    matches.
/// 3. **No `--mcp` flag.** The operator's own session is
///    `<repo>/target/release/netget --mcp …`. It satisfies (2) by construction,
///    and it would satisfy (1) the moment its own parent exited — so this is the
///    condition actually protecting it, and it is why the check is on any
///    argument *beginning* `--mcp` rather than the exact word (`--mcp-http` is
///    the same session in a different clothes).
///
/// `cmdline` is `argv` joined by spaces; `argv[0]` is taken as the executable
/// path, which is true for everything this suite spawns (`Command::new(path)`).
pub fn is_orphaned_test_netget(ppid: i32, cmdline: &str) -> bool {
    if ppid != 1 {
        return false;
    }

    let mut argv = cmdline.split_whitespace();
    let Some(exe) = argv.next() else {
        return false;
    };

    // (2a) It must actually be netget, by the name of the file that is running.
    if exe.rsplit('/').next() != Some("netget") {
        return false;
    }

    // (2b) …under a build target directory.
    let components: Vec<&str> = exe.split('/').collect();
    let under_target = components.contains(&"target");
    let profile_dir = components
        .len()
        .checked_sub(2)
        .map(|i| components[i] == "debug" || components[i] == "release")
        .unwrap_or(false);
    if !under_target && !profile_dir {
        return false;
    }

    // (3) Never an MCP session. This is the operator's own process.
    if argv.any(|arg| arg.starts_with("--mcp")) {
        return false;
    }

    true
}

/// Read the process table. Returns an empty list if `ps` is unavailable.
pub fn process_table() -> Vec<ProcRow> {
    // `-ww` disables the column-width truncation that would otherwise cut the
    // argument list short — and a truncated line could lose the `--mcp` that is
    // the only thing protecting the operator's session.
    let Ok(out) = Command::new("ps")
        .args(["-Awwo", "pid=,ppid=,args="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let rest = line
                .split_whitespace()
                .skip(2)
                .collect::<Vec<_>>()
                .join(" ");
            if rest.is_empty() {
                return None;
            }
            Some(ProcRow {
                pid,
                ppid,
                cmdline: rest,
            })
        })
        .collect()
}

/// Every process the sweeper would kill, without killing anything.
pub fn find_orphaned_netget() -> Vec<ProcRow> {
    process_table()
        .into_iter()
        .filter(|row| is_orphaned_test_netget(row.ppid, &row.cmdline))
        .collect()
}

/// Kill every orphaned test netget. Returns what was killed.
pub fn sweep_orphaned_netget() -> Vec<ProcRow> {
    let victims = find_orphaned_netget();
    for row in &victims {
        // SAFETY: a plain `kill(2)` on a pid that `find_orphaned_netget`
        // established is an orphaned test binary.
        unsafe {
            libc::kill(row.pid, libc::SIGKILL);
        }
        println!(
            "[reap] killed orphaned test netget pid={} ({})",
            row.pid, row.cmdline
        );
    }
    victims
}

/// Sweep once per test binary, before the first netget is spawned.
///
/// Safe to run while other test binaries are mid-run: their children have a live
/// parent, so `ppid == 1` excludes them.
pub fn sweep_orphaned_netget_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let killed = sweep_orphaned_netget();
        if !killed.is_empty() {
            eprintln!(
                "[reap] swept {} orphaned netget process(es) left by earlier runs",
                killed.len()
            );
        }
    });
}
