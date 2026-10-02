//! Process fixtures only: no NetGet binary, model or network endpoint.
#![cfg(unix)]
mod helpers {
    pub mod child_guard;
}
#[path = "eval/case.rs"]
mod case;
#[path = "eval/probe.rs"]
mod probe;
use std::time::Duration;

#[tokio::test]
async fn probe_drains_both_outputs_while_sending_large_stdin() {
    let script = "import sys; sys.stdout.write('o'*200000); sys.stdout.flush(); sys.stderr.write('e'*200000); sys.stderr.flush(); data=sys.stdin.buffer.read(); print('INPUT='+str(len(data)))";
    let spec = case::Probe::client("python3", &["-c", script])
        .until_exit()
        .stdin(&"x".repeat(300000));
    let result = probe::run(&spec, 0, Duration::from_secs(5)).await.unwrap();
    assert!(!result.timed_out && !result.output_truncated);
    assert!(result.stdout.ends_with("INPUT=300000\n"));
    assert_eq!(result.stderr.len(), 200000);
    assert_eq!(result.exit_code, Some(0));
}

#[tokio::test]
async fn probe_deadline_covers_a_child_that_never_reads_stdin() {
    let spec = case::Probe::client("python3", &["-c", "import time; time.sleep(60)"])
        .until_exit()
        .stdin(&"x".repeat(1024 * 1024));
    let start = std::time::Instant::now();
    let result = probe::run(&spec, 0, Duration::from_millis(150))
        .await
        .unwrap();
    assert!(result.timed_out);
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn probe_output_overflow_is_explicit_and_bounded() {
    let spec = case::Probe::client(
        "python3",
        &["-c", "import sys; sys.stdout.write('x'*2000000)"],
    )
    .until_exit();
    let result = probe::run(&spec, 0, Duration::from_secs(5)).await.unwrap();
    assert!(result.output_truncated);
    assert_eq!(result.stdout.len(), probe::MAX_CAPTURE_BYTES);
    assert!(result.stderr.contains("HARNESS:"));
}

#[tokio::test]
async fn probe_preserves_held_stdin_until_the_exchange_finishes() {
    let script = "import os,select; print('ready',flush=True); r,_,_=select.select([0],[],[],0.15); print('open' if not r else 'eof',flush=True)";
    let spec = case::Probe::generic("python3", &["-c", script])
        .until_exit()
        .stdin("");
    let result = probe::run(&spec, 0, Duration::from_secs(5)).await.unwrap();
    assert_eq!(result.stdout, "ready\nopen\n");
    assert!(!result.timed_out);
}

#[test]
fn probe_availability_requires_a_regular_executable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("client");
    std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!probe::binary_available(file.to_str().unwrap()));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(probe::binary_available(file.to_str().unwrap()));
    assert!(!probe::binary_available(dir.path().to_str().unwrap()));
}
