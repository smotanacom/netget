//! A server does not die because nobody is reading its stdout.
//!
//! # The defect
//!
//! `println!` panics when the underlying write fails. NetGet's non-interactive runner printed
//! every `[STATUS]` line with it, from the main task, so `EPIPE` on stdout unwound out of
//! `run_server` and took the process with it. A closed downstream pipe is completely ordinary —
//! `netget "serve …" | head -1`, a terminal that went away, a supervisor that stopped reading —
//! and nothing about the *service* has failed when it happens. The failure was also arbitrary:
//! a chatty instance died within a second and a quiet one never noticed, so what it actually
//! did was kill the servers that were doing work.
//!
//! # Why it is fixed rather than kept
//!
//! It was load-bearing by accident. Orphaned test binaries — 78 of them were found alive on one
//! machine — accumulated more slowly than they might have because a netget whose reader had
//! gone away usually killed itself. That is a real property and it is now provided properly, by
//! `tests/helpers/child_guard.rs`'s OS-level death tie, which covers the quiet case too and does
//! not depend on the child being chatty enough to notice. Exiting on a closed stdout is a
//! defensible *policy*; implementing it as a panic from a diagnostic line, on whichever task
//! happens to print next, is not — a panic in the spawned status forwarder is swallowed and the
//! process lives, while the same write on the main task kills it.
//!
//! # The shape of the test
//!
//! Read stdout until the server announces its port — proving the pipe worked and finding the
//! port — then **close the read end** and make the server chatty by connecting to it. Before the
//! fix the process is gone within a second or two; after it, it is still serving. The final
//! assertion is a completed TCP exchange rather than `try_wait()` alone, because a process that
//! is alive but has lost its accept loop would satisfy the weaker check.

// The shared harness, for the mock model netget refuses to start without and for the child
// reaper. A hand-rolled spawn with no backend exits within a second and the test then passes
// for entirely the wrong reason.
mod helpers;

use helpers::mock_builder::MockLlmBuilder;
use helpers::mock_ollama::MockOllamaServer;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Kills its child on drop even if an assertion unwound first.
struct Spawned(Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        if let Some(pid) = Some(self.0.id()) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            helpers::child_guard::untie_child(pid);
        }
    }
}

/// Pull the `127.0.0.1:<port>` out of a status line, if it has one.
fn port_in(line: &str) -> Option<u16> {
    let rest = line.split("127.0.0.1:").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_stdout_pipe_does_not_kill_a_running_server(
) -> Result<(), Box<dyn std::error::Error>> {
    helpers::child_guard::sweep_orphaned_netget_once();

    // The model opens a TCP server and echoes; every connection then produces status lines,
    // which is what has to reach a pipe nobody is reading.
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_instruction_containing("broken pipe")
            .respond_with_actions(serde_json::json!([{
                "type": "open_server",
                "port": 0,
                "base_stack": "TCP",
                "instruction": "Echo whatever arrives"
            }]))
            .expect_at_least(1)
            .and()
            .on_event("tcp_data_received")
            .respond_with_actions(serde_json::json!([{
                "type": "send_tcp_data",
                "data": "pong"
            }]))
            .expect_at_least(0)
            .and()
            .build(),
    )
    .await?;

    let binary = helpers::common::get_netget_binary_path()?;
    let mut child = Command::new(&binary)
        .arg("--model")
        .arg("qwen3.8:27b-mlx")
        .arg("--log-level")
        .arg("info")
        .arg("--listen-addr")
        .arg("127.0.0.1")
        .arg("--ollama-url")
        .arg(mock.base_url())
        .arg("--llm-max-concurrent")
        .arg("1000")
        .arg("Serve a TCP echo server for the broken pipe test")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    helpers::child_guard::tie_child(child.id());
    let stdout = child.stdout.take().expect("piped stdout");
    let mut child = Spawned(child);

    // Read until the server announces where it is listening. This is also the proof that the
    // pipe was working before it was closed — otherwise the assertions below would hold for a
    // process that never wrote anything at all.
    let port = tokio::task::spawn_blocking(move || -> Result<u16, String> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            if Instant::now() >= deadline {
                return Err("netget never announced a listening port on stdout".to_string());
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return Err("netget closed stdout before announcing a port".to_string()),
                Ok(_) => {}
                Err(e) => return Err(format!("reading netget stdout: {e}")),
            }
            if line.contains("listening on") {
                if let Some(port) = port_in(&line) {
                    // Dropping the reader here closes our end of the pipe, which is the whole
                    // experiment: every later write by the child gets EPIPE.
                    return Ok(port);
                }
            }
        }
        // `reader` drops here on every path, closing the read end.
    })
    .await??;

    // Make it chatty, with nobody listening. Each connection produces status lines on the main
    // task's drain loop, which is where the panic used to land.
    for _ in 0..5 {
        let mut peer = TcpStream::connect(("127.0.0.1", port)).await?;
        peer.write_all(b"ping\n").await?;
        peer.flush().await?;
        let mut buf = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(5), peer.read(&mut buf)).await;
        drop(peer);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Give a dying process time to die. Before the fix it is gone well inside this.
    tokio::time::sleep(Duration::from_secs(3)).await;

    if let Some(status) = child.0.try_wait()? {
        return Err(format!(
            "netget exited with {status} after its stdout reader went away. A server must not \
             die because nobody is reading its diagnostics — the `[STATUS]` write path is \
             panicking on EPIPE."
        )
        .into());
    }

    // Alive is not enough: assert it is still doing its job. A process that survived the panic
    // but lost its accept loop would pass the check above.
    let mut peer = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map_err(|_| "the server no longer accepts connections after its stdout pipe closed")??;
    peer.write_all(b"ping\n").await?;
    peer.flush().await?;
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(20), peer.read(&mut buf))
        .await
        .map_err(|_| "the server accepted a connection but answered nothing")??;
    assert!(
        read > 0,
        "the server closed the connection without answering; it is up but no longer serving"
    );

    Ok(())
}
