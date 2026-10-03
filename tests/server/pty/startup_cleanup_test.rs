//! A rejected startup must return both PTY descriptors, including the master.

use netget::server::pty::PtyServer;
use netget::state::{AppState, ServerId};
use std::sync::Arc;

#[test]
fn rejected_link_path_does_not_leak_pty_descriptors() {
    // Other tests open descriptors concurrently. Re-run just this test in its own
    // process so the before/after count measures the failed startups alone.
    const CHILD_ENV: &str = "NETGET_PTY_FD_REGRESSION_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "server::pty::startup_cleanup_test::rejected_link_path_does_not_leak_pty_descriptors",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("run isolated descriptor regression");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "descriptor regression did not pass: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary directory");
        let link = temp.path().join("keep.txt");
        std::fs::write(&link, "keep this file").expect("existing regular file");
        let state = Arc::new(AppState::new_with_options(
            false,
            "http://127.0.0.1:1".into(),
        ));
        let llm = netget::llm::OllamaClient::new("http://127.0.0.1:1");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let descriptor_count = || {
            std::fs::read_dir("/dev/fd")
                .expect("descriptor directory")
                .count()
        };
        let before = descriptor_count();
        for _ in 0..16 {
            let error = PtyServer::spawn_with_llm_actions(
                Some(link.clone()),
                false,
                llm.clone(),
                state.clone(),
                tx.clone(),
                ServerId::new(1),
            )
            .await
            .expect_err("an ordinary file must not be replaced by a PTY link");
            assert!(error.to_string().contains("not a symlink"), "{error:#}");
        }
        assert_eq!(
            descriptor_count(),
            before,
            "failed startup leaked PTY descriptors"
        );
        assert_eq!(
            std::fs::read_to_string(link).expect("preserved file"),
            "keep this file"
        );
    });
}
