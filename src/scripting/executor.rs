//! Script execution engine
//!
//! See `src/scripting/CLAUDE.md` for the execution model, the input/output
//! contract, and — importantly — the trust boundary: scripts are **not**
//! sandboxed and run with the full privileges of the netget process.
//!
//! # Async vs sync
//!
//! [`execute_script_async`] is the entry point that should be used from every
//! async context (all protocol servers, event handlers, MCP). It is built on
//! [`tokio::process`] so no OS thread is parked while a script runs, and it
//! writes stdin concurrently with draining stdout/stderr so a script that
//! produces output before consuming its input cannot deadlock.
//!
//! [`execute_script`] is a blocking convenience wrapper retained for
//! synchronous callers (tests, tooling). It runs the async path on a dedicated
//! thread with its own current-thread runtime, so it never nests runtimes, but
//! it *does* block the calling thread and must not be used from async code.

use super::types::{
    parse_script_response, ScriptConfig, ScriptInput, ScriptLanguage, ScriptResponse,
};
use anyhow::{Context as AnyhowContext, Result};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tracing::{debug, error, trace, warn};

/// Default timeout for script execution (30 seconds)
///
/// Scripts must complete within this time limit or they will be terminated.
/// This value is also communicated to the LLM in prompts.
pub const SCRIPT_TIMEOUT_SECS: u64 = 30;

/// Default wall-clock budget for a single script invocation.
///
/// The budget covers the *entire* interaction: spawning the interpreter,
/// writing the event JSON to stdin, draining stdout/stderr, and the child
/// exiting. There is no un-timed phase.
pub const DEFAULT_SCRIPT_TIMEOUT: Duration = Duration::from_secs(SCRIPT_TIMEOUT_SECS);

/// How long to wait for a killed child to actually be reaped before giving up.
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Monotonic counter used to give concurrently executing Go scripts distinct
/// temporary file names (the process id alone is not unique per invocation).
static SCRIPT_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Execute a script with the given input (async, non-blocking).
///
/// This is the entry point for all async callers. It never parks an OS thread:
/// the child process is spawned with [`tokio::process`] and awaited with
/// [`tokio::time::timeout`].
///
/// # Arguments
/// * `config` - Script configuration (language, source, etc.)
/// * `input` - Structured input to send to the script
///
/// # Returns
/// * `Ok(ScriptResponse)` - Parsed response from the script
/// * `Err(_)` - If execution failed, timed out, or the response was invalid
///
/// # Errors
/// This function will return an error if:
/// - The script code cannot be loaded
/// - The interpreter is not installed / not on `PATH`
/// - The script execution exceeds the timeout (the child is killed and reaped)
/// - The script returns invalid JSON
/// - The script exits with non-zero status
pub async fn execute_script_async(
    config: &ScriptConfig,
    input: &ScriptInput,
) -> Result<ScriptResponse> {
    execute_script_with_timeout_async(config, input, DEFAULT_SCRIPT_TIMEOUT).await
}

/// Execute a script with an explicit timeout (async, non-blocking).
///
/// Same as [`execute_script_async`] but with a caller-supplied wall-clock
/// budget. Primarily useful for tests and for callers that need a tighter
/// bound than [`DEFAULT_SCRIPT_TIMEOUT`].
pub async fn execute_script_with_timeout_async(
    config: &ScriptConfig,
    input: &ScriptInput,
    timeout: Duration,
) -> Result<ScriptResponse> {
    // Get the script code
    let code = config
        .source
        .get_code()
        .context("Failed to load script code")?;

    // Serialize input to JSON (pretty-printed for logs)
    let input_json = serde_json::to_string(&input).context("Failed to serialize input to JSON")?;

    debug!(
        "Executing {} script for context '{}' (timeout: {}s, input: {} bytes)",
        config.language.as_str(),
        input.event_type_id,
        timeout.as_secs(),
        input_json.len()
    );

    if tracing::enabled!(tracing::Level::TRACE) {
        let input_json_pretty =
            serde_json::to_string_pretty(&input).unwrap_or_else(|_| input_json.clone());
        trace!("─────────────────────────────────────────────");
        trace!("SCRIPT EXECUTION START");
        trace!("Language: {}", config.language.as_str());
        trace!("Context: {}", input.event_type_id);
        trace!("Handles: {:?}", config.handles_contexts);
        trace!("");
        trace!("Script code:");
        trace!("{}", code);
        trace!("");
        trace!("Script input (JSON):");
        trace!("{}", input_json_pretty);
        trace!("─────────────────────────────────────────────");
    }

    // Execute the script based on language
    let (output, stderr) = match config.language {
        ScriptLanguage::Python => execute_python(&code, &input_json, timeout).await?,
        ScriptLanguage::JavaScript => execute_javascript(&code, &input_json, timeout).await?,
        ScriptLanguage::Go => execute_go(&code, &input_json, timeout).await?,
        ScriptLanguage::Perl => execute_perl(&code, &input_json, timeout).await?,
    };

    if tracing::enabled!(tracing::Level::TRACE) {
        trace!("─────────────────────────────────────────────");
        trace!("SCRIPT EXECUTION COMPLETE");
        trace!("");
        trace!("Script stdout:");
        trace!("{}", output);
        if !stderr.is_empty() {
            trace!("");
            trace!("Script stderr:");
            trace!("{}", stderr);
        }
        trace!("─────────────────────────────────────────────");
    }

    // Parse the response
    let response = parse_script_response(&output).with_context(|| {
        format!(
            "Failed to parse script response as JSON object with actions array. Output was: {}",
            output
        )
    })?;

    debug!(
        "Script executed successfully: {} actions",
        response.actions.len()
    );

    Ok(response)
}

/// Execute a script with the given input (blocking).
///
/// **Do not call this from async code** — it blocks the calling thread for the
/// duration of the script. Use [`execute_script_async`] instead.
///
/// The work is performed on a dedicated thread running its own current-thread
/// tokio runtime, so this is safe to call even when a runtime already exists on
/// the current thread (it will not panic with "cannot start a runtime from
/// within a runtime"), but it will still block that thread.
pub fn execute_script(config: &ScriptConfig, input: &ScriptInput) -> Result<ScriptResponse> {
    execute_script_blocking_with_timeout(config, input, DEFAULT_SCRIPT_TIMEOUT)
}

/// Blocking variant of [`execute_script_with_timeout_async`].
///
/// See [`execute_script`] for the caveats.
pub fn execute_script_blocking_with_timeout(
    config: &ScriptConfig,
    input: &ScriptInput,
    timeout: Duration,
) -> Result<ScriptResponse> {
    let config = config.clone();
    let input = input.clone();

    std::thread::Builder::new()
        .name("netget-script-sync".to_string())
        .spawn(move || -> Result<ScriptResponse> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("Failed to build runtime for blocking script execution")?;
            runtime.block_on(execute_script_with_timeout_async(&config, &input, timeout))
        })
        .context("Failed to spawn thread for blocking script execution")?
        .join()
        .map_err(|_| anyhow::anyhow!("Script execution thread panicked"))?
}

/// Execute Python script with stdin/stdout
///
/// Returns (stdout, stderr) tuple
async fn execute_python(
    code: &str,
    input_json: &str,
    timeout: Duration,
) -> Result<(String, String)> {
    execute_with_command(
        ScriptLanguage::Python,
        "python3",
        &["-c"],
        Some(code),
        input_json,
        timeout,
    )
    .await
}

/// Execute JavaScript (Node.js) script with stdin/stdout
///
/// Returns (stdout, stderr) tuple
async fn execute_javascript(
    code: &str,
    input_json: &str,
    timeout: Duration,
) -> Result<(String, String)> {
    // For Node.js, we need to read from stdin in the script
    // Wrap the code to automatically read stdin
    let wrapped_code = format!(
        r#"
const fs = require('fs');
const inputJson = fs.readFileSync(0, 'utf-8');
const input = JSON.parse(inputJson);

// User's script begins here
(function() {{
{}
}})();
"#,
        code
    );

    execute_with_command(
        ScriptLanguage::JavaScript,
        "node",
        &["-e"],
        Some(&wrapped_code),
        input_json,
        timeout,
    )
    .await
}

/// Execute Perl script with stdin/stdout
///
/// Returns (stdout, stderr) tuple
async fn execute_perl(code: &str, input_json: &str, timeout: Duration) -> Result<(String, String)> {
    execute_with_command(
        ScriptLanguage::Perl,
        "perl",
        &["-e"],
        Some(code),
        input_json,
        timeout,
    )
    .await
}

/// Execute Go script with stdin/stdout
///
/// Go requires a file, so we create a temporary .go file and use `go run`
/// Returns (stdout, stderr) tuple
async fn execute_go(code: &str, input_json: &str, timeout: Duration) -> Result<(String, String)> {
    // Create a temporary file. The name must be unique *per invocation*, not
    // per process: several Go scripts can run concurrently inside netget.
    let temp_dir = std::env::temp_dir();
    let script_name = format!(
        "netget_script_{}_{}.go",
        crate::utils::clock::process_id(),
        SCRIPT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let script_path = temp_dir.join(script_name);

    // Wrap the user's code in a complete Go program
    let wrapped_code = format!(
        r#"package main

import (
    "encoding/json"
    "fmt"
    "io"
    "os"
)

func main() {{
    // Read JSON input from stdin
    inputBytes, err := io.ReadAll(os.Stdin)
    if err != nil {{
        fmt.Fprintf(os.Stderr, "Error reading stdin: %v\n", err)
        os.Exit(1)
    }}

    var input map[string]interface{{}}
    if err := json.Unmarshal(inputBytes, &input); err != nil {{
        fmt.Fprintf(os.Stderr, "Error parsing JSON: %v\n", err)
        os.Exit(1)
    }}

    // User's script begins here
    _ = input // Make input available to user code
    {{
{}
    }}
}}
"#,
        code
    );

    // Write the script to the temp file
    tokio::fs::write(&script_path, wrapped_code.as_bytes())
        .await
        .with_context(|| format!("Failed to write Go script to {:?}", script_path))?;

    // Execute with `go run <path>`
    let script_path_str = script_path.to_string_lossy().to_string();
    let result = execute_with_command(
        ScriptLanguage::Go,
        "go",
        &["run", &script_path_str],
        None,
        input_json,
        timeout,
    )
    .await;

    // Clean up the temp file
    let _ = tokio::fs::remove_file(&script_path).await;

    result
}

/// Generic async command executor.
///
/// Spawns `command args... [code]`, then concurrently:
/// - writes `input_json` to the child's stdin and closes it,
/// - drains the child's stdout,
/// - drains the child's stderr,
/// - waits for the child to exit.
///
/// The whole interaction runs under a single [`tokio::time::timeout`], so there
/// is no phase (in particular, no stdin write) that can hang unbounded. On
/// timeout the child is killed *and reaped* so it cannot linger as a zombie.
///
/// Returns (stdout, stderr) tuple.
async fn execute_with_command(
    language: ScriptLanguage,
    command: &str,
    args: &[&str],
    code: Option<&str>,
    input_json: &str,
    timeout: Duration,
) -> Result<(String, String)> {
    let mut builder = Command::new(command);
    builder.args(args);
    if let Some(code) = code {
        builder.arg(code);
    }
    builder
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If this future is dropped (e.g. the connection task is cancelled),
        // make sure we do not leak the interpreter process.
        .kill_on_drop(true);

    let mut child = builder
        .spawn()
        .map_err(|e| spawn_error(language, command, e))?;

    let mut stdin = child.stdin.take();
    let mut stdout = child
        .stdout
        .take()
        .context("Failed to capture script stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Failed to capture script stderr")?;

    let input_bytes = input_json.as_bytes().to_vec();

    // Write stdin concurrently with draining stdout/stderr. Doing the write
    // first (as the old synchronous implementation did) deadlocks whenever the
    // payload exceeds the pipe buffer while the child is itself blocked writing
    // output.
    let write_fut = async move {
        if let Some(mut handle) = stdin.take() {
            handle.write_all(&input_bytes).await?;
            handle.flush().await?;
            // Dropping the handle closes the pipe, giving the script EOF.
            drop(handle);
        }
        Ok::<(), std::io::Error>(())
    };
    let stdout_fut = async move {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    };
    let stderr_fut = async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    };

    let interaction = async { tokio::join!(write_fut, stdout_fut, stderr_fut, child.wait()) };

    let joined = tokio::time::timeout(timeout, interaction).await;

    let (write_result, stdout_result, stderr_result, status_result) = match joined {
        Ok(results) => results,
        Err(_elapsed) => {
            // Kill the child and *await* its exit so it is reaped rather than
            // left as a zombie.
            warn!(
                "{} script exceeded {:?} timeout, killing interpreter process",
                language.as_str(),
                timeout
            );
            if let Err(e) = child.start_kill() {
                warn!("Failed to signal script process for termination: {}", e);
            }
            reap(&mut child).await;
            anyhow::bail!(
                "{} script execution timed out after {:?} (process killed)",
                language.as_str(),
                timeout
            );
        }
    };

    // A script that never reads stdin (or exits early) gives us EPIPE. That is
    // not an error from our side — the child's exit status is what matters.
    if let Err(e) = write_result {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            debug!(
                "{} script closed stdin before the full input was written (script did not read all input)",
                language.as_str()
            );
        } else {
            return Err(e).context("Failed to write event JSON to script stdin");
        }
    }

    let stdout_bytes = stdout_result.context("Failed to read script stdout")?;
    let stderr_bytes = stderr_result.context("Failed to read script stderr")?;
    let status = status_result.context("Failed to wait for script process")?;

    let stdout = String::from_utf8(stdout_bytes).context("Script stdout is not valid UTF-8")?;
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

    // Check exit status
    if !status.success() {
        error!(
            "{} script execution failed with exit code {:?}",
            language.as_str(),
            status.code()
        );
        error!("Script stderr: {}", stderr);
        anyhow::bail!(
            "{} script exited with non-zero status. stderr: {}",
            language.as_str(),
            stderr
        );
    }

    // Log warnings if stderr is present
    if !stderr.is_empty() {
        warn!("Script produced stderr output (but succeeded): {}", stderr);
    }

    Ok((stdout.trim().to_string(), stderr))
}

/// Wait for a killed child to be reaped, bounded so a wedged process cannot
/// stall the caller forever.
async fn reap(child: &mut Child) {
    match tokio::time::timeout(KILL_REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            debug!("Killed script process reaped (status: {:?})", status);
        }
        Ok(Err(e)) => {
            warn!("Failed to reap killed script process: {}", e);
        }
        Err(_) => {
            warn!(
                "Killed script process did not exit within {:?}; it may linger",
                KILL_REAP_TIMEOUT
            );
        }
    }
}

/// The executable each language needs, and how to get it.
///
/// Used to turn an opaque "failed to spawn" into an actionable message.
fn interpreter_requirement(language: ScriptLanguage) -> (&'static str, &'static str) {
    match language {
        ScriptLanguage::Python => (
            "Python 3",
            "install Python 3 (e.g. `brew install python3` or `apt install python3`)",
        ),
        ScriptLanguage::JavaScript => (
            "Node.js",
            "install Node.js (e.g. `brew install node` or `apt install nodejs`)",
        ),
        ScriptLanguage::Go => (
            "the Go toolchain",
            "install Go (e.g. `brew install go` or see https://go.dev/dl/)",
        ),
        ScriptLanguage::Perl => (
            "Perl",
            "install Perl (e.g. `brew install perl` or `apt install perl`)",
        ),
    }
}

/// Build an actionable error for a failed interpreter spawn.
fn spawn_error(language: ScriptLanguage, command: &str, err: std::io::Error) -> anyhow::Error {
    let (requirement, remedy) = interpreter_requirement(language);

    if err.kind() == std::io::ErrorKind::NotFound {
        error!(
            "Cannot run {} script: `{}` not found on PATH",
            language.as_str(),
            command
        );
        anyhow::anyhow!(
            "Interpreter `{command}` was not found on PATH. \
             The '{lang}' script handler requires {requirement}. \
             To fix: {remedy}, or switch this event handler to a language whose \
             interpreter is installed, or to an `llm`/`static` handler.",
            command = command,
            lang = language.as_str(),
            requirement = requirement,
            remedy = remedy,
        )
    } else {
        anyhow::anyhow!(
            "Failed to start interpreter `{command}` for the '{lang}' script handler \
             (requires {requirement}): {err}",
            command = command,
            lang = language.as_str(),
            requirement = requirement,
            err = err,
        )
    }
}
