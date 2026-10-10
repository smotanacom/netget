//! NetGet's LPD client against LPRng 3.8.B's lpd, unchanged, started unprivileged on a free
//! port. Fails rather than skips without it: `scripts/test-peers/install-lprng.sh` installs it
//! and writes the /etc/lprng configuration LPRng insists on, with a "netget" queue spooling to
//! NETGET_LPRNG_SPOOL. The printed document is asserted from LPRng's own spool directory.
use super::session_test::{start, wait_log};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

/// LPRng's queue server calls setsid(), so no process-group kill reaches it, and while it
/// lives it holds the spool's lpd.lock.* and every later lpd refuses to start. It records its
/// PID in that lock file; kill exactly that process (checked to be an lpd), before the test
/// in case an earlier run died, and on drop.
struct LockHolder(PathBuf);
impl LockHolder {
    fn kill(&self) {
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if !entry.file_name().to_string_lossy().starts_with("lpd.lock") {
                continue;
            }
            let Ok(pid) = std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .trim()
                .parse::<i32>()
            else {
                continue;
            };
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if String::from_utf8_lossy(&cmdline).starts_with("lpd") {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
}
impl Drop for LockHolder {
    fn drop(&mut self) {
        self.kill();
    }
}

const HINT: InstallHint = InstallHint {
    brew: "lprng (no formula: use scripts/test-peers/install-lprng.sh on Linux)",
    apt: "lprng, configured by scripts/test-peers/install-lprng.sh",
};

#[tokio::test]
async fn netget_prints_to_lprng_lpd() {
    let spool = PathBuf::from(
        std::env::var("NETGET_LPRNG_SPOOL").unwrap_or_else(|_| "/var/spool/lpd/netget".into()),
    );
    assert!(
        spool.is_dir(),
        "LPRng spool {} missing: run scripts/test-peers/install-lprng.sh",
        spool.display()
    );
    let holder = LockHolder(spool.clone());
    holder.kill();
    let binary = std::env::var("NETGET_LPRNG_ROOT")
        .map(|root| format!("{root}/usr/sbin/lpd"))
        .unwrap_or_else(|_| "lpd".into());
    // LPRng forks a queue server that outlives its parent; RealServer kills the whole group.
    let daemon = RealServer::builder(&binary, HINT)
        .args(["-F", "-p", "{port}", "-P", "off"])
        .start()
        .await
        .unwrap_or_else(|e| panic!("LPRng lpd: {e}"));
    let port = daemon
        .addr()
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let marker = format!("netget-{}", uuid::Uuid::new_v4());
    let (state, id) = start(
        format!("127.0.0.1:{port}"),
        json!({"host":"netgethost","user":"alice"}),
        vec![
            json!({"event_pattern":"lpd_ready","handler":{"type":"static","actions":[{"type":"lpd_print","queue":"netget","text":format!("{marker}\nsecond line\n"),"job_name":marker}]}}),
            json!({"event_pattern":"lpd_print_result","handler":{"type":"static","actions":[{"type":"lpd_queue","queue":"netget","long":true}]}}),
            json!({"event_pattern":"lpd_reply","handler":{"type":"static","actions":[]}}),
        ],
    )
    .await;
    let result = wait_log(&state, id, r#""accepted":true"#).await;
    assert!(result.contains(r#""refused_at":null"#), "{result}");
    let listing = wait_log(&state, id, r#""command":"queue""#).await;
    assert!(
        listing.contains(&marker) || listing.contains("alice"),
        "LPRng lpq listing names the job: {listing}"
    );
    let spooled = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            for entry in std::fs::read_dir(&spool).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("df") {
                    let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                    if body.contains(&marker) {
                        return body;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("LPRng spooled the document");
    assert_eq!(spooled, format!("{marker}\nsecond line\n"));
    // LPRng refuses a queue it does not have, at the first acknowledgement.
    state
        .send_to_client(
            id,
            json!({"type":"lpd_print","queue":"nosuchqueue","text":"x"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    wait_log(&state, id, r#""refused_at":"queue""#).await;
    state.remove_client(id).await;
    drop(daemon);
}
