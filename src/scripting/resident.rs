//! Resident (long-lived) script handlers.
//!
//! See `src/scripting/CLAUDE.md` for the full model. In short: the default
//! script path ([`super::executor`]) spawns a fresh interpreter **per event**,
//! so a script can keep no state between events and pays interpreter start-up
//! (and, for Go, a full compile) every time. A *resident* script is spawned
//! **once per scope** and then driven with one event per line on its stdin,
//! replying with one JSON line of actions per event. Because the process stays
//! alive, module-level variables persist across events — counters, a parsed
//! config, a connection map — with no re-reading.
//!
//! Resident mode is **opt-in** (`"resident": true` on a `script` event handler);
//! the stateless per-event path is unchanged and remains the default.
//!
//! # Trust boundary
//!
//! Resident scripts run under the **same unsandboxed trust boundary** as
//! per-event scripts (see the banner in `src/scripting/CLAUDE.md`): the
//! interpreter runs as the netget user with full privileges. Resident mode adds
//! no new capability to the script itself — it only keeps the same interpreter
//! process alive between events instead of respawning it. The one operational
//! difference worth stating: a resident process outlives a single event, so it
//! is cleaned up on scope shutdown / idle eviction rather than by process exit.
//!
//! # Wire protocol (newline-delimited JSON, one round-trip per event)
//!
//! ```text
//! parent  ──►  {"event_type_id": "...", "server": {...}, "connection": {...}, "event": {...}}\n
//! child   ──►  {"actions": [ {...}, ... ]}\n
//! ```
//!
//! The child harness (built here per language) wraps the user's code, which must
//! define a `handle(event_type, event, message)` function. The harness reads a
//! line, calls `handle`, and writes exactly one JSON line back. A user `handle`
//! that raises emits `{"error": "..."}`, which the parent treats as a failed
//! event (→ caller falls back to the LLM) — distinct from a legitimate empty
//! `{"actions": []}`.
//!
//! # Robustness
//!
//! Every event round-trip runs under a [`tokio::time::timeout`]. If the resident
//! wedges on one event or its process has died (EOF on stdout), the round-trip
//! returns `Err`, the process is killed and evicted from the registry, and the
//! caller falls back to the LLM. The next event for that scope spawns a fresh
//! process. Nothing parks an OS thread: the child is spawned with
//! [`tokio::process`] and awaited asynchronously.

use super::environment::ScriptingEnvironment;
use super::types::{
    parse_script_response, ScriptConfig, ScriptInput, ScriptLanguage, ScriptResponse,
};
use crate::utils::clock::Instant;
use anyhow::{anyhow, Context as AnyhowContext, Result};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// A resident process that has not been used for this long is killed and evicted
/// on the next registry access. This bounds the lifetime of connection-scoped
/// residents even when the owning connection closes through a path that cannot
/// call [`shutdown_connection`] directly.
const IDLE_TTL: Duration = Duration::from_secs(300);

/// Scope that decides which events share one resident process (and therefore
/// share in-process state). Mirrors the scheduled-task scopes in the root
/// `CLAUDE.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentScope {
    /// One process per server: every connection's events for this handler share
    /// the same process and state (e.g. a server-wide counter).
    Server,
    /// One process per connection: each connection gets an independent process
    /// and independent state. Falls back to server scope for connectionless
    /// events (no connection id to key on).
    Connection,
}

impl ResidentScope {
    /// Parse a scope string (`"server"` / `"connection"`); anything else, or
    /// `None`, defaults to [`ResidentScope::Server`].
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("connection") | Some("conn") => ResidentScope::Connection,
            _ => ResidentScope::Server,
        }
    }
}

/// Is this language supported in resident mode?
///
/// Python, JavaScript and Perl run a persistent stdin read-loop cheaply. Go is
/// **not** supported: it is compiled per `go run` invocation and has no cheap
/// persistent-interpreter form, so a resident Go handler transparently falls
/// back to the per-event executor.
pub fn resident_language_supported(language: ScriptLanguage) -> bool {
    matches!(
        language,
        ScriptLanguage::Python | ScriptLanguage::JavaScript | ScriptLanguage::Perl
    )
}

/// Key identifying one resident process. Two handlers with identical code and
/// scope share a process; different code (or a different connection under
/// connection scope) gets its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ScopeKey {
    /// Server or client id, per `owner_is_client`. Ids are unified process-wide
    /// (one counter allocates both), but the discriminator keeps ownership
    /// explicit rather than resting on that invariant.
    owner_id: u32,
    owner_is_client: bool,
    /// `Some` only under connection scope with a connection present.
    connection_id: Option<String>,
    language: ScriptLanguage,
    code_hash: u64,
}

impl ScopeKey {
    fn new(
        scope: ResidentScope,
        input: &ScriptInput,
        language: ScriptLanguage,
        code: &str,
    ) -> Self {
        let connection_id = match scope {
            ResidentScope::Connection => input.connection.as_ref().map(|c| c.id.clone()),
            ResidentScope::Server => None,
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        language.as_str().hash(&mut hasher);
        code.hash(&mut hasher);
        // For clients, `Server` and `Connection` scope coincide (one connection
        // per client) — both key on the client id alone.
        let (owner_id, owner_is_client) = match (&input.server, &input.client) {
            (Some(server), _) => (server.id, false),
            (None, Some(client)) => (client.id, true),
            (None, None) => (0, false),
        };
        Self {
            owner_id,
            owner_is_client,
            connection_id,
            language,
            code_hash: hasher.finish(),
        }
    }

    fn describe(&self) -> String {
        let owner = if self.owner_is_client {
            format!("client #{}", self.owner_id)
        } else {
            format!("server #{}", self.owner_id)
        };
        match &self.connection_id {
            Some(c) => format!("{} conn {} [{}]", owner, c, self.language.as_str()),
            None => format!("{} [{}]", owner, self.language.as_str()),
        }
    }
}

/// The live pipes of a resident process. `None` in [`ResidentScript::io`] once
/// the process is dead so a later round-trip fails fast instead of writing to a
/// broken pipe.
struct ResidentIo {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// A single resident process. Access to its pipes is serialized by the `io`
/// mutex — one event round-trip at a time per process, which is required (two
/// concurrent writers on one stdin would interleave) and mirrors the
/// per-connection state machine used elsewhere.
pub struct ResidentScript {
    language: ScriptLanguage,
    io: Mutex<Option<ResidentIo>>,
    describe: String,
}

impl ResidentScript {
    /// Spawn the interpreter with the wrapped resident harness. Synchronous
    /// (`.spawn()` does not await), so it is safe to call while holding the
    /// registry lock.
    fn spawn(language: ScriptLanguage, code: &str, describe: String) -> Result<Arc<Self>> {
        let (command, args, wrapped): (&str, Vec<&str>, String) = match language {
            ScriptLanguage::Python => ("python3", vec!["-u", "-c"], python_harness(code)),
            ScriptLanguage::JavaScript => ("node", vec!["-e"], javascript_harness(code)),
            ScriptLanguage::Perl => ("perl", vec!["-e"], perl_harness(code)),
            ScriptLanguage::Go => {
                anyhow::bail!("Go is not supported in resident script mode");
            }
        };

        let mut builder = Command::new(command);
        builder.args(&args);
        builder.arg(&wrapped);
        builder
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the last Arc is dropped (scope shutdown, idle eviction, task
            // cancellation) make sure the interpreter dies with it.
            .kill_on_drop(true);

        let mut child = builder
            .spawn()
            .map_err(|e| spawn_error(language, command, e))?;

        let stdin = child
            .stdin
            .take()
            .context("resident: failed to capture stdin")?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .context("resident: failed to capture stdout")?,
        );
        let stderr = child
            .stderr
            .take()
            .context("resident: failed to capture stderr")?;

        // Drain stderr in the background so the child never blocks on a full
        // stderr pipe, and so diagnostics reach the log.
        let describe_for_log = describe.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    warn!("resident script {}: {}", describe_for_log, line);
                }
            }
        });

        info!(
            "Spawned resident {} script ({})",
            language.as_str(),
            describe
        );

        Ok(Arc::new(Self {
            language,
            io: Mutex::new(Some(ResidentIo {
                child,
                stdin,
                stdout,
            })),
            describe,
        }))
    }

    /// Send one event and read one response, under `timeout`. On any failure the
    /// process is killed and marked dead (`io` set to `None`), so the caller can
    /// evict it and the next event respawns.
    async fn round_trip(&self, input: &ScriptInput, timeout: Duration) -> Result<ScriptResponse> {
        let mut guard = self.io.lock().await;
        if guard.is_none() {
            return Err(anyhow!("resident script process is not alive"));
        }

        // One newline-delimited JSON request. `to_string` (not pretty) keeps it
        // to a single line, which the child's readline-per-event loop requires.
        let mut line = serde_json::to_string(input).context("resident: serialize event")?;
        line.push('\n');

        let result: Result<String> = {
            let io = guard.as_mut().expect("checked is_some above");
            let interaction = async {
                io.stdin
                    .write_all(line.as_bytes())
                    .await
                    .context("resident: write event to stdin")?;
                io.stdin.flush().await.context("resident: flush stdin")?;
                let mut response = String::new();
                let n = io
                    .stdout
                    .read_line(&mut response)
                    .await
                    .context("resident: read response line")?;
                if n == 0 {
                    return Err(anyhow!("resident script exited (EOF on stdout)"));
                }
                Ok(response)
            };
            match tokio::time::timeout(timeout, interaction).await {
                Ok(inner) => inner,
                Err(_) => Err(anyhow!(
                    "resident script event exceeded {:?} timeout",
                    timeout
                )),
            }
        };

        match result {
            Ok(response) => {
                // A `{"error": ...}` reply (a `handle()` exception in the child)
                // has no `actions` field, so parsing fails here and the caller
                // falls back to the LLM — while the process stays alive for the
                // next event.
                parse_script_response(response.trim())
                    .with_context(|| format!("resident script {} response", self.describe))
            }
            Err(e) => {
                // Kill and mark dead so a wedged/broken process is not reused.
                if let Some(mut io) = guard.take() {
                    let _ = io.child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(2), io.child.wait()).await;
                }
                warn!("resident script {} failed: {}", self.describe, e);
                Err(e)
            }
        }
    }

    /// Kill this process now (used by scope shutdown). Idempotent.
    async fn kill(&self) {
        let mut guard = self.io.lock().await;
        if let Some(mut io) = guard.take() {
            let _ = io.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), io.child.wait()).await;
            debug!("Killed resident script {}", self.describe);
        }
    }

    /// Whether the process is still alive (pipes present). A per-event failure
    /// such as a `handle()` exception leaves the process alive; a wedge or crash
    /// marks it dead (`io == None`).
    async fn is_alive(&self) -> bool {
        self.io.lock().await.is_some()
    }

    /// Language this resident runs (for diagnostics/tests).
    pub fn language(&self) -> ScriptLanguage {
        self.language
    }
}

/// One registry slot: the shared process plus its last-use time for idle
/// eviction.
struct ResidentEntry {
    script: Arc<ResidentScript>,
    last_used: Instant,
}

/// Process-global registry of resident scripts, keyed by scope+code. A single
/// mutex guards the map; it is held only for lookup/insert/evict (no `.await`
/// across the round-trip), so it is not on the hot path the way `AppState` is.
static REGISTRY: Lazy<Mutex<HashMap<ScopeKey, ResidentEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Manager for resident scripts. All methods are static; state lives in the
/// process-global [`REGISTRY`].
pub struct ResidentScriptManager;

impl ResidentScriptManager {
    /// Dispatch one event to the resident process for `(scope, code)`, spawning
    /// it on first use. Returns the script's actions, or `Err` if the process
    /// could not be reached (dead, wedged, or unsupported language) — in which
    /// case the caller should fall back to the LLM.
    ///
    /// The default budget is [`super::executor::DEFAULT_SCRIPT_TIMEOUT`]; use
    /// [`dispatch_with_timeout`](Self::dispatch_with_timeout) for a custom one.
    pub async fn dispatch(
        config: &ScriptConfig,
        input: &ScriptInput,
        scope: ResidentScope,
    ) -> Result<ScriptResponse> {
        Self::dispatch_with_timeout(
            config,
            input,
            scope,
            super::executor::DEFAULT_SCRIPT_TIMEOUT,
        )
        .await
    }

    /// [`dispatch`](Self::dispatch) with an explicit per-event timeout.
    pub async fn dispatch_with_timeout(
        config: &ScriptConfig,
        input: &ScriptInput,
        scope: ResidentScope,
        timeout: Duration,
    ) -> Result<ScriptResponse> {
        if !resident_language_supported(config.language) {
            anyhow::bail!(
                "language '{}' is not supported in resident mode",
                config.language.as_str()
            );
        }
        let code = config
            .source
            .get_code()
            .context("resident: failed to load script code")?;
        let key = ScopeKey::new(scope, input, config.language, &code);

        // Acquire (or spawn) the resident process. The registry lock is held only
        // for this block — spawning does not await — and dropped before the
        // round-trip, so a slow event never blocks other scopes' lookups.
        let script = {
            let mut registry = REGISTRY.lock().await;
            evict_idle(&mut registry);
            match registry.get_mut(&key) {
                Some(entry) => {
                    entry.last_used = Instant::now();
                    entry.script.clone()
                }
                None => {
                    let script = ResidentScript::spawn(config.language, &code, key.describe())?;
                    registry.insert(
                        key.clone(),
                        ResidentEntry {
                            script: script.clone(),
                            last_used: Instant::now(),
                        },
                    );
                    script
                }
            }
        };

        match script.round_trip(input, timeout).await {
            Ok(response) => {
                debug!(
                    "resident script {} handled '{}' ({} actions)",
                    key.describe(),
                    input.event_type_id,
                    response.actions.len()
                );
                Ok(response)
            }
            Err(e) => {
                // Distinguish a dead/wedged process (evict so the next event
                // respawns) from a live process that merely failed this one event
                // — a `handle()` exception, or an unparseable reply. The latter
                // must stay resident so its in-process state survives; only the
                // failed event is deferred to the LLM.
                if !script.is_alive().await {
                    let mut registry = REGISTRY.lock().await;
                    if let Some(entry) = registry.get(&key) {
                        if Arc::ptr_eq(&entry.script, &script) {
                            registry.remove(&key);
                        }
                    }
                }
                Err(e)
            }
        }
    }

    /// Kill and evict every resident process for a server (all scopes,
    /// connection- and server-scoped). Call from the server-close path. Returns
    /// how many processes were shut down.
    pub async fn shutdown_server(server_id: u32) -> usize {
        let count = Self::shutdown_owner(server_id, false).await;
        if count > 0 {
            info!(
                "Shut down {} resident script(s) for server #{}",
                count, server_id
            );
        }
        count
    }

    /// Kill and evict every resident process for a client. Call from the
    /// client-removal path. Returns how many processes were shut down.
    pub async fn shutdown_client(client_id: u32) -> usize {
        let count = Self::shutdown_owner(client_id, true).await;
        if count > 0 {
            info!(
                "Shut down {} resident script(s) for client #{}",
                count, client_id
            );
        }
        count
    }

    async fn shutdown_owner(owner_id: u32, owner_is_client: bool) -> usize {
        let scripts = {
            let mut registry = REGISTRY.lock().await;
            let keys: Vec<ScopeKey> = registry
                .keys()
                .filter(|k| k.owner_id == owner_id && k.owner_is_client == owner_is_client)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| registry.remove(&k).map(|e| e.script))
                .collect::<Vec<_>>()
        };
        let count = scripts.len();
        for script in scripts {
            script.kill().await;
        }
        count
    }

    /// Kill and evict the connection-scoped resident(s) for one connection.
    pub async fn shutdown_connection(server_id: u32, connection_id: &str) -> usize {
        let scripts = {
            let mut registry = REGISTRY.lock().await;
            let keys: Vec<ScopeKey> = registry
                .keys()
                .filter(|k| {
                    k.owner_id == server_id
                        && !k.owner_is_client
                        && k.connection_id.as_deref() == Some(connection_id)
                })
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| registry.remove(&k).map(|e| e.script))
                .collect::<Vec<_>>()
        };
        let count = scripts.len();
        for script in scripts {
            script.kill().await;
        }
        count
    }

    /// Kill and evict every resident process (all servers). Returns the count.
    pub async fn shutdown_all() -> usize {
        let scripts = {
            let mut registry = REGISTRY.lock().await;
            registry.drain().map(|(_, e)| e.script).collect::<Vec<_>>()
        };
        let count = scripts.len();
        for script in scripts {
            script.kill().await;
        }
        count
    }

    /// Number of live resident processes. Intended for tests and diagnostics.
    pub async fn active_count() -> usize {
        REGISTRY.lock().await.len()
    }
}

/// Remove (and, via drop + `kill_on_drop`, kill) entries idle longer than
/// [`IDLE_TTL`]. Called under the registry lock on every dispatch.
fn evict_idle(registry: &mut HashMap<ScopeKey, ResidentEntry>) {
    let now = Instant::now();
    let stale: Vec<ScopeKey> = registry
        .iter()
        .filter(|(_, e)| now.duration_since(e.last_used) > IDLE_TTL)
        .map(|(k, _)| k.clone())
        .collect();
    for key in stale {
        debug!("Evicting idle resident script {}", key.describe());
        registry.remove(&key);
    }
}

/// Whether the interpreter for a resident language is installed. Mirrors the
/// per-event executor's availability check so a resident handler for a missing
/// interpreter falls back to the LLM rather than erroring per event.
pub fn resident_available(env: &ScriptingEnvironment, language: ScriptLanguage) -> bool {
    resident_language_supported(language) && env.is_available(language)
}

// ---------------------------------------------------------------------------
// Harness wrappers: the persistent read-loop each language runs around the
// user's `handle(event_type, event, message)` function.
// ---------------------------------------------------------------------------

/// Python resident harness. Runs under `python3 -u` (unbuffered). The user code
/// must define `handle(event_type, event, message)`.
fn python_harness(user_code: &str) -> String {
    format!(
        r#"import sys, json, traceback

def _ng_emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

# ===== user code: must define handle(event_type, event, message) =====
{user_code}
# ===== end user code =====

for _ng_line in sys.stdin:
    _ng_line = _ng_line.strip()
    if not _ng_line:
        continue
    try:
        _ng_msg = json.loads(_ng_line)
    except Exception as _ng_err:
        sys.stderr.write("resident: could not parse input line: %s\n" % _ng_err)
        _ng_emit({{"error": "bad input line"}})
        continue
    try:
        _ng_result = handle(_ng_msg.get("event_type_id"), _ng_msg.get("event"), _ng_msg)
    except Exception:
        sys.stderr.write("resident handle() raised:\n%s" % traceback.format_exc())
        _ng_emit({{"error": "handle() raised"}})
        continue
    if isinstance(_ng_result, dict) and "actions" in _ng_result:
        _ng_emit(_ng_result)
    elif isinstance(_ng_result, list):
        _ng_emit({{"actions": _ng_result}})
    elif _ng_result is None:
        _ng_emit({{"actions": []}})
    else:
        sys.stderr.write("resident handle() returned unexpected type; expected list or dict\n")
        _ng_emit({{"actions": []}})
"#,
        user_code = user_code
    )
}

/// JavaScript (Node.js) resident harness. The user code must define
/// `function handle(event_type, event, message)`.
fn javascript_harness(user_code: &str) -> String {
    format!(
        r#"const readline = require('readline');
function _ngEmit(obj) {{
    process.stdout.write(JSON.stringify(obj) + "\n");
}}

// ===== user code: must define handle(event_type, event, message) =====
{user_code}
// ===== end user code =====

const _ngRl = readline.createInterface({{ input: process.stdin }});
_ngRl.on('line', (line) => {{
    line = line.trim();
    if (!line) return;
    let msg;
    try {{
        msg = JSON.parse(line);
    }} catch (e) {{
        process.stderr.write("resident: could not parse input line: " + e + "\n");
        _ngEmit({{ error: "bad input line" }});
        return;
    }}
    let result;
    try {{
        result = handle(msg.event_type_id, msg.event, msg);
    }} catch (e) {{
        process.stderr.write("resident handle() raised: " + (e && e.stack ? e.stack : e) + "\n");
        _ngEmit({{ error: "handle() raised" }});
        return;
    }}
    if (Array.isArray(result)) {{
        _ngEmit({{ actions: result }});
    }} else if (result && typeof result === 'object' && 'actions' in result) {{
        _ngEmit(result);
    }} else if (result === undefined || result === null) {{
        _ngEmit({{ actions: [] }});
    }} else {{
        process.stderr.write("resident handle() returned unexpected type; expected array or object\n");
        _ngEmit({{ actions: [] }});
    }}
}});
"#,
        user_code = user_code
    )
}

/// Perl resident harness. Uses `JSON::PP` (core since Perl 5.14, no CPAN
/// dependency). The user code must define `sub handle {{ my ($event_type,
/// $event, $message) = @_; ... }}` returning an array-ref or hash-ref.
fn perl_harness(user_code: &str) -> String {
    format!(
        r#"use strict;
use warnings;
use JSON::PP;
$| = 1;
my $_ng_json = JSON::PP->new->utf8->canonical;

sub _ng_emit {{
    my ($obj) = @_;
    print $_ng_json->encode($obj) . "\n";
}}

# ===== user code: must define sub handle($event_type, $event, $message) =====
{user_code}
# ===== end user code =====

while (my $_ng_line = <STDIN>) {{
    $_ng_line =~ s/^\s+|\s+$//g;
    next if $_ng_line eq '';
    my $_ng_msg = eval {{ $_ng_json->decode($_ng_line) }};
    if ($@) {{
        print STDERR "resident: could not parse input line: $@\n";
        _ng_emit({{ error => "bad input line" }});
        next;
    }}
    my $_ng_result = eval {{ handle($_ng_msg->{{event_type_id}}, $_ng_msg->{{event}}, $_ng_msg) }};
    if ($@) {{
        print STDERR "resident handle() raised: $@\n";
        _ng_emit({{ error => "handle() raised" }});
        next;
    }}
    if (ref($_ng_result) eq 'HASH' && exists $_ng_result->{{actions}}) {{
        _ng_emit($_ng_result);
    }} elsif (ref($_ng_result) eq 'ARRAY') {{
        _ng_emit({{ actions => $_ng_result }});
    }} elsif (!defined $_ng_result) {{
        _ng_emit({{ actions => [] }});
    }} else {{
        print STDERR "resident handle() returned unexpected type; expected array-ref or hash-ref\n";
        _ng_emit({{ actions => [] }});
    }}
}}
"#,
        user_code = user_code
    )
}

/// Build an actionable error for a failed interpreter spawn (mirrors the
/// per-event executor's message).
fn spawn_error(language: ScriptLanguage, command: &str, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        anyhow!(
            "Interpreter `{command}` for resident '{lang}' script was not found on PATH. \
             Install it, or switch this handler to a language whose interpreter is installed.",
            command = command,
            lang = language.as_str(),
        )
    } else {
        anyhow!(
            "Failed to start interpreter `{command}` for resident '{lang}' script: {err}",
            command = command,
            lang = language.as_str(),
            err = err,
        )
    }
}
