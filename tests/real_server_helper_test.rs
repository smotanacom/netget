//! The guarantees `tests/helpers/real_server.rs` makes, checked without any third-party server.
//!
//! Every client's real-server evidence rests on this helper, so its three promises are worth
//! pinning directly rather than trusting them to show up as a protocol test failing for the
//! right reason:
//!
//! 1. a missing binary is an **error** naming the brew formula and the apt package, never a
//!    skip;
//! 2. a server that lost the probe-port race ("address already in use") is retried on a fresh
//!    port rather than reported as broken;
//! 3. dropping the guard kills the server — the process group, not just the pid.
//!
//! The "server" is `/bin/sh`, so this needs nothing installed and makes no LLM calls.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test real_server_helper_test

// The shared harness is compiled whole; this file uses one module of it.
#![allow(dead_code, unused_imports, unused_variables)]

mod helpers;

use helpers::real_server::{InstallHint, RealServer};
use std::time::Duration;

const HINT: InstallHint = InstallHint {
    brew: "some-formula",
    apt: "some-package",
};

#[tokio::test]
async fn a_missing_binary_fails_and_says_how_to_install_it() {
    let result = RealServer::builder("netget-no-such-server-binary", HINT)
        .start()
        .await;
    let err = match result {
        Ok(_) => panic!("a binary that does not exist must not start"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("`netget-no-such-server-binary`"),
        "the error must name the binary: {err}"
    );
    assert!(
        err.contains("brew install some-formula"),
        "the error must name the Homebrew formula: {err}"
    );
    assert!(
        err.contains("apt-get install some-package"),
        "the error must name the Ubuntu package: {err}"
    );
    assert!(
        err.contains("FAILS rather than skipping"),
        "the error must say why it is not a skip: {err}"
    );
}

/// The first attempt reports the bind race and exits; the second finds its marker file and
/// comes up. The marker lives in a directory this test owns, not in the helper's per-attempt
/// temp dir, which is fresh each time by design.
#[tokio::test]
async fn a_lost_bind_race_is_retried_on_a_fresh_port() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("second-attempt");
    let script = format!(
        "if [ -f '{m}' ]; then echo \"listening on {{port}}\"; exec sleep 60; \
         else touch '{m}'; echo 'bind: Address already in use' >&2; exit 1; fi",
        m = marker.display()
    );

    let server = RealServer::builder("/bin/sh", HINT)
        .args(["-c", script.as_str()])
        .ready_when_log_matches(r"listening on \d+")
        .without_tcp_readiness()
        .start()
        .await
        .expect("the second attempt should be ready");

    assert!(marker.exists(), "the first attempt never ran");
    assert!(
        server
            .log()
            .contains(&format!("listening on {}", server.port)),
        "the retried attempt must be handed its own probed port: {}",
        server.log()
    );
}

#[tokio::test]
async fn a_server_that_exits_for_another_reason_is_an_error_with_its_log() {
    let result = RealServer::builder("/bin/sh", HINT)
        .args(["-c", "echo 'fatal: bad config' >&2; exit 3"])
        .ready_when_log_matches("never printed")
        .start()
        .await;
    let err = match result {
        Ok(_) => panic!("a server that exits must not read as ready"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("fatal: bad config"),
        "the error must carry the server's own log: {err}"
    );
}

/// The shell forks a child `sleep` into the same process group; dropping the guard must take
/// both. Killing only the pid we hold would leave the grandchild running, which is exactly
/// nginx's master/worker shape.
#[tokio::test]
async fn dropping_the_guard_kills_the_whole_process_group() {
    let server = RealServer::builder("/bin/sh", HINT)
        .args(["-c", "sleep 60 & echo \"child $!\"; wait"])
        .ready_when_log_matches(r"child \d+")
        .without_tcp_readiness()
        .start()
        .await
        .expect("the shell should start");

    let grandchild: i32 = server
        .log()
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .expect("the shell printed its child's pid");

    drop(server);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // Signal 0 probes for existence. ESRCH means gone; a zombie still answers, but a
        // SIGKILLed grandchild is reparented to init and reaped almost immediately.
        let alive = unsafe { libc::kill(grandchild, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pid {grandchild} (a grandchild in the server's process group) survived the drop"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
