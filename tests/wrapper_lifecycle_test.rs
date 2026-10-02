//! Real wrapper ownership, exercised with inert shell/sleep fixtures only.
#![cfg(unix)]

#[path = "helpers/wrapper_lifecycle.rs"]
mod fixture;
#[path = "e2e/netget_wrapper.rs"]
mod netget_wrapper;

use netget_wrapper::NetGetWrapper;
use std::time::Duration;

async fn start_fixture(path: std::path::PathBuf) -> (NetGetWrapper, u32) {
    let mut wrapper = NetGetWrapper::with_binary(path);
    wrapper
        .start("unused-fixture-argument", vec![])
        .await
        .unwrap();
    wrapper
        .wait_for_output("FIXTURE_PID=", Duration::from_secs(3))
        .await
        .unwrap();
    let pid = fixture::pid_from_output(&wrapper.get_output().await);
    (wrapper, pid)
}

#[test]
fn hard_killed_e2e_parent_cannot_leave_child() {
    if let Some(marker) = fixture::worker_state() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_wrapper, pid) = runtime.block_on(start_fixture(fixture::inert_script(
            marker.parent().unwrap(),
            false,
        )));
        fixture::announce_and_wait(&marker, pid);
    }
    fixture::assert_parent_death_cleans_child("hard_killed_e2e_parent_cannot_leave_child");
}

#[tokio::test]
async fn graceful_and_forced_stop_reap_the_exact_child() {
    for graceful in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (mut wrapper, pid) = start_fixture(fixture::inert_script(dir.path(), graceful)).await;
        tokio::time::timeout(Duration::from_secs(4), wrapper.stop())
            .await
            .unwrap()
            .unwrap();
        fixture::assert_reaped(pid).await;
        assert!(!wrapper.is_running());
        wrapper.stop().await.unwrap(); // Idempotent after successful teardown.
    }
}

#[tokio::test]
async fn cancelling_stop_keeps_child_owned_until_wrapper_drop() {
    let dir = tempfile::tempdir().unwrap();
    let (mut wrapper, pid) = start_fixture(fixture::inert_script(dir.path(), false)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), wrapper.stop())
            .await
            .is_err()
    );
    assert!(
        wrapper.is_running(),
        "cancelled stop must retain the child in its wrapper"
    );
    drop(wrapper);
    fixture::assert_reaped(pid).await;
}
