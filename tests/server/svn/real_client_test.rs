//! SVN against the real `svn` client (1.14.5).
//!
//! # This test is parked, and the reason is a real defect
//!
//! It is `#[ignore]`d because **the real `svn` client cannot get past its own first message**
//! against this server. That is a finding, not a gap in the test, and it is recorded here
//! because `src/server/svn/actions.rs` and `src/server/svn/CLAUDE.md` both said only that a
//! real `svn checkout` was unsupported — the truth is worse and earlier.
//!
//! ## What actually happens, measured
//!
//! 1. NetGet opens the connection by raising `svn_greeting` and writing, from
//!    `send_svn_greeting`:
//!    `( success ( 2 2 ( ANONYMOUS ) ( edit-pipeline svndiff1 ) ) )\n`
//! 2. The real client parses it and replies with its own capability tuple:
//!    `( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo
//!    log-revprops ) 26:svn://127.0.0.1:PORT/repo 35:SVN/1.14.5 (…) ( ) ) `
//!    — ending in a **space**. ra_svn frames on tuple structure and counted strings; it does
//!    **not** use newlines as message terminators, and a real client never sends one.
//! 3. `src/server/svn/mod.rs` reads commands with `BufReader::read_line`. There is no `\n`, so
//!    the read never completes; after `FIRST_COMMAND_READ_TIMEOUT` (30s) the server logs
//!    `sent nothing for 30s; closing idle connection` and hangs up.
//! 4. `svn` reports `E210002: Network connection closed unexpectedly`.
//!
//! So no `svn info`, no `svn log` and no `svn checkout` can work — and the failure is at the
//! client's *first* message, before any command is ever sent. The existing
//! `tests/server/svn/e2e_test.rs` does not catch this because it writes
//! `format!("{}\n", command)` itself: it speaks a line-oriented protocol that only NetGet
//! speaks.
//!
//! A second, independent blocker sits behind it, so fixing the framing alone is not enough: the
//! ra_svn handshake requires a server **auth-request**, then the client's
//! `( ANONYMOUS ( 33:…\n ) )` — note the `\n` *inside* a counted string, which would desync a
//! line reader all over again — then an auth success **and** a repos-info tuple. NetGet has no
//! action for any of those; they could only be faked through `send_svn_response`.
//!
//! ## Why `#[ignore]` rather than deleted, or made to assert the stall
//!
//! This repository's rule is "when a test fails, fix the implementation, not the test", and the
//! fix is a reframing of the read loop that the pass which found this was not scoped to make.
//! Asserting the stall would enshrine it and would fail the day somebody fixes it. **Nothing
//! may cite this test as evidence** — `metadata().e2e_testing` says so. Un-`#[ignore]` it when
//! the server frames ra_svn properly.

#![cfg(feature = "svn")]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
#[ignore = "KNOWN DEFECT: NetGet reads ra_svn commands with read_line, but a real svn client's \
            first message ends in a space and contains no newline, so the server stalls for 30s \
            and the client reports E210002. Un-ignore when ra_svn framing is implemented. See \
            the module doc."]
async fn test_svn_info_against_real_svn_client() -> E2EResult<()> {
    println!("\n=== E2E Test: real `svn` against NetGet's ra_svn server ===");

    match std::process::Command::new("svn")
        .arg("--version")
        .arg("--quiet")
        .output()
    {
        Ok(out) if out.status.success() => println!(
            "svn present: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        Ok(out) => {
            return Err(format!("`svn --version` exited {}", out.status).into());
        }
        Err(e) => {
            return Err(format!(
                "svn not available ({e}): this test drives the real Subversion client against \
                 NetGet's ra_svn server. Install it with `brew install subversion`."
            )
            .into())
        }
    }

    let prompt = "Listen on port {AVAILABLE_PORT} via SVN. Serve the lab repository";

    let config = NetGetConfig::new_no_scripts(prompt).with_mock(|mock| {
        mock.on_event("svn_greeting")
            .respond_with_actions(serde_json::json!([{
                "type": "send_svn_greeting",
                "min_version": 2,
                "max_version": 2,
                "mechanisms": ["ANONYMOUS"]
            }]))
            .expect_calls(1)
            .and()
            .on_instruction_containing("via SVN")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "SVN",
                    "instruction": "Serve the lab repository"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;
    let work = TempDir::new()?;

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("svn")
            .arg("info")
            .arg(format!("svn://127.0.0.1:{}/lab", server.port))
            .arg("--non-interactive")
            .arg("--config-dir")
            .arg(work.path())
            .output(),
    )
    .await
    .map_err(|_| "svn did not finish within 120s")??;

    let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
    out.push_str(&String::from_utf8_lossy(&output.stderr));
    println!("--- svn output ---\n{out}\n--- end ---");

    assert!(
        output.status.success(),
        "the real svn client could not complete `svn info` (exit {}):\n{out}",
        output.status
    );
    assert!(
        out.contains("Revision:"),
        "svn did not print repository information:\n{out}"
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
