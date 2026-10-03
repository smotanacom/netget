//! Inert, exactly-owned process fixtures for wrapper lifecycle regressions.
#![allow(dead_code)]

use super::child_guard;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WORKER_STATE: &str = "NETGET_WRAPPER_LIFECYCLE_STATE";

pub fn worker_state() -> Option<PathBuf> {
    std::env::var_os(WORKER_STATE).map(PathBuf::from)
}

pub fn inert_script(directory: &Path, graceful: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("fixture.sh");
    let body = if graceful {
        "while IFS= read -r line; do if [ \"$line\" = exit ]; then exit 0; fi; done"
    } else {
        // The outer test's unique TempDir owns the lease, not the worker being
        // killed. A broken death tie must fail the assertion while the lease is
        // retained; unwinding then revokes it without signalling an orphan PID
        // that the kernel may have recycled. Self-expiry also bounds leftovers
        // if the outer test itself is killed before TempDir can be dropped.
        std::fs::write(path.with_extension("sh.lease"), b"owned fixture lease").unwrap();
        "remaining=600; while [ -f \"$0.lease\" ] && [ \"$remaining\" -gt 0 ]; do /bin/sleep 0.1; remaining=$((remaining-1)); done"
    };
    std::fs::write(
        &path,
        format!("#!/bin/sh\ntrap '' HUP\nprintf 'FIXTURE_PID=%s\\n' \"$$\"\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

pub fn pid_from_output(output: &str) -> u32 {
    output
        .lines()
        .find_map(|line| {
            line.strip_prefix("FIXTURE_PID=")
                .and_then(|s| s.parse().ok())
        })
        .expect("the inert child must report its process ID")
}

pub fn announce_and_wait(path: &Path, pid: u32) -> ! {
    std::fs::write(path, pid.to_string()).unwrap();
    loop {
        std::thread::park();
    }
}

pub fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

struct OwnedProcess {
    child: Child,
    tie: Option<child_guard::DeathTie>,
}

impl OwnedProcess {
    fn spawn(command: &mut Command) -> Self {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut owned = Self { child, tie: None };
        owned.tie = child_guard::arm_death_tie(owned.child.id());
        assert!(
            owned.tie.is_some(),
            "fixture must have parent-death cleanup"
        );
        owned
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(tie) = self.tie.as_mut() {
            tie.disarm();
        }
    }
}

pub fn assert_parent_death_cleans_child(test_name: &str) {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("child.pid");
    let mut sentinel = OwnedProcess::spawn(Command::new("/bin/sleep").arg("600"));
    let mut worker = OwnedProcess::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
            .env(WORKER_STATE, &marker),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(
            worker.child.try_wait().unwrap().is_none(),
            "fixture worker exited before reporting its child"
        );
        assert!(
            Instant::now() < deadline,
            "fixture worker never reported its child"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        pid_exists(pid),
        "fixture must be alive before killing its parent"
    );
    worker.child.kill().unwrap(); // SIGKILL: no destructors or handlers can run.
    worker.child.wait().unwrap();
    worker.tie.as_mut().unwrap().disarm();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !pid_exists(pid) {
            break;
        }
        // Some CI init processes retain already-dead orphan zombies briefly.
        let status = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&status.stdout)
            .trim_start()
            .starts_with('Z')
        {
            break;
        }
        if Instant::now() >= deadline {
            // TempDir cleanup revokes the fixture lease; never signal a numeric
            // orphan PID, which may now identify an unrelated process.
            panic!("owned child {pid} survived its parent's abrupt death");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        sentinel.child.try_wait().unwrap().is_none(),
        "unrelated sentinel must survive"
    );
}

pub async fn assert_reaped(pid: u32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while pid_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned child must be terminated and reaped, not left a zombie");
}
