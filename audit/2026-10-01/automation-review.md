# Automation/MCP review — 2026-10-01

## Scope

Second review wave covered every source module under `src/mcp_stdio/` and `src/scripting/`, their local CLAUDE documentation and relevant root integration tests. The root coordinator additionally transferred the FIFO notification writer in `src/llm/agent_queue.rs` and a new dedicated regression file. No GPU, model endpoint, model download, deployment, publication or commit was run.

The review inspected startup configuration, tool validation/routing, transport EOF draining, FIFO notifications, process creation and cancellation, interpreter detection, resident process ownership, script response parsing, static interpolation and syntax highlighting. Source modules were all inventoried and read at least at interface/risk points; this is not branch-complete verification of every MCP tool/protocol combination. The root coordinator runs the full native regression pass; test execution status below intentionally distinguishes completed local checks from pending coordinated checks.

## Implemented changes and evidence

### 1. Resident request cancellation no longer leaves an unread response for the next event

Previously a resident round-trip borrowed a `ResidentIo` stored inside a mutex-protected Option. Cancelling the future released the mutex but left the interpreter alive in the registry, potentially still computing the old response. The next event could write its own request and consume the old reply as its answer.

The round-trip now takes ownership of the child and pipes from the slot for the duration of the exchange. Only a completed exchange returns them to the slot. Cancellation drops the owned child with `kill_on_drop(true)`, leaves the slot empty, and the next registry lookup replaces the dead resident. Successful responses and ordinary handler errors preserve the process and its module state as before.

Regression: `resident_cancelled_exchange_cannot_supply_the_next_events_reply`. A CPU Python script writes a local marker before sleeping; the test waits for that exact marker, cancels the first event, and demands a fresh fast event return its own identity and reset counter. The test does not sleep to guess when cancellation is safe.

### 2. Resident event deadlines include queue wait

Previously `round_trip` awaited its IO mutex before starting the supplied timeout. A request with a 50ms budget could wait behind a 30s handler and then receive another 50ms. Error cleanup also waited for that mutex again when checking whether the resident was alive.

The new deadline is established before acquiring the mutex and reused for the exchange. A timed-out queued request returns without interrupting the event already running. Dead-process checks use try_lock; a held mutex denotes an in-flight event and cannot stretch the expired request's return time.

Regression: `resident_queue_wait_is_inside_the_events_timeout`, with a marker-confirmed slow first event and a short-budget second one. All spawned test tasks/processes are cancelled or shut down before assertions that could fail.

Independent integration review caught an extreme-budget regression in the first implementation: directly adding `Duration::MAX` to an Instant can panic. The deadline now uses checked_add and returns a descriptive error when the clock cannot represent it. `resident_extreme_timeout_is_an_error_without_panicking` exercises that rejection and then proves the next normal event still runs on an untouched resident counter.

### 3. Invalid resident return values are errors

The Python, JavaScript and Perl harnesses printed a diagnostic for unsupported return values (such as scalar 42), then emitted `{"actions":[]}`. The caller therefore treated a failed handler as a successful intentional silence. All three now emit an error object, which the existing response parser rejects and routes into existing failure/fallback handling. Returning None/null/undefined still means an intentional empty response.

Regression: `resident_invalid_return_is_an_error_without_losing_process_state` drives actual Python, Node and Perl/JSON::PP interpreters; the first invalid return must fail, and the next valid event proves the same resident counter survives. Required runtimes are asserted, not silently skipped.

### 4. JavaScript resident handlers can be async

The JavaScript harness now awaits `handle(...)`. Previously a Promise was classified as an unsupported object and silently converted into no actions. Rejections now enter the existing handler-error catch path; successful async responses retain their action list.

Regression: `resident_javascript_awaits_async_handlers`, whose fixture resolves a Promise and returns a known action after a short host timer. This runs Node CPU code only.

### 5. MCP notification paths must really be FIFOs

`ensure_fifo` previously returned success for any existing path. A caller supplying a regular file could have request identifiers written over its initial bytes; a directory or symlink was also accepted despite the documented named-pipe contract. Startup now uses symlink_metadata, accepts only real FIFOs, rejects regular files/directories/symlinks with an actionable error, and preserves normal absent-path creation.

Regressions in `mcp_startup_config_test.rs`: existing files, directories and symlinks are refused while file contents remain intact; a real FIFO is created and reused successfully. Services use `--llm-agent`, so no model is contacted.

### 6. FIFO verification survives path replacement after startup

Startup checks alone do not close the race: a FIFO pathname may later be replaced. The transferred agent queue writer now opens with `O_NONBLOCK | O_NOFOLLOW`, then checks the opened descriptor's metadata is a FIFO before writing. Checking the descriptor, rather than rechecking the pathname, prevents a rename race from redirecting the write to an ordinary file.

`tests/agent_queue_fifo_test.rs` tests replacing a configured FIFO with a file and then a symlink; neither target is modified and both requests remain queued. A real FIFO still receives the exact request id. These are local filesystem/in-memory queue tests, with no network or model.

### 7. MCP HTTP binds IPv6 correctly and reports the actual port

The HTTP entry point previously formatted `host:port` directly, producing an invalid unbracketed address when `--listen-addr` was an IPv6 literal such as `::1`. It now passes the host and port as a tuple to Tokio and logs `listener.local_addr()`, which includes proper IPv6 formatting and the actual assigned port when zero was requested.

This edit is confined to the `mcp-http` feature branch. The coordinator must record whether that feature was compiled; no HTTP transport runtime test is claimed in this report.

### 8. Avoid repeated highlighter resource loading

SyntaxSet and ThemeSet are now cached with OnceLock. They previously reloaded/deserialized the complete bundled syntax/theme resources for every highlighted script. Per-call HighlightLines state remains separate, preserving parser state isolation. Existing `scripting_highlight_test` is the appropriate behavioral regression; no new implementation-mirroring test was added.

### 9. Avoid allocation in script event matching

`ScriptConfig::handles_context` now compares borrowed strings in one iterator rather than allocating `"all"` and the event type on every check. Matching semantics are unchanged; existing scripting manager tests cover dispatch selection.

### 10. Keep documentation consistent with implemented behavior

Updated scripting documentation with cancellation ownership, queue-inclusive deadlines, async JavaScript and invalid-result behavior. Updated MCP documentation with startup/open-time FIFO checks. Removed the stale tools.rs comment claiming scheduled tasks were TUI-only even though the MCP ticker already exists.

## Validation ownership/status

- Targeted `rustfmt --edition 2021` on all edited native files: passed.
- `git diff --check`: passed at this report checkpoint.
- Root-coordinated CPU targets requested: `scripting_resident_test`, `scripting_highlight_test`, `scripting_manager_test`, `mcp_startup_config_test`, `agent_queue_fifo_test`; the final consolidated report is authoritative for compiled/tested results.
- `mcp-http` feature compile requested separately for the tuple binding change.
- The resident tests use actual lightweight interpreters, not LLMs. Existing tests in the broader resident suite contain skip-when-runtime-missing gates; the newly added runtime-dependent regressions explicitly fail if their required interpreter is absent.
- No source-level success is presented as a passed runtime test. No model fallback was invoked to validate failure paths.

## Deferred, substantive findings

1. **Interpreter probe timeout does not cover every pipe-drain scenario.** environment.rs polls child exit before draining stdout/stderr. A verbose fake --version command can fill a pipe and be killed as unavailable; a child that exits while a descendant keeps a pipe open can make wait_with_output block after the nominal deadline. A robust concurrent/bounded collection helper deserves dedicated child/descendant fixtures.
2. **Script output is unbounded.** Per-event stdout/stderr use read_to_end, resident stdout uses read_line, resident stderr uses lines. A noisy trusted script can consume substantial memory inside the timeout. Limits need an explicit compatibility decision and tests for data exceeding the chosen bounds; no arbitrary output ceiling was added during stabilization.
3. **Only immediate interpreter children are killed.** Scripts remain deliberately trusted and unsandboxed. Descendant process-group cleanup and OS resource limits are separate design changes; cancellation fixes do not claim to sandbox scripts or reap all descendants.
4. **Go temporary scripts are predictable and cancellation cleanup is incomplete.** The pid+sequence filename avoids same-process collisions, but the write does not use exclusive creation and cleanup is only reached after the child await. A cancelled Go invocation can leave the source file. A private temp-directory RAII design would address both and should be tested with a real/fake Go child.
5. **File-backed script loading is synchronous.** ScriptSource::get_code performs std::fs::read_to_string before async execution deadlines. A special file or very slow filesystem can block the caller; the FilePath trust model and async loading/bounds need a deliberate change.
6. **MCP background tasks intentionally outlive service objects.** Reaper and task-ticker handles are detached and capture AppState. This fits process-lifetime CLI use but can retain state in repeated in-process embeddings/tests; a shared-state cancellation lifecycle could improve that use case.
7. **Static interpolation recurses through JSON and can expand data.** Parsing already imposes serde nesting limits for normal input, but programmatically constructed values and repeated whole-event interpolation can grow output significantly. No global interpolation byte/node budget exists.
8. **Script entry-point detection is heuristic.** defines_handle strips # comments in every language and searches string snippets rather than parsing code; strings/comments/unusual formatting can confuse it. No parser rewrite was attempted.
9. **MCP control validation is action-name validation.** check_action_types ensures offered action names, while parameter semantics remain the protocol executor's job. This is the documented boundary; no new universal schema checker was invented.
10. **FIFO parent-directory trust is unchanged.** O_NOFOLLOW protects the final component and fstat protects the written descriptor's type. This does not promise secure traversal of attacker-controlled parent directories or ownership authentication of another FIFO; the configured path remains trusted local configuration.

## Source coverage inventory

- `src/mcp_stdio/mod.rs`: STDIO/HTTP transport lifecycle and binding (edited HTTP bind).
- `src/mcp_stdio/tools.rs`: parameter deserialization, startup configuration, queue/FIFO setup, reaper/ticker and tool dispatch (edited FIFO setup/comment).
- `src/mcp_stdio/control.rs`: vocabulary unions, action-name validation, intercept ownership and outcome descriptions (reviewed; no change).
- `src/mcp_stdio/docs.rs`: registry-derived MCP docs, startup/action/event descriptions and parameter schemas (interface/risk review; no change).
- `src/mcp_stdio/drain.rs`: outstanding request registration, Notify ordering, send result removal and one EOF drain deadline (reviewed; no change).
- `src/scripting/mod.rs`: module/export inventory (reviewed; no change).
- `src/scripting/types.rs`: source loading, event routing, context schemas and response parse shapes (allocation fix).
- `src/scripting/manager.rs`: selection, mode/runtime construction and error fallback routing (reviewed; no change).
- `src/scripting/event_handler.rs`: handler configuration/validation, event matching, reference finding/path walking and recursive interpolation (reviewed; no change).
- `src/scripting/environment.rs`: cached interpreter detection, spawn/poll/deadline/output collection (reviewed; deferred pipe collection flaw).
- `src/scripting/executor.rs`: async subprocess IO, timeout, kill/reap, language wrappers and Go file lifecycle (reviewed; deferred bounds/temp cleanup).
- `src/scripting/resident.rs`: scope keys, process ownership, queue/exchange deadlines, registry cleanup, all three harnesses (edited as described).
- `src/scripting/highlight.rs`: bundled syntax/theme loading and per-call parsing (cached resources).
- `src/llm/agent_queue.rs`: transferred FIFO notification write boundary only (open flags and descriptor validation).

Native integration test changes are in `tests/scripting_resident_test.rs`, `tests/mcp_startup_config_test.rs` and new `tests/agent_queue_fifo_test.rs`.
