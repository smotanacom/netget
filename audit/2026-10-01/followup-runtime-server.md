# Runtime and server follow-up — 2026-10-01

## Scope and authorization

This follows the user's request to implement the remaining findings, including optional ones, from [server-review.md](server-review.md) and [runtime-review.md](runtime-review.md). Those reports preserve the original inventory and the distinction between full-tree scans and focused manual review. Their sections describing unfixed issues are historical; the disposition table below supersedes them.

Owned source areas: `src/server/**`, `src/state/**`, `src/cli/**`, and `src/protocol/**`, with focused changes to scheduler call sites in `src/events/handler.rs` and `src/llm/actions/task_actions.rs`. The root agent owns session serialization and documentation-state bounds. The surfaces agent owns the additional scheduler execution-owner token integration with MCP. Source ownership was explicitly transferred at those boundaries to avoid conflicting edits.

A later handoff also assigned the shared Cargo wrappers and the released-port test helper. This report includes that work. No actual Cargo process was cancelled during validation. No GPU, embedded model, real inference service, public network service, raw socket, hardware device, deployment, or commit was used by this work. The wrapper tests cancel only fake Cargo scripts created by their own temporary fixture. Rust validation is centralized by the root agent; this report does not turn an unexecuted test into a passing result.

## Complete original-finding disposition

| Original finding or boundary | Final disposition | Implementation and evidence |
|---|---|---|
| Server S1: SAN/lifetime panics and silent malformed SAN filtering | Implemented in first pass; retained and extended | Shared fallible certificate conversion, checked validity arithmetic, strict SAN arrays. Initial TLS certificate regressions retained; QUIC now uses the same extractor. |
| Server S2: DoT partial-body wait could retain a connection indefinitely | Implemented in first pass; retained | Finite body deadline; incomplete DNS-body local TLS regression. |
| Server S3: PTY failure before ownership conversion leaked descriptors | Implemented in first pass; retained | Immediate RAII ownership; invalid-link-path descriptor regression. |
| Server S4: detached shared peer command workers survived server removal | Implemented and extended | Workers now belong to individual peer IDs and are aborted on peer close or server removal. Both cancellation paths have a blocked-write regression. |
| Server S5: MITM certificate cache had no cardinality bound | Implemented in first pass; retained | Existing expiry/eviction behavior plus maximum 1,024 leaf certificates; cache-bound regression. |
| Server remaining 1: every E2E, device and real-model mode not executed | Concrete execution boundary | Exact CPU-safe targets were selected. Real-model, hardware, privileged packets and production-load/fuzz campaigns cannot be represented by these local regressions and were not run under the user's CPU-only constraint. No runtime completeness claim is made. |
| Server remaining 2: QUIC filtered invalid SAN entries | Implemented | QUIC calls the mandatory shared TLS parameter extractor; direct spawn rejects mixed, numeric and null SAN array entries before binding. |
| Server remaining 3: closing one peer did not cancel a blocked write | Implemented | Per-peer task registry plus central close/remove integration, including registration/removal race handling. |
| Server remaining 4: contradictory historical documentation | Implemented for identified stale claims and affected paths | TLS/proxy connection-limit descriptions, proxy cache bound, DoT test coverage, QUIC SAN contract, TCP peer ownership and test-port guidance updated. Historical protocol narratives are not recast as proof of runtime correctness. |
| Server remaining 5: source scans cannot prove absence of all bugs | Evidence boundary, not an unimplemented defect | Full original coverage ledger retained. No protocol maturity rating raised and no claim of exhaustive functional verification. |
| Runtime R1: malformed parameter containers bypassed validation | Implemented in first pass; extended | Object-only container/overlay validation plus schema type/required-field preflight and shared TLS material validation before restart. |
| Runtime R2: stored scheduled-task ID differed from map key | Implemented in first pass; retained | Allocation writes the ID into the stored task; lookup/status/removal regression retained. |
| Runtime R3: concurrent timer ticks could select the same task | Implemented in first pass; extended | Atomic claim plus atomic spawn/ownership registration; active runners cannot be reclaimed during final bookkeeping. |
| Runtime R4: max-executions one ran twice | Implemented in first pass; retained | Completion includes the just-finished run; existing deterministic mock regression retained. |
| Runtime R5: log-template values were interpreted again | Implemented in first pass; retained | One-pass regex replacement with literal data and existing control-character sanitization. |
| Runtime R6: legal SQL identifiers broke metadata refresh | Implemented in first pass; retained | Double-quoted identifiers with escaped embedded quotes; in-memory punctuation/Unicode regression retained. |
| Runtime remaining: extreme scheduler delay overflow | Implemented | Fallible checked constructors; all startup/action/update callers validate before mutation; recurring/backoff arithmetic checked. |
| Runtime remaining: running scheduled futures lacked cancellation ownership | Implemented | Handles registered atomically under task ID; task removal and all scope teardown paths abort them. MCP and headless run loops additionally cancel their own executions on owner closure. |
| Runtime remaining: protocol value validation could happen after restart | Implemented where validation is deterministic before resource acquisition | Declared JSON shape, required fields, task definitions, event handlers and shared TLS material checked first. Actual listener/device acquisition remains a spawn-time operation; availability can change after preflight. |
| Runtime remaining: optional native libraries, hardware and all feature permutations | Concrete execution boundary | No optional device or privileged library probe was invoked merely for review. Native CPU and wasm checks are coordinated by root; the final validation report records exactly what compiled and ran. |
| Runtime remaining: SQL prefix classification and unbounded results | Implemented | Prepared-statement metadata, one-statement enforcement, row/value/SQL/result/work/time limits and cleanup of the progress handler. |
| Runtime remaining: optional easy-client implementation and trusted JSON fields | Implemented | Both client and server generated actions use typed normal management forms; typed IDs link the wrapper. Static local client/server regressions cover the dispatch path. |

## Scheduler validation and ownership

`ScheduledTask::new_one_shot`, `new_recurring`, and their connection-scoped constructors now return `anyhow::Result<Self>`. `checked_task_deadline` uses checked addition against the repository clock. A request such as `u64::MAX` seconds is rejected with an error instead of panicking, clamping, or running immediately. Recurring interval zero and execution limit zero are rejected.

`ScheduledTask::from_definition` and `prepare_tasks` provide one conversion for nested definitions. Explicit initial delay is honored for recurring tasks, including task actions that previously ignored it. Nested recurring definitions with no initial delay retain immediate-first-run behavior; their default interval remains 60 seconds.

Server/client startup prepares the complete task batch before allocating the instance. Management update prepares the batch before replacing tasks or restarting an instance. Legacy event-handler startup and standalone task actions propagate constructor errors. Recurring and failure-backoff scheduling also use checked deadlines. Counters saturate rather than overflowing.

`AppState` owns a `scheduled_executions` map separate from serializable task definitions. `spawn_scheduled_task` takes the same state write lock that task removal uses, checks the task is still claimed, and registers its handle atomically with spawning. Removing a previously claimed task prevents it from starting. Removing a running task aborts its future. Server, client, and connection teardown use the same removal helper. Name cleanup checks the alias still belongs to that ID before deleting it, preserving a newer same-name task.

Claiming excludes any still-running handle even if its final bookkeeping already set the definition back to `Scheduled`. This closes the window in which another tick could claim an active runner and leave a claim stranded when registration rejects it. Finished handles are pruned at the next claim.

The surfaces agent added `execute_due_tasks_owned_public`, which applies a cancellation token only to executions launched by that service. MCP teardown preserves configuration and unrelated executions while cancelling its own in-flight work. The headless client/global-task and server loops now use that same owned execution path; exiting the run cancels the token.

The headless path previously returned immediately when a saved configuration contained global tasks and no client/server. It now remains live for `Scheduled` or `Executing` tasks, runs the one-second scheduler ticker, honors run limits, drains task status messages before returning, and also tracks any server/client created by a task. Completed or failed tasks do not by themselves keep the run alive. A restored-global-only binary fixture proves both lifetime and ticker dispatch with an unavailable loopback endpoint, without contacting an actual model.

Regression files:

- `tests/scheduled_task_lifecycle_test.rs`: invalid/extreme delay and zero recurrence; explicit initial delay; removal at task/server/client/connection scope; removed-claim and final-bookkeeping races.
- `tests/scheduled_task_claim_test.rs`: earlier task identity, concurrent claim and one-run mock regressions retained.
- `tests/non_interactive_run_limits_test.rs`: restored global-only configuration remains alive until dispatch, then exits under normal failure/run-limit handling.
- Existing scheduler tests were migrated to the fallible constructor API without changing their assertions: scheduled action vocabulary, server removal, MCP removal and Kafka connection task cleanup.

## Startup validation and easy protocols

`StartupParams::new_validated` supplements the existing accessor-oriented constructor. It validates required parameters and documented JSON types before mutation: strings, booleans, numbers, integers, arrays, objects, supported typed arrays, and unions written with `|` or ` or `. Optional null retains existing absent-value behavior. Descriptive unknown type hints remain subject to protocol-specific parsing instead of inventing a new interpretation.

Server startup and management reuse `validate_server_startup_params`. Where the complete shared TLS parameter schema is present, preflight invokes the common extractor, checking SANs, validity, paired certificate/key paths and parseable material. QUIC uses the required-TLS extractor directly. Malformed parameters no longer stop a working static HTTP instance before failing.

Preflight cannot guarantee that an OS bind will later succeed, that a device remains present, or that a certificate file is unchanged between two reads. Those are actual resource-acquisition boundaries, not skipped deterministic checks. The existing startup rollback still removes failed new resources. No broad guarantee of transactionally atomic protocol restart is claimed.

`execute_easy_startup_action` now handles both `open_server` and `open_client` through `ServerForm`/`ClientForm`. It retains normal handler, initial memory, startup parameter, feedback and scheduled-task behavior. An out-of-range port is rejected by typed deserialization instead of narrowed with `as u16`. `EasyUnderlyingId` returns either a real `ServerId` or `ClientId`, eliminating JSON number casts when linking the easy wrapper. Action-generation failures set the wrapper's error status instead of leaving it in startup.

The client agent confirmed that the currently registered easy implementation generates HTTP server actions; the common metadata/trait contract permits client actions, and the shared executor now supports them. No fictional new easy protocol registration was added.

Regressions:

- `startup_params_result_test::preflight_checks_required_types_typed_arrays_and_unions`.
- `management_test::invalid_types_tls_material_and_task_delays_preserve_running_server` verifies the original static server and ID remain usable after rejected update/create requests.
- `server::quic::certificate_validation_test::quic_rejects_non_string_san_entries` exercises the actual protocol adapter without a socket bind or model.
- `easy_startup_test` verifies local TCP client memory/handler retention, static HTTP server handling, and rejection of port 65,536 before instance creation.

## Cancellation while a new instance is starting

Final integration review identified a gap before a create call returns its newly allocated ID. A client could be registered with scoped tasks and then fail its handshake; the old error path marked it `Error` but returned no ID, so the caller could not roll it back. Cancelling either server or client startup while it awaited protocol work had the same ownership gap. This also affected cancellation of session restore before `form.create` returned.

The shared private `src/cli/startup_guard.rs` guard now takes ownership immediately after `add_server` or `add_client` returns, before the next await. It stays armed through configuration, task registration, protocol startup and final status updates. Successful startup disarms it immediately before returning the ID. Cancellation schedules removal through the active runtime; removal also aborts associated instance workers and scheduled executions. Ordinary client failure explicitly awaits `remove_client` before returning its error, and the existing server failure cleanup retains its awaited removal. A cancelled explicit rollback leaves the guard armed so cleanup is still attempted.

`tests/startup_cancellation_test.rs` adds two VNC-feature regressions. A local peer closing before the RFB greeting makes create fail and verifies cleanup is already complete when the error returns. A peer withholding the greeting holds session restore inside client startup; aborting restore then removes that unreturned registration and its task while preserving a preexisting unrelated client/task. The peer observes EOF, showing the cancelled handshake released its socket. No VNC frame, model, GUI or external service is involved. POP3 was considered as a barrier but rejected after confirming its greeting is handled in a background task rather than inside the create future.

A trivial unnecessary `mut` in `src/server/dc/mod.rs` was also removed after the wide native check identified the warning.

## Individual peer cancellation

`AppState` records peer worker handles by `(ServerId, connection_id)`. `spawn_peer_command_task` registers its worker in that map. Registration checks the server and peer handle still exist; if close won the race, the worker is aborted immediately.

`remove_peer_handle`, central connection close, central connection removal, and server cleanup all release the corresponding worker handles and abort pending operations. Closing a command channel alone was insufficient when a worker was already awaiting a blocked write; explicit abort now releases the writer and pending command reply.

The TCP peer lifecycle fixture supplies an intentionally blocking writer and checks its drop signal. It exercises both full server removal and individual peer close; the second test verifies the server remains present. No peer traffic or model call is necessary to establish cancellation.

## SQLite query boundaries and classification

Query dispatch no longer guesses from text prefixes. It prepares one statement, rejects a second statement before execution, uses `column_count` to detect row-producing statements, and uses `readonly` to decide whether metadata needs refreshing. Leading comments, CTEs, `REPLACE`, `RETURNING`, and query-producing PRAGMAs follow SQLite's actual statement behavior.

Named limits are enforced in `src/state/sqlite.rs`:

| Limit | Value | Enforcement |
|---|---:|---|
| Returned rows | 10,000 | Checked before appending the next row. |
| Aggregate result budget | 8 MiB | Conservative JSON byte estimate checked before copying each value. |
| SQLite value/row allocation | 8 MiB | SQLite `SQLITE_LIMIT_LENGTH`. |
| SQL text | 1 MiB | SQLite `SQLITE_LIMIT_SQL_LENGTH`. |
| VM work | 10,000,000 steps | SQLite progress callback, checked every 1,000 steps. |
| Query duration | 5 seconds | Same progress callback checks elapsed time. |

SQLite engine limits run before a huge `zeroblob`/text value is copied into Rust. Text accounting includes a conservative escape allowance and BLOB accounting includes hexadecimal expansion. The progress callback is cleared after both success and error so a cancelled query does not poison the next one. Limit-installation errors are propagated with context.

A modifying statement can commit changes before its `RETURNING` rows exceed the result budget. Error messages state that possibility; this API has not silently wrapped arbitrary caller SQL in a new transaction. Query interrupts have SQLite's normal transaction behavior. Metadata refresh follows successful mutations, including CTE-prefixed mutations.

`tests/sqlite_query_bounds_test.rs` covers statement classification/metadata, row and byte limits, oversized engine values, an unbounded recursive CTE interrupted by the work budget, recovery on the next query, and rejection of multiple statements before a DDL side effect. All databases are in memory. Root enabled the `rusqlite` `limits` and `hooks` dependency features.

## Bounded canvas integration

The surfaces agent made display rendering fallible with pixel/command/text bounds. The VNC server now calls `DisplayCanvas::try_render` inside its blocking renderer and propagates failures instead of treating the compatibility wrapper's empty image as a real frame. The BGRX output buffer uses fallible reservation before expansion. Display limit regressions and VNC feature validation are in the central test handoff.

## Safe cancellation for shared Cargo targets

The obsolete `cargo-isolated-kill.sh` found builds by old isolated target directory names and process text. That no longer matched the shared-target wrapper and could not safely distinguish sessions.

The wrappers now launch Cargo through `scripts/cargo_session.py`. The supervisor starts its child in a dedicated process group and records a private session entry under `tmp/cargo-sessions`. Each entry includes the caller PID plus process start identity, supervisor identity, canonical repository root, private Unix-socket path, random token, command and target directory. Linux identity includes boot identity and kernel start ticks. Darwin reads microsecond process start time from `PROC_PIDTBSDINFO`; second-resolution `ps` timestamps are insufficient for rapid PID reuse.

The cancellation command filters registry entries by live caller identity and repository, then sends the random token to the private socket. It never signals a PID discovered from a JSON record or a process listing. Only the original supervisor signals its own child process group. During cancellation escalation it retains the unreaped child until after signalling, avoiding reuse of the group identifier between checks. Stale or fabricated records can fail socket IPC; they cannot direct `kill` at an unrelated PID.

`CARGO_SESSION_PID` can explicitly tie several wrapper invocations to a live shell; by default it inherits the caller. `cargo-isolated.sh` exports it before delegating to `cargo.sh`. `cargo-isolated-kill.sh --list` is read-only, the default cancellation path asks before sending, and `--yes` supports explicitly automated cancellation. Shared target/cache behavior and Cargo exit status are retained. Log filenames include both session and wrapper IDs to avoid parallel log collisions.

Registry directories and records must be owned by the current user and private. The short private socket directory avoids Darwin Unix-socket path-length failures in deep checkouts. Missing process-identity access fails closed. The scripts require Python 3 on a POSIX host, consistent with the shell wrappers; native Windows process-group cancellation is not represented by this Unix IPC design.

Validation: `python3 tests/build_scripts_test.py` passed **7/7** after the final supervisor changes. This included shared-target isolation, stale/fabricated PID records, Cargo failure propagation, argument preservation, wrapper selection, legacy cleanup and private literal credential generation. The local process-inspection/socket fixture required a sandbox escalation, which was approved. No real Cargo build was a cancellation target. `bash -n cargo.sh cargo-isolated.sh cargo-isolated-kill.sh` passed.

## Race-free ephemeral test ports

`get_available_port` bound a temporary TCP listener, read its port, and dropped the socket before NetGet used the number. That introduced a race under parallel execution and did not reserve a UDP port at all.

The common helper and re-export are removed. `{AVAILABLE_PORT}` now becomes `0`; the actual server owns its OS-assigned socket from bind onward. The shared startup parser consumes canonical server-ID/address messages, accepts IPv4 and IPv6, merges duplicate start confirmations, and preserves a nonzero bound port when a later direct-start acknowledgement has no address. It no longer assigns a random listening message to the most recent zero-port server. Socketless protocols still retain their explicit startup record.

All Rust call sites were audited and migrated:

- The shared live-LLM helper requests port zero and reads the actual `NetGetServer.port` before protocol traffic. Its live-model gate is unchanged; it was not run.
- Four SNMP behavior tests and the SNMP failure-path test request port zero and send UDP traffic to the reported port.
- LLMNR requests port zero and uses the server's actual port for both UDP and TCP.
- The IGMP client fixture records the retained UDP socket's `local_addr` port from its mocked connected event before sending multicast. It no longer attempts to protect UDP with a released TCP reservation. This platform-dependent multicast test was not run here.
- Two MySQL tool-call and two web-search HTTP tests now use the managed shared startup helper and the resulting server port instead of duplicate bind/drop helpers and fixed startup sleeps. Existing explicit real-model opt-in/ignore annotations remain. None was executed during this review.
- Guidance under `tests/README.md` and the affected protocol `CLAUDE.md` files now describes port-zero discovery.

`tests/harness_port_allocation_test.rs` verifies startup confirmations arriving out of order, duplicate IDs, IPv6, invalid addresses, socketless startup, and retained local ephemeral listener ownership. The parser module is pure standard-library code, making the parsing regression independently testable. All Rust `get_available_port` definitions and callers are absent after this migration.

## Validation handoff and final review

All edited Rust files were individually formatted with `rustfmt --edition 2021 --config skip_children=true`. Shell syntax and full-worktree `git diff --check` passed at the final local checkpoint. The accumulated scheduler, ownership, schema, TLS, easy, SQLite, startup parser, wrapper and headless-loop diffs were re-read for introduced regressions. Review specifically checked claim/spawn/remove ordering, alias cleanup, callback lifetime, early/duplicate startup confirmation behavior, process identity reuse, and retention of model test gates.

New follow-up Rust test functions owned by this section: four scheduler lifecycle, four SQLite bounds/classification, two easy dispatch, one required/type preflight, one management preflight, one QUIC SAN, one individual-peer close, two harness port tests, one restored-global headless test, and two startup cancellation/error tests. That is **19 new follow-up functions**, in addition to the 15 first-pass server/runtime functions. Existing test migrations are not counted as new tests. Cross-agent MCP/display/session tests are reported by their owners and not double-counted here.

Additional targets handed to root:

| Target/filter | CPU feature requirement | Execution boundary |
|---|---|---|
| `scheduled_task_lifecycle_test` | none | State, futures, local runtime only. |
| `sqlite_query_bounds_test`, `sqlite_identifier_test`, `sqlite_test` | `sqlite` with `limits,hooks` | In-memory SQL. |
| `easy_startup_test` | `tcp,http` | Static loopback client/server. |
| `startup_params_result_test` | none | Pure JSON schema/accessors. |
| `management_test` | `http` plus shared TLS feature in union | Static loopback server; invalid updates. |
| `server` filter `quic_rejects_non_string_san_entries` | `quic` | Rejects before bind. |
| `server` filter `peer_lifecycle_test` | `tcp` | Controlled blocked writer. |
| `startup_cancellation_test` | `vnc` | Local handshake failure/cancellation; no model or framebuffer. |
| `harness_port_allocation_test` | none | Pure parser plus retained loopback listeners. |
| `non_interactive_run_limits_test` | `tcp` | Child binary, static handlers, unavailable loopback model URL. |
| `build_scripts_test.py` | Python 3/POSIX | Temporary fake commands/process groups; 7/7 passed locally. |

Root's final validation report is authoritative for centralized Cargo results. This file records implemented behavior and concrete evidence boundaries, not blanket success across the full protocol matrix.

## Final evaluation-harness handoff

The client agent completed a pure scoring extraction while ownership was transferred for final integration. `tests/eval/scoring.rs::score_probe` is shared by the live runner and the new CPU-only `eval_probe_classification_test`. Output exceeding the capture cap is assigned verdict `error` and failure mode `probe_output_limit` before expectation matching or model diagnosis. This prevents both a false pass from a matching retained prefix and a misleading model-failure verdict. The runner retains its existing client-output, command, exit, timing, executed-action and model-evidence fields.

Final review added evidence-retention coverage and found the analogous invalid-regex path: the classifier already named it `harness_error`, but the runner still gave it verdict `fail`. The pure scorer now gives `HARNESS:` expectation errors verdict `error`. Four pure regressions cover cap precedence for matching and mismatching prefixes, ordinary pass/fail behavior, rejected action names versus actual execution, and invalid-regex harness errors with evidence. These four collaboration tests are additional to the 19 runtime/server follow-up functions counted above and should be counted once in the combined report.

`tests/ollama_model_test.rs` now calls the shared `real_ollama_requested` helper rather than treating environment-variable presence as opt-in. Existing pure affirmative-opt-in tests cover unset, empty, zero, false, off and misspelled values as disabled. The actual suite was not run against a real endpoint. Both saved-session CLI/startup cancellation fixtures were corrected to the implemented session format version 2 before centralized validation.

The surfaces agent reports the final startup guard also passed the isolated WASM rebuild and its 43-request Node smoke; root's validation ledger records those cross-agent results. Final additional native target: `eval_probe_classification_test` (no protocol feature requirement, no subprocess/network/model execution).

## Final repository checks and timer-fixture investigation

The independent final sweep passed **26/26 checks**: eight changed shell syntax checks, three changed Python AST checks, eight changed JavaScript/CJS/MJS syntax checks, all five workflow YAML files (including duplicate-key rejection), full diff whitespace, and module reachability/format inspection. The module graph contains **2,283 Rust files under `src`/`tests`, zero allowlisted and zero unreachable**. Results and per-check output are preserved in `followup/final-repository-checks.json` and `.log`.

The first module-check attempt exposed a check-script bookkeeping defect: its graph inventory excluded deleted files, but its formatting inventory still passed deleted `src/ui/events.rs` and `src/ui/layout.rs` to rustfmt. The formatting inventory now uses existing tracked/new files consistently. Initial failure details remain in the JSON history and `followup/module-format-diagnostic.log`; the corrected check passed.

Root's first final native run produced **323 passing tests and one failure across 41 targets**. The sole failure was the preexisting run-limit fixture's total elapsed bound (4.0318 seconds against `<4`). An exact old-binary rerun reproduced 4.019985 seconds. Two separate static CLI timestamp probes observed 2.1060 and 2.1063 seconds from server readiness to the two-second stop message. The library constructs its run-limit clock inside the noninteractive runner, after process/argument/settings initialization, while the fixture had started its clock before spawning the process. The shared CLI executable was also relinked by root's focused Cargo run between observations, so the old failure and direct timing probes are retained separately rather than presented as the same binary measurement.

Only the fixture changed: it retains the overall two-second minimum, starts the existing four-second upper bound at readiness, tightens its exit wait from twenty to four seconds, and prints startup/serving/total durations. The production timer is unchanged. Investigation evidence is under `tmp/audit-2026-10-01/followup/non-interactive-investigation.json`, with the original failure log and per-line timestamp probes. The rebuilt all-five-test target subsequently passed, as recorded below; the original failure remains preserved separately.

## Final centralized validation results

The fresh `non_interactive_run_limits_test` rebuild and all **5 tests passed** with zero failures/ignored tests. The timer fixture printed `startup=1.907018625s`, `after_ready=2.233383083s`, and `total=4.140402s`, directly demonstrating why startup must be separated from the serving deadline. Its exact locked/offline CPU-only Cargo command, exit code zero, elapsed build/test time and raw log are in `tmp/audit-2026-10-01/followup/non-interactive-final.json` and `.log`. The earlier 323-pass/one-failure manifest was retained rather than overwritten.

Final owned-target outcomes parsed from the centralized native log (the noninteractive row uses the fresh correction result):

| Test target | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `scheduled_task_lifecycle_test` | 4 | 0 | 0 |
| `scheduled_task_claim_test` | 8 | 0 | 0 |
| `scheduled_task_actions_test` | 2 | 0 | 0 |
| `sqlite_query_bounds_test` | 4 | 0 | 0 |
| `sqlite_identifier_test` | 1 | 0 | 0 |
| `easy_startup_test` | 2 | 0 | 0 |
| `startup_cancellation_test` | 2 | 0 | 0 |
| `management_test` | 8 | 0 | 0 |
| `startup_params_result_test` | 11 | 0 | 0 |
| `harness_port_allocation_test` | 2 | 0 | 0 |
| `eval_probe_classification_test` | 4 | 0 | 0 |
| `log_template_test` | 10 | 0 | 0 |
| `log_template_injection_test` | 5 | 0 | 0 |
| `server_stop_cleans_scheduled_tasks_test` | 3 | 0 | 0 |
| `non_interactive_run_limits_test` | 5 | 0 | 0 |

All **10 server regressions across six exact filtered invocations passed**: certificate validation (5, including QUIC), DoT partial-body deadline (1), PTY failure cleanup (1), server removal with blocked peer write (1), individual peer close with blocked write (1), and proxy cache cardinality (1). Each command and raw log is indexed by `tmp/audit-2026-10-01/followup/final-check-results.json`.

Root's final 46-feature native library check passed. The final all-target Clippy run passed with `clippy::correctness` and `clippy::suspicious` denied. The recursive-decoder source check passed all 3 tests after root removed three stale allowlist entries whose implementations already had explicit depth bounds. Root also corrected the shared child-guard duplicate module import surfaced during integration. These are integration corrections, not additional protocol runtime claims.

The final **7/7 Python build-wrapper fixture** output is preserved at `tmp/audit-2026-10-01/followup/build-scripts-final.log`, with exact command, exit code zero, 9.229-second test duration, and capture provenance in the adjacent `.json`. This is the captured output of the already-completed final tool run (session 59969), not a new execution or the earlier five-test baseline. No real build was cancelled.
