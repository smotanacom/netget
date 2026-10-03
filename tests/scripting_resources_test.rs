//! CPU-only resource/cancellation regressions. No model fallback is invoked.
use netget::scripting::environment::ScriptingEnvironment;
use netget::scripting::executor::execute_script_with_timeout_async;
use netget::scripting::{
    ResidentScope, ResidentScriptManager, ScriptConfig, ScriptInput, ScriptLanguage, ScriptSource,
};
use std::time::{Duration, Instant};

fn config(code: &str) -> ScriptConfig {
    ScriptConfig {
        language: ScriptLanguage::Python,
        source: ScriptSource::Inline(code.into()),
        handles_contexts: vec!["all".into()],
    }
}
fn input() -> ScriptInput {
    ScriptInput {
        event_type_id: "test".into(),
        server: Some(netget::scripting::ServerContext {
            id: 770100,
            port: 0,
            stack: "TCP".into(),
            memory: String::new(),
            instruction: String::new(),
        }),
        client: None,
        connection: None,
        event: serde_json::json!({}),
    }
}

#[test]
fn probe_caps_output_while_draining_both_pipes() {
    let before = Instant::now();
    assert!(ScriptingEnvironment::probe_with_timeout(
        "python3",
        &[
            "-c",
            "import sys; sys.stdout.write('x'*1000000); sys.stdout.flush()"
        ],
        Duration::from_secs(2)
    )
    .is_none());
    assert!(before.elapsed() < Duration::from_secs(2));
    let version = ScriptingEnvironment::probe_with_timeout(
        "python3",
        &[
            "-c",
            "import sys; sys.stderr.write('e'*50000); print('version-ok')",
        ],
        Duration::from_secs(2),
    );
    assert_eq!(version.as_deref(), Some("version-ok"));
}

#[test]
fn probe_deadline_includes_pipes_inherited_by_descendants() {
    let before = Instant::now();
    let result = ScriptingEnvironment::probe_with_timeout("python3", &["-c", "import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); print('parent exited')"], Duration::from_millis(150));
    assert!(result.is_none());
    assert!(
        before.elapsed() < Duration::from_secs(2),
        "pipe EOF must be inside the probe budget"
    );
}

#[tokio::test]
async fn per_event_output_caps_fail_before_the_script_timeout() {
    for stream in ["stdout", "stderr"] {
        let code = format!("import sys,time\nsys.{stream}.write('x'*(9*1024*1024))\nsys.{stream}.flush()\ntime.sleep(60)");
        let before = Instant::now();
        let error =
            execute_script_with_timeout_async(&config(&code), &input(), Duration::from_secs(5))
                .await
                .unwrap_err();
        assert!(format!("{error:#}").contains("cap"), "{error:#}");
        assert!(before.elapsed() < Duration::from_secs(3));
    }
}

#[tokio::test]
async fn resident_response_cap_kills_and_evicts_the_process() {
    let config = config(
        "def handle(*args):\n    return [{'type':'show_message','value':'x'*(9*1024*1024)}]",
    );
    let error = ResidentScriptManager::dispatch_with_timeout(
        &config,
        &input(),
        ResidentScope::Server,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    let removed = ResidentScriptManager::shutdown_server(770100).await;
    assert!(format!("{error:#}").contains("cap"), "{error:#}");
    assert_eq!(
        removed, 0,
        "oversized resident responses evict the broken process"
    );
}

#[tokio::test]
async fn source_loading_rejects_oversize_and_non_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.py");
    std::fs::write(&path, vec![b'x'; ScriptSource::MAX_CODE_BYTES + 1]).unwrap();
    assert!(ScriptSource::FilePath(path.to_string_lossy().into())
        .get_code_async()
        .await
        .is_err());
    assert!(ScriptSource::FilePath(dir.path().to_string_lossy().into())
        .get_code_async()
        .await
        .is_err());
    std::fs::write(&path, "print('[]')").unwrap();
    assert_eq!(
        ScriptSource::FilePath(path.to_string_lossy().into())
            .get_code_async()
            .await
            .unwrap(),
        "print('[]')"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_script_source_is_rejected_without_waiting_for_a_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.pipe");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let source = ScriptSource::FilePath(path.to_string_lossy().into());
    let result = tokio::time::timeout(Duration::from_secs(1), source.get_code_async())
        .await
        .expect("FIFO open must not block");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_terminates_interpreter_descendants() {
    use netget::scripting::process_io::ProcessGroup;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut command = tokio::process::Command::new("python3");
    command.args(["-u", "-c", "import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)']); print(p.pid); time.sleep(60)"])
        .stdout(std::process::Stdio::piped()).kill_on_drop(true);
    ProcessGroup::configure(&mut command);
    let mut child = command.spawn().unwrap();
    let guard = ProcessGroup::new(&child).unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let pid: i32 = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    drop(guard);
    child.wait().await.unwrap();
    // A just-killed orphan can briefly be a zombie before the host's init reaps it.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            let status = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&status.stdout)
                .trim()
                .starts_with('Z')
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the descendant must die with the owned process group");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_go_execution_removes_its_private_source_directory() {
    assert!(
        ScriptingEnvironment::detect().is_available(ScriptLanguage::Go),
        "this staging regression requires the local Go toolchain"
    );
    let marker = format!("netget-stage-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("wait.pipe");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let code = format!("// {marker}\nf, err := os.Open({}); if err != nil {{ panic(err) }}; defer f.Close(); fmt.Println(\"[]\")", serde_json::to_string(&fifo.to_string_lossy()).unwrap());
    let config = ScriptConfig {
        language: ScriptLanguage::Go,
        source: ScriptSource::Inline(code),
        handles_contexts: vec!["all".into()],
    };
    let task = tokio::spawn(async move {
        execute_script_with_timeout_async(&config, &input(), Duration::from_secs(30)).await
    });
    let source_directory = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for entry in std::fs::read_dir(std::env::temp_dir()).unwrap().flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("netget-script-")
                    && std::fs::read_to_string(entry.path().join("main.go"))
                        .is_ok_and(|code| code.contains(&marker))
                {
                    return entry.path();
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Go source must be staged in an identifiable private directory");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&source_directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    task.abort();
    let _ = task.await;
    assert!(
        !source_directory.exists(),
        "cancellation must remove the source and its private directory"
    );
}
