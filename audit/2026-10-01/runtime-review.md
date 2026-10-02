# CLI, state and protocol runtime review — 2026-10-01

## Scope and constraints

Second-wave review of `src/cli/**`, `src/state/**` and `src/protocol/**`, after the server review. Every Rust file in these directories participated in inventory and risk-pattern scans (panics, raw I/O, casts, time arithmetic, task creation/cancellation, parameter validation, registry lookup and dynamic SQL/log formatting). Focused manual reads followed the state/task, create/update and rendering paths described below. This report distinguishes those targeted reads from full functional verification.

- `src/cli`: 15 Rust files, 6,873 lines at the report snapshot.
- `src/state`: 11 Rust files, 5,483 lines at the report snapshot.
- `src/protocol`: 15 Rust files, 5,906 lines at the report snapshot.

No GPU, embedded model, public service or real LLM was run. The scheduler completion regression uses the project's **local `MockOllamaServer`** with one canned response, pinned model name, and scripting disabled. Other new tests are pure state/formatting/in-memory SQL or deterministic static HTTP on loopback. All Cargo builds/tests are coordinated by the root agent.

## Implemented changes

### R1. Reject malformed parameter containers before creation or restart

**Source:** `src/protocol/spawn_context.rs`, `src/cli/management.rs`.

`StartupParams::new` only checked keys inside `if let Some(obj) = params.as_object()`. Arrays, booleans, numbers, strings and null bypassed validation and behaved like an empty parameter set when optional accessors were used. Separately, the management update's `merge_params` silently ignored a non-object overlay and returned the old parameters as a new object; the caller then treated the supplied update as restart-worthy and could drop an otherwise healthy connection for an invalid request.

The constructor now requires a JSON object and returns `StartupParamError::Invalid` naming `startup_params` for malformed containers. `merge_params` is fallible and rejects a non-object overlay before either server or client update mutates state. Valid object overlays keep their previous key-merge behavior; explicit null **field values inside an object** still retain their existing accessor semantics. An absent container belongs in the outer `Option`, rather than as a scalar value inside `StartupParams`.

**Tests:** `tests/startup_params_result_test.rs::parameter_container_must_be_an_object` covers six invalid JSON shapes plus an empty object; `tests/management_test.rs::non_object_startup_update_keeps_the_existing_server` attempts four malformed overlays while requiring the original static HTTP server ID and response to survive.

### R2. Scheduled tasks use their allocated ID everywhere

**Source:** `src/state/app_state.rs::add_task`.

`add_task` allocated the storage-map key but left `ScheduledTask.id` at its caller-supplied value. Constructors in the tree pass zero or a random placeholder. The scheduler later updated status, recorded executions and removed completed tasks using this stale internal ID, which could miss the stored task or affect a different task if a placeholder happened to collide. This is a functional defect, not just display inconsistency: a completed task could remain scheduled indefinitely.

`add_task` now assigns the allocated ID back to the task before storing it. Name lookup, numeric lookup, task clones, execution bookkeeping and removal refer to the same identity.

**Test:** `allocated_task_id_controls_lookup_status_and_removal` passes a distinctive placeholder, compares returned and stored IDs, changes status through the stored ID and removes the task through that same ID.

### R3. Concurrent scheduler ticks atomically claim work

**Source:** `src/state/app_state.rs::claim_due_tasks`, `src/cli/tasks.rs`.

The timer used a snapshot from `get_all_tasks()` and then separate `update_task_status` calls. Two concurrent timer drivers could both see `Scheduled`, independently mark the same task and both execute it. The scan also cloned future/completed tasks that would immediately be ignored.

The state API now selects only due scheduled tasks and marks them `Executing` while holding one write lock, returning owned clones to the timer. The timer spawns only this claimed set; network/model work still occurs after the state lock is released.

**Test:** sixteen concurrent claimers synchronize on a barrier around one due task, one future task and one completed task. Exactly one claimer receives the due task, a further tick receives nothing while it is executing, and the future task remains scheduled. The test has no model or socket.

### R4. Recurring tasks stop at their execution limit

**Source:** `src/cli/tasks.rs::handle_task_success`, `src/cli/mod.rs`.

Success handling incremented the stored execution count but checked the pre-execution task snapshot against `max_executions`. A task with `max_executions: 1` therefore scheduled a second run. The comparison now includes the just-completed run using saturating addition on the snapshot count.

The existing tick wrapper is publicly re-exported from `cli` so an integration test can drive the production timer path; the entire internal task module remains private.

**Test:** `recurring_task_stops_after_its_first_allowed_execution` creates a recurring task with a one-run limit and a far-future interval, makes its first deadline due, drives one real scheduler tick against a local mock, waits for the explicit removal status, and asserts the task is absent. A second tick cannot call the mock again; exact mock call verification requires one request. This covers the R2 task-ID fix and the R4 completion fix together without a real model.

### R5. Log templates replace only original template matches

**Source:** `src/protocol/log_template.rs`.

The old renderer iterated matches from the original template but repeatedly called `String::replace` on the evolving result. A peer's field value containing `{another_field}` was then interpreted again when that other field appeared later in the template. Repeated template occurrences could compound the rewriting. Log output no longer faithfully represented the input data, and each match rescanned/reallocated the complete growing string.

The renderer now uses one `Regex::replace_all` pass with a closure. Replacement strings are inserted literally, including dollar signs and braces. Each field still goes through `line_field`, so CR/LF and other terminal control characters remain sanitized.

**Test:** `tests/log_template_test.rs::field_values_containing_placeholders_remain_literal` mixes cross-referencing placeholder text, a repeated placeholder, `$1` and CRLF. It requires literal data values and sanitized control characters. Existing `log_template_injection_test` remains relevant regression coverage.

### R6. SQLite introspection quotes legal table identifiers

**Source:** `src/state/sqlite.rs`.

Schema refresh and DML row-count refresh inserted table names from `sqlite_master` into single-quoted SQL without escaping. A legal table such as `"O'Brien"` could be created successfully, then cause the following metadata refresh to fail, reporting the overall operation as an error while the table already existed. Embedded double quotes and punctuation also need intentional identifier handling.

A private, feature-gated identifier helper wraps names in double quotes and doubles embedded double quotes. The same helper is used for `PRAGMA table_info`, initial `COUNT(*)` and DML count refresh. Values and arbitrary caller SQL are not rewritten.

**Test:** `tests/sqlite_identifier_test.rs` creates tables named `O'Brien`, `double"quote`, `semi;colon`, and `雪 table` in an in-memory database; inserts two rows, checks column/row metadata, deletes one row, rechecks the count and refreshes the complete schema. No filesystem database is created.

## Validation handoff and results

All changed Rust files were formatted individually with `rustfmt --edition 2021 --config skip_children=true`. `git diff --check` passed for the owned source/test paths. The complete accumulated source diff was manually re-read after the final changes, including the earlier server changes.

Cargo execution is centralized. The root agent received these target names and CPU-only feature requirements; the main review report records the actual execution outcomes:

| Target | New tests | Required feature | Runtime behavior |
|---|---:|---|---|
| `startup_params_result_test` | 1 | none | Pure parameter parsing. |
| `management_test` | 1 | `http` | In-process static loopback HTTP, no model. |
| `scheduled_task_claim_test` | 3 | none | Two pure state tests; one local mock HTTP server. |
| `log_template_test` | 1 | none | Pure string rendering. |
| `sqlite_identifier_test` | 1 | `sqlite` | In-memory SQL only. |

Related existing regression targets requested: `server_stop_cleans_scheduled_tasks_test`, `scheduled_task_actions_test`, `log_template_injection_test`, `sqlite_test`. Seven new runtime test functions were added. Together with eight new server test functions, this agent's two sections add fifteen focused regressions.

## Remaining observed risks and boundaries

### Scheduling delay arithmetic can still overflow

**Confirmed locally with a standalone CPU-only standard-library program:**

```rust
fn main() {
    let now = std::time::Instant::now();
    println!("{}", now.checked_add(std::time::Duration::from_secs(u64::MAX)).is_some());
}
```

This printed `false` on the current native macOS environment. The unguarded `now + Duration::from_secs(u64::MAX)` form consequently panics. Relevant paths:

- `ScheduledTask::new_one_shot` and `new_recurring` in `src/state/task.rs`.
- `next_execution: Instant::now() + delay` in `src/cli/server_startup.rs`, `client_startup.rs` and management task construction.
- Recurring rescheduling in `src/cli/tasks.rs`.

A minimal in-process repository reproduction is `ScheduledTask::new_one_shot(TaskId::new(0), "overflow".into(), TaskScope::Global, u64::MAX, "noop".into(), None)`. A corresponding supplied `scheduled_tasks` definition with `delay_secs: 18446744073709551615` reaches the CLI builders. This report does **not** claim that constructor reproduction was executed against the full NetGet library; the standalone representability test and the source additions establish the arithmetic failure.

The root agent explicitly retained this as follow-up for this batch: a correct fix should define a scheduling validation policy, return an actionable error before creating/mutating an instance, and propagate it through direct task constructors and the LLM action path. Silently clamping a requested delay or executing it immediately would change the requested behavior. No broad fallible-constructor API migration was attempted in this batch.

### Other evidence boundaries

- Atomic claiming prevents duplicate selection; it does not add cancellation ownership to already-running scheduled-task execution futures. A task removed after selection can still have in-flight work, and scope teardown requires separate cancellation analysis.
- Parameter-container validation does not promise every protocol-specific value is validated before a restart; many semantic checks live in the protocol spawn implementation. Existing valid-object behavior is preserved.
- Registry/dependency code was inspected structurally, but optional native libraries, hardware and all feature permutations were not loaded or built by this subagent.
- SQLite query classification still follows the existing statement-prefix logic. This change fixes identifier handling in metadata refreshes; it is not a SQL parser redesign or a bound on arbitrary query result size.
- Existing `easy_startup` has a limited client implementation and trusts generated action field conventions. This pass did not claim to complete that optional surface.

## Complete file coverage ledger

Every file below received inventory/risk-pattern scanning. Focused notes identify directly followed code paths; other entries are static triage, not assertions of full runtime correctness.

| File | Lines | Focus / disposition |
|---|---:|---|
| `src/cli/args.rs` | 935 | Reviewed lazy cached stdin/action-file handling; scanned numeric parsing and options. |
| `src/cli/banner.rs` | 194 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/cli/client_startup.rs` | 426 | Reviewed symmetric client startup/task construction and unchecked delay. |
| `src/cli/crash_restore.rs` | 92 | Scanned signal/termios unsafe ownership; no signal tests or terminal changes. |
| `src/cli/easy_startup.rs` | 164 | Read generated action execution, wrapper status, ID and port handling; no change. |
| `src/cli/input_state.rs` | 419 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/cli/management.rs` | 1,355 | Fixed malformed-overlay validation; followed hot/restart paths and scheduled-task construction. |
| `src/cli/mod.rs` | 780 | Re-exported existing tick wrapper for integration coverage; inspected module boundary. |
| `src/cli/model_select.rs` | 98 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/cli/non_interactive.rs` | 717 | Reviewed run-limit elapsed-time checks and timer/shutdown paths; no change. |
| `src/cli/server_startup.rs` | 830 | Read generic schema/privilege/dependency/startup paths; recorded unchecked scheduled delay. |
| `src/cli/setup.rs` | 196 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/cli/tasks.rs` | 410 | Fixed atomic timer claiming and one-run completion limit; inspected backoff and action dispatch. |
| `src/cli/terminal_cleanup.rs` | 4 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/cli/theme.rs` | 253 | Inspected terminal flag preservation and nonblocking flush; no terminal query run. |
| `src/protocol/binding_defaults.rs` | 96 | Read user override precedence; no change. |
| `src/protocol/client_registry.rs` | 882 | Inspected exact/normalized client lookup and feature error paths; no change. |
| `src/protocol/connect_context.rs` | 83 | Read client context and StartupParams propagation; no change. |
| `src/protocol/default_port.rs` | 173 | Inspected protocol defaults and privilege/in-use fallback; no sockets opened by review. |
| `src/protocol/dependencies.rs` | 502 | Inspected dynamic-library probe handle cleanup and native dependency checks; no probes run. |
| `src/protocol/docs.rs` | 124 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/protocol/dual.rs` | 117 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/protocol/easy_registry.rs` | 84 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/protocol/event_logger.rs` | 325 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/protocol/event_type.rs` | 472 | Inspected action declarations/default examples and event field conventions; no change. |
| `src/protocol/log_template.rs` | 285 | Fixed recursive replacement of input values; retained sanitization. |
| `src/protocol/metadata.rs` | 582 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/protocol/mod.rs` | 121 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/protocol/server_registry.rs` | 1,449 | Inspected exact/normalized lookup and feature-gated registry paths; no registration changes. |
| `src/protocol/spawn_context.rs` | 611 | Fixed object-shape validation; reviewed typed accessor error paths. |
| `src/state/app_state.rs` | 3,302 | Fixed task ID assignment and atomic claims; reviewed server/client task ownership, handles, intercepts and teardown. |
| `src/state/client.rs` | 343 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/state/client_handles.rs` | 65 | Read command queue/reply types and lifetime documentation; no change. |
| `src/state/easy.rs` | 136 | Inspected wrapper instance IDs/status and optional handle; no change. |
| `src/state/intercepts.rs` | 103 | Read dead-entry detection and oneshot ownership; no change. |
| `src/state/machine.rs` | 58 | Read generic map-backed state machine; no change. |
| `src/state/mod.rs` | 26 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/state/server.rs` | 483 | Read connection registration/removal/history and cleanup; no change. |
| `src/state/server_handles.rs` | 55 | Structural/risk-pattern scan; no demonstrated defect changed in this file. |
| `src/state/sqlite.rs` | 612 | Fixed identifier quoting; inspected metadata refresh, query dispatch and database-name protections. |
| `src/state/task.rs` | 300 | Read task definitions/constructors and execution counters; documented extreme-delay overflow. |
