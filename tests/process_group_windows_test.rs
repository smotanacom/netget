//! Windows Job Object lifetime tests using only this test executable as a fixture.
//! No shell, interpreter, application binary, GPU, network, or model is invoked.
#![cfg(windows)]

use netget::scripting::process_io::ProcessGroup;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

const ROLE: &str = "NETGET_WINDOWS_PROCESS_FIXTURE_ROLE";
const MARKER: &str = "NETGET_WINDOWS_PROCESS_FIXTURE_MARKER";

fn fixture_command(role: &str, marker: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["process_fixture_worker", "--exact", "--test-threads=1"])
        .env(ROLE, role)
        .env(MARKER, marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    ProcessGroup::configure(&mut command);
    command
}

async fn start_owned(role: &str, marker: &Path) -> (tokio::process::Child, ProcessGroup) {
    let mut child = fixture_command(role, marker).spawn().unwrap();
    let group = ProcessGroup::new(&child).expect("attach fixture to Windows Job Object");
    // The child cannot fork before assignment: descendants must inherit this job.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"go\n")
        .await
        .unwrap();
    (child, group)
}

async fn announced_process(marker: &Path) -> OwnedHandle {
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(marker).await {
                if let Ok(pid) = text.parse::<u32>() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture did not announce its owned child");
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    assert!(
        !raw.is_null(),
        "open exact descendant PID {pid}: {}",
        std::io::Error::last_os_error()
    );
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    assert_eq!(
        unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) },
        WAIT_TIMEOUT,
        "descendant must be alive before its owner is closed"
    );
    handle
}

async fn wait_child(child: &mut tokio::process::Child) {
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("owned fixture did not terminate")
        .unwrap();
}

#[tokio::test]
async fn dropping_job_kills_child_and_descendant_but_preserves_an_independent_job() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("descendant.pid");
    let (mut sentinel, sentinel_job) = start_owned("leaf", &dir.path().join("unused")).await;
    let (mut parent, job) = start_owned("parent", &marker).await;
    let descendant = announced_process(&marker).await;
    drop(job);
    wait_child(&mut parent).await;
    assert_eq!(
        unsafe { WaitForSingleObject(descendant.as_raw_handle(), 5_000) },
        WAIT_OBJECT_0,
        "closing the job must terminate inherited descendants"
    );
    assert!(
        sentinel.try_wait().unwrap().is_none(),
        "an independent job must survive"
    );
    drop(sentinel_job);
    wait_child(&mut sentinel).await;
}

#[tokio::test]
async fn abruptly_killing_job_owner_also_kills_its_child() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("owned.pid");
    let (mut owner, safety_job) = start_owned("owner", &marker).await;
    let owned = announced_process(&marker).await;
    // TerminateProcess does not run Rust Drop. Keep the outer safety job alive
    // until after the assertion, so only the dead owner's handle closure can
    // account for its child's termination.
    owner.start_kill().unwrap();
    wait_child(&mut owner).await;
    assert_eq!(
        unsafe { WaitForSingleObject(owned.as_raw_handle(), 5_000) },
        WAIT_OBJECT_0,
        "owner death must close its kill-on-close Job Object"
    );
    drop(safety_job);
}

#[test]
fn process_fixture_worker() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let marker = std::env::var_os(MARKER).unwrap();
    let mut go = String::new();
    std::io::stdin().read_line(&mut go).unwrap();
    assert_eq!(go, "go\n");
    if role == "leaf" {
        loop {
            std::thread::park();
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut child = fixture_command("leaf", Path::new(&marker)).spawn().unwrap();
        let _job = if role == "owner" {
            Some(ProcessGroup::new(&child).expect("create nested owner Job Object"))
        } else {
            assert_eq!(role, "parent");
            None // Inherits its already-assigned parent's job.
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"go\n")
            .await
            .unwrap();
        tokio::fs::write(Path::new(&marker), child.id().unwrap().to_string())
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
}
