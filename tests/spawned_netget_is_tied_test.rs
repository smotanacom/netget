//! Every test that spawns the `netget` binary ties the child's life to its own.
//!
//! # Why a ratchet and not a behavioural test
//!
//! The behaviour is already proved once, properly:
//! `tests/orphan_reaper_test.rs::sigkilled_parent_takes_its_child_with_it` actually `SIGKILL`s a
//! parent and watches the child disappear. Repeating that per call site would be slow and would
//! prove the same mechanism nine times.
//!
//! What no behavioural test can catch is the *next* file that spawns the binary and forgets. The
//! four fixed alongside this ratchet — `tool_call_integration_test.rs`, `cli_args_test.rs`,
//! `server/stdio/e2e_test.rs`, `toolcall/web_search_integration_test.rs` — were forgotten
//! exactly that way when the main harness adopted `child_guard`: the mechanism existed, and they
//! were simply not on the list.
//!
//! # Why `Drop` is not enough, restated here because it is the whole argument
//!
//! `kill_on_drop(true)` and a hand-written `Drop` guard run on the normal path and on an
//! unwinding panic. They do **not** run when the test binary is `SIGKILL`ed, when it aborts,
//! when it calls `std::process::exit`, or when a harness timeout kills it. Those are exactly the
//! cases that left **78 orphaned `netget` processes** alive on one machine over ten hours,
//! holding loopback ports and quietly oversubscribing the box while load-sensitive test failures
//! were being investigated on it. `child_guard::tie_child` is the OS-level half; see that
//! module's docs for what it covers and what it does not.

use std::path::{Path, PathBuf};

/// Files that spawn the binary and are **not** tied, each with the reason it is still here.
///
/// **Shrink only.** Adding an entry means accepting that a killed test run can leave a `netget`
/// behind; do that only with a reason better than "it was easier".
const BASELINE: &[(&str, &str)] = &[
    (
        "tests/e2e/netget_wrapper.rs",
        "has a `Drop` guard (`start_kill`) but no OS-level tie. Same class as the four fixed \
         here and a genuine gap; left alone because it was outside the task boundary that \
         landed this ratchet, not because it is safe.",
    ),
    (
        "tests/terminal_snapshot/mod.rs",
        "spawns netget inside a pty, with a `Drop` guard whose comments record that `kill()` + \
         `wait()` once hung on the success path — so adopting the tie here needs the pty \
         teardown re-read rather than a two-line edit. Also outside that task's boundary.",
    ),
];

/// Does this file spawn the netget binary?
///
/// Deliberately crude: naming the binary and calling `spawn` is what every such site looks
/// like. A false positive is a file that must then say `tie_child`, which is harmless; a false
/// negative is a file that spawns netget by some route nobody has used yet.
fn spawns_netget(source: &str) -> bool {
    let names_binary = source.contains("CARGO_BIN_EXE_netget")
        || source.contains("get_netget_binary")
        || source.contains("netget_binary_path");
    names_binary && source.contains("spawn(")
}

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_test_spawns_netget_without_a_death_tie() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&tests_dir(), &mut files);
    files.sort();

    let baselined: Vec<&str> = BASELINE.iter().map(|(path, _)| *path).collect();

    let mut offenders = Vec::new();
    let mut baseline_still_needed = Vec::new();

    for path in &files {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // The mechanism itself, and the test that proves it, both name the binary while
        // describing it.
        if relative.ends_with("helpers/child_guard.rs")
            || relative.ends_with("orphan_reaper_test.rs")
        {
            continue;
        }

        let source = std::fs::read_to_string(path).unwrap_or_default();
        if !spawns_netget(&source) {
            continue;
        }
        if source.contains("tie_child") {
            if baselined.contains(&relative.as_str()) {
                baseline_still_needed.push(relative);
            }
            continue;
        }
        if baselined.contains(&relative.as_str()) {
            continue;
        }
        offenders.push(relative);
    }

    assert!(
        offenders.is_empty(),
        "these test files spawn the `netget` binary without arming a death tie, so a run that \
         is killed or aborted leaves the child alive at PPID 1 holding its ports:\n  {}\n\n\
         The fix is two lines: `child_guard::tie_child(pid)` right after `spawn()`, and \
         `child_guard::untie_child(pid)` after the child has been signalled (never before — a \
         tie released first leaves nothing watching).",
        offenders.join("\n  ")
    );

    assert!(
        baseline_still_needed.is_empty(),
        "these files are on the BASELINE in this test but now tie their children. Delete their \
         entries: the baseline may only shrink, and a stale entry hides the next real one.\n  {}",
        baseline_still_needed.join("\n  ")
    );
}
