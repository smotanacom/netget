//! Detecting script runtimes must not be able to hang the process.
//!
//! `AppState::new()` calls `ScriptingEnvironment::detect()`, and every server creation and
//! every test goes through `AppState::new()`. The detection ran four `Command::output()` calls
//! — `python3 --version`, `node --version`, `go version`, `perl --version` — and
//! `Command::output()` has **no timeout**: it reads the child's stdout to EOF, so a child that
//! never exits blocks the calling thread forever.
//!
//! That is not a hypothetical. A `--test-threads=100` run of the client suite wedged
//! completely. Sampling the stuck process showed **98 of 100 threads parked** in
//! `OnceLock::get_or_init` behind one thread initialising `AppState::new`, and that thread's
//! stack was:
//!
//! ```text
//! AppState::new → new_with_options → ScriptingEnvironment::detect
//!   → detect_javascript → Command::output → read_output      ← blocked, indefinitely
//! ```
//!
//! Every "load-sensitive flake" investigated that day sat on top of this, because every one of
//! them begins by constructing an `AppState`.
//!
//! In production the same stack hangs `netget --mcp` at startup — before it can log anything —
//! whenever a runtime on the machine is wedged. A server that hangs before it starts is worse
//! than one that refuses to.
//!
//! Two properties fix it and both are asserted here: the work happens **once per process**
//! rather than once per `AppState`, and each probe is **bounded**.

use std::time::{Duration, Instant};

use netget::scripting::ScriptingEnvironment;

/// Detection is cached process-wide, so constructing many `AppState`s costs one round of
/// probes rather than four subprocesses each.
///
/// The old behaviour made a 100-thread suite spawn four hundred subprocesses to learn the same
/// four facts about the machine — facts that cannot change while the process runs.
#[test]
fn detection_is_cached_across_calls() {
    // Warm it, so the first call's real cost is not attributed to the second.
    let first = ScriptingEnvironment::detect();

    let start = Instant::now();
    for _ in 0..200 {
        let again = ScriptingEnvironment::detect();
        // Same answer every time: it is a property of the machine, not of the caller.
        assert_eq!(again.python.is_some(), first.python.is_some());
        assert_eq!(again.javascript.is_some(), first.javascript.is_some());
        assert_eq!(again.go.is_some(), first.go.is_some());
        assert_eq!(again.perl.is_some(), first.perl.is_some());
    }
    let elapsed = start.elapsed();

    // 200 cached reads cannot take as long as a single subprocess spawn. If this fails, the
    // cache is gone and every caller is paying for four `Command` spawns again.
    assert!(
        elapsed < Duration::from_millis(500),
        "200 cached detections took {elapsed:?}; the process-wide cache is not being used, so \
         every AppState::new is spawning subprocesses again"
    );
}

/// Constructing an `AppState` does not re-probe.
///
/// This is the caller that mattered: it is on the path of every test and every server
/// creation, and it is where the wedge was observed.
#[test]
fn constructing_app_state_does_not_reprobe() {
    use netget::state::app_state::AppState;

    let _warm = ScriptingEnvironment::detect();

    let start = Instant::now();
    for _ in 0..20 {
        let _state = AppState::new();
    }
    let elapsed = start.elapsed();

    // Twenty `AppState`s used to be eighty subprocess spawns. Generous bound — the point is to
    // catch a return to per-construction probing, not to measure allocation.
    assert!(
        elapsed < Duration::from_secs(5),
        "20 AppState::new took {elapsed:?}; that is subprocess-detection territory, which \
         means detection has moved back inside construction"
    );
}

/// A probe against a runtime that never exits must give up rather than block forever.
///
/// Driven through a real command rather than a mock, because the defect lived in
/// `Command::output()`'s blocking read and nothing short of a real child exercises it. `sleep`
/// is the stand-in for a wedged `node`: it ignores `--version`, produces no output, and
/// outlives any sane probe deadline.
#[test]
fn a_probe_against_a_hanging_child_gives_up() {
    let start = Instant::now();
    // `probe` is the bounded helper every detector now uses.
    let result = ScriptingEnvironment::probe("sleep", &["30"]);
    let elapsed = start.elapsed();

    assert!(
        result.is_none(),
        "a child that produced no version string must read as unavailable, not as a version"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "the probe took {elapsed:?} against a child that sleeps for 30s — it is still waiting \
         for the child to exit, which is the defect: Command::output() reads to EOF and a \
         child that never exits blocks the thread forever"
    );
}

/// A runtime that is simply absent is reported absent, quickly, and does not error.
#[test]
fn a_missing_runtime_is_absent_not_fatal() {
    let start = Instant::now();
    let result = ScriptingEnvironment::probe("netget-no-such-runtime-xyzzy", &["--version"]);

    assert!(result.is_none());
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a missing binary should fail to spawn immediately, not wait out the deadline"
    );
}
