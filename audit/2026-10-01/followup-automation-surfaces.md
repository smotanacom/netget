# Follow-up: automation, display, dashboard, browser network, and npm

This implements the remaining concrete issues and optional improvements listed in `automation-review.md` and `surfaces-review.md`. No GPU workload, GPU capability probe, real model endpoint, model download, deployment, package publication, or commit is involved. The browser checks use Chromium with GPU-related features disabled and plain JavaScript model/GPU fixtures; they never invoke those real APIs.

## Mapping of every original automation item

| Original item | Resolution | Evidence and limits |
|---|---|---|
| Interpreter probes could deadlock while draining inherited pipes | Replaced exit polling followed by blocking `wait_with_output` with concurrent, bounded Tokio collectors and child wait under one deadline. The four runtime probes run concurrently. | `scripting_resources_test`: noisy stdout refusal, large stderr without deadlock, descendant-retained pipe deadline. Probe output limited to 64 KiB per stream. Browser detects no subprocess runtimes. |
| Per-event and resident output was unbounded | Per-event stdout 8 MiB, stderr 1 MiB; collectors fail immediately on excess. Resident reply lines 8 MiB with strict UTF-8 and complete-line EOF validation. Resident diagnostics log at most 1 MiB over the child lifetime, then drain/discard in fixed 8 KiB scratch space. | Oversized-output and resident-eviction regressions. Error paths retain ordinary BrokenPipe compatibility on stdin. The parent bounds retained output; trusted child code itself is not memory-sandboxed. |
| Only immediate interpreter children were killed | Added shared `scripting::process_io::ProcessGroup`: independent Unix process groups and Windows kill-on-close Job Objects; completion, failure, timeout, cancellation and resident shutdown terminate the group/job. | Unix descendant termination regression. Public helper also available to the coordinator's eval probe. Windows API/type signatures passed an isolated x86_64-pc-windows-gnu cross-check importing this exact module; lifecycle execution still requires Windows runtime CI. No Windows execution claimed on macOS. Cleanup is not a hostile-code sandbox: deliberate new Unix sessions can escape; Windows Job assignment immediately follows spawn. |
| Predictable Go files and cancellation leaks | Random exclusively created private directories (0700 Unix), fixed `main.go` inside, RAII directory cleanup. Creation, staging and compilation are inside the overall execution budget. | Cancellation regression identifies its unique staged source, verifies directory mode, cancels the Go future and requires directory removal. Uses the real local Go toolchain without dependencies/model calls. |
| Synchronous file-backed source loading | Added `get_code_async` on the blocking pool; opens and checks the actual descriptor is a regular file; Unix `O_NONBLOCK` refuses FIFOs without waiting for a writer. Both inline/file source capped at 4 MiB. Per-event and resident dispatch deadlines cover source loading and registry waits. | Tests oversized file, directory, valid regular file, FIFO without writer. A caller stops awaiting a slow filesystem operation at the deadline; a kernel filesystem call already running on a worker is not forcibly interrupted. |
| Detached MCP reaper/ticker retained state | Shared service now owns reaper/ticker/status-drain lifecycle; last service owner closes these tasks even when another component retains an AppState clone. | Last-owner lifecycle regression first proves a retained service clone keeps ticking, then proves AppState alone does not. A per-service cancellation token also cancels already-launched executions from that service; their definitions remain with Failed status, and externally launched executions keep running. A second lifecycle test covers that distinction. The ticker finishes its current claim/spawn batch before stopping. |
| Static interpolation could amplify or recurse without budget | Shared expanded-text budget 8 MiB, 65,536 nodes and 64-level trees. Iterative preflight before recursive transformation or cloning; bounded writer for embedded JSON; aggregate output validated before return. Duplicate interpolated object keys now fail explicitly. | Tests repeated whole-value/embedded substitutions, aggregate action budget, 500-level programmatic JSON, colliding keys, and existing type-preserving/literal-brace suite. Limits are deliberate configuration bounds. |
| Handle detection stripped `#` in every language and searched substrings | Language-aware tokenizer skips relevant comments, ordinary/triple/template strings, JS regexes and Perl quote operators; whitespace-separated declarations and async/ordinary arrow bindings are recognized. | Regression fixtures distinguish real declarations from comments, strings, regexes and ordinary parenthesized assignments. This is a configuration diagnostic, not a full language parser or security sandbox. |
| MCP validates action names, not every parameter schema | Documented the existing division explicitly: control vocabulary validation; protocol executor owns parameter semantics. | No invented universal schema was imposed. Static references receive their own new resource/syntax checks. This is a trust/interface boundary, not unfinished generic-validator code. |
| FIFO parents/ownership were not authenticated | Retained prior startup FIFO check and open-time `O_NOFOLLOW` plus descriptor FIFO check; documented trusted parent-path/ownership boundary. | Existing FIFO replacement/file/symlink regressions remain. The local configured pathname is trusted; authenticated ownership and hostile ancestor traversal were not the contract of a notification FIFO. |

## Mapping of every original surface item

| Original item | Resolution | Evidence and limits |
|---|---|---|
| New TextRenderer per canvas text operation reloads fonts | Canvas rendering lazily owns one TextRenderer for the whole render, shared by labels, textboxes, windows and ASCII art. Glyph cache reused and bounded to about 32 MiB between glyphs. | Existing alpha/baseline/wide-text checks plus native display suite. Buffer layout is clipped to visible height, offscreen glyphs skip rasterization, fonts capped at 512 px and text at the canvas budget. |
| Infallible huge canvas allocation | Added `try_render` with an explicit CanvasError, 16-megapixel/10,000-command/1 MiB text budgets, checked/fallible backing allocations and bounded font sizes. | Oversized dimension test fails before allocating pixels. Compatibility `render()` returns an empty image on error; the sole production VNC caller was changed by the server agent to propagate `try_render()` errors. |
| Recursive window rendering and cloned subtrees | Preflight/execute borrowed command references in an iterative queue with accumulated saturating offsets. Preserve preorder and title-bar offsets. Canvas command destruction/clear is iterative too. | 20,000-level rejected tree can be cleared/dropped without recursion; nested-window pixel test verifies exact offsets and draw order. |
| Unbounded virtual TCP accept queues | Bounded 128-entry backlog. Connect reserves a slot before allocating its duplex pair and waits for accept capacity; closing listener returns ConnectionRefused. | Shim backlog backpressure/resumption/close regression. Existing 8 TCP peek/read/split/EOF checks pass. TCP peek scratch allocation also clamps to per-direction pipe capacity. |
| Unbounded UDP queues | Bounded 128-datagram queue; new arrivals are dropped at capacity, modeling receive-buffer loss, without allocating their payload when no permit exists. | Overflow/drop-new/recovery/zero-byte datagram regression. Up to one additional datagram may occupy the shared peek slot. |
| Concurrent UDP peek/readiness and connected filtering parity | Queue and peek slot now share one mutex; pending peeks cannot separately consume and overwrite datagrams. Every receive/peek/readiness variant filters connected peer ports. | Simultaneous pending peek test, connected recv_from/peek test, old try_recv/filter tests. Addresses remain the documented single-host virtual network abstraction. |
| Feed cursor changes identity when the ring evicts | Selection and mouse hit targets now use stable sequence IDs. Evicting the selected entry moves to the oldest retained entry. | 5,000-entry eviction regression retains selected text while earlier entries disappear, then checks selected eviction fallback. |
| Scrolled activity jumps as entries arrive | Stores top entry sequence plus wrapped-line offset and resolves the anchor at subsequent paints; explicit scroll commands invalidate/recompute it. Unseen counter saturates at ring capacity. | Software TestBackend viewport test compares the same top row before/after ten new events. |
| Card fold state survives vanished instances/peers forever | Prunes collapsed/open/show-all state against current cards, live/recent peers, connectionless requests and client attempt history. | Fold lifecycle test preserves state while nodes exist, removes it when absent, and gives a returned node default state. |
| Inactive UI source referenced removed APIs | Removed uncompiled `src/ui/events.rs` and `src/ui/layout.rs`; `src/ui/mod.rs` only exposes the still-used app state. | Coordinator removed their entries from `.github/unreachable-modules.txt`; no references from active modules/tests were found. |
| npm downloads trusted archives without checksum provenance | Fallback requires exact single archive entry in release `SHA256SUMS`, streams SHA-256 during download, refuses mismatch/missing/ambiguous manifests before extraction. Extracts only the expected binary; regular-file check remains. | Coordinator wires generation/verification/upload in release and npm publication pipelines. Manifest authenticates bytes relative to the trusted release/mirror; not an independent publisher signature. Existing platform dependencies and explicit NETGET_BINARY retain precedence. |
| npm timeout/staging/signals lacked fault injection | Download bytes bounded (512 MiB archive, 128 KiB manifest), deadlines for download/extractor, random cache staging, cleanup of both locations on failure, repeated signal forwarding tested. | Eight Node launcher/download tests pass: argv/status, platform cache, manifest matching, valid installation, corruption/missing manifest, rename failure cleanup, stream excess/stall, repeated SIGTERM. Fetched executables are harmless inert fixtures and never run. Windows signal/extractor runtime requires Windows CI. |
| Browser tests required potentially real GPU/model probing | Added explicit `NETGET_CPU_ONLY=1` mode disabling GPU/compositing/software rasterizer/WebGPU/on-device model features, initializes pages with no real GPU/model APIs, and skips real Prompt API probe. Requests outside local server are fulfilled from fixtures or blocked, including fonts. | All 12 synthetic browser scenarios passed against both the preexisting bundle and the rebuilt current-source bundle, Chromium 154.0.8037.93 and xterm stub. No screenshots taken; real Prompt API probe explicitly skipped. |
| Asset packaging for added Telnet module | Existing JS asset packaging already includes `site/js/telnet.js`; no deployment required. | Six hermetic Node parser/composer tests rerun successfully. |
| Windows runtime coverage unavailable locally | Implemented Windows Job Objects and retained cross-platform launcher logic, while marking native Windows execution evidence separately. | An isolated Windows GNU cross-check importing the exact ProcessGroup module passed. Full package cross-build stops in ring because MinGW GCC is absent. Coordinator added a Windows CI job; macOS does not claim Windows execution. |
| Historical public/repository/model documentation could drift | Changed only verifiable local behavior in scripting/MCP/npm/browser docs. No external model/hosting calls were made to refresh unrelated claims. | External currentness is a source/provenance boundary rather than a reason to invent values. |

## Additional integration improvements

- `web/build.sh` now respects `CARGO_TARGET_DIR` when locating the wasm-bindgen input instead of hardcoding `target/`. Cargo builds use `--locked`; installed wasm-bindgen version is still checked against the lockfile.
- The server agent changed the VNC production path to propagate the fallible renderer result and allocate its output copy fallibly.
- Resource helper APIs are reusable by the root agent's eval/probe lifetime cleanup instead of introducing a second implementation.
- CI now installs Playwright/Chromium and runs the CPU-only browser fixture suite after the WASM build/smoke. The resource lifecycle job installs Go and includes the MCP startup/owner tests. CI YAML and browser Python parse successfully.
- Original first-pass fixes and tests remain part of the final diff; this follow-up does not replace their evidence.

## Verification record

Completed locally:

1. `cargo test -p netget-tokio-wasm --test tcp_peek_test --test udp_receive_test --test time_math_test --test backlog_test --locked --offline` with isolated CPU target: **18 passed**, zero failures/ignored.
2. `node --test tests/npm_download_test.cjs tests/npm_launcher_test.cjs`: **8 passed**, zero failures/skips. Log `tmp/audit-2026-10-01/followup/npm-tests.log`.
3. `node --test web/test/composer_model_test.mjs web/test/telnet_test.mjs`: **6 passed**, zero failures/skips. Log `tmp/audit-2026-10-01/followup/browser-model-tests.log`.
4. First `NETGET_CPU_ONLY=1 python3 web/test/page_composer.py`: **all 12 synthetic scenarios passed**, existing WASM bundle. Initial sandbox loopback bind was denied; the CPU-only loopback test was subsequently approved and ran. No application publication or external request was involved.
5. `env CARGO_TARGET_DIR=/private/tmp/netget-audit-web-20261001 RUSTC_WRAPPER= CARGO_NET_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 ./web/build.sh --dev`: initial full WASM build **passed** in 4m47s with cached toolchains/dependencies. The final current-source incremental rebuild also **passed** in 1m48s; see final integration results below.
6. Targeted rustfmt, JS/Python/shell syntax checks and `git diff --check`: passed at this checkpoint.

Coordinator-run native targets requested: `scripting_resources_test`, `scripting_resident_test`, `scripting_executor_test`, `scripting_environment_test`, `static_handler_interpolation_test`, `mcp_startup_config_test`, `dashboard_frame_test`, `display_rendering_test`. MCP features include tcp/mcp-stdio; mcp-http compile covers the prior IPv6 transport change. The consolidated root report is authoritative for final native pass counts and any platform conditions.

## Final integration results

- Current-source dev-WASM build **passed**: `tmp/audit-2026-10-01/followup/wasm-build-final.log`.
- Rebuilt-bundle `node web/test/smoke.mjs` **passed**, including 43 synthetic requests over virtual TCP/UDP, HTTP, OpenAPI, JSON-RPC, RSS, HTTP/2 and HTTP-family client fixtures. These use a JavaScript model bridge and local virtual protocol fixtures, including the fixture named Ollama; no real model was used. Log `tmp/audit-2026-10-01/followup/wasm-smoke-final.log`.
- Rebuilt-bundle `NETGET_CPU_ONLY=1 python3 web/test/page_composer.py` **passed all 12 synthetic scenarios**, Chromium 154.0.8037.93, xterm DOM stub; real Prompt API explicitly reported `not probed (CPU-only mode)`. Responsive DOM layout was checked at 1280x800, 1440x900, 1920x1080 and 390x844. This is interaction/layout evidence, not a screenshot or real xterm canvas rendering check. Log `tmp/audit-2026-10-01/followup/browser-cpu-final.log`.
- Isolated Windows GNU fixture **passed** in 6.89s. Its library imports the exact current `src/scripting/process_io.rs`, with direct dependencies pinned to Tokio 1.48.0, UUID 1.18.1 and windows-sys 0.61.2; `cargo check --target x86_64-pc-windows-gnu --offline` checks Job Object APIs without needing a linker. Fixture at `/private/tmp/netget-windows-process-fixture-20261001`, log `tmp/audit-2026-10-01/followup/windows-process-fixture.log`. This does not claim Windows process execution.
- An initial unoptimized dev-WASM smoke exposed a memory access fault late in the synthetic server scenario. Moving new 8 KiB read scratch arrays onto the heap reduced nested async future construction size; the subsequent unchanged-profile build and full smoke passed. The failure and correction are retained in the logs rather than hidden as a skipped scenario.

The coordinator's `native-second` CPU run completed successfully (exit 0, 219.76 seconds), with 32 test threads. Within this report's owned surface targets: **120 passed, 0 failed, 0 ignored across 10 targets**. This includes all new scripting resource, service-owner, interpolation, dashboard and fallible renderer regressions.

| Native target | Passed | Failed | Ignored |
|---|---:|---:|---:|
| `dashboard_frame_test` | 20 | 0 | 0 |
| `display_rendering_test` | 6 | 0 | 0 |
| `mcp_startup_config_test` | 13 | 0 | 0 |
| `scripting_environment_test` | 3 | 0 | 0 |
| `scripting_executor_test` | 11 | 0 | 0 |
| `scripting_highlight_test` | 2 | 0 | 0 |
| `scripting_manager_test` | 8 | 0 | 0 |
| `scripting_resident_test` | 15 | 0 | 0 |
| `scripting_resources_test` | 8 | 0 | 0 |
| `static_handler_interpolation_test` | 34 | 0 | 0 |

Evidence: `tmp/audit-2026-10-01/followup/native-second.log` and `native-second.json`. The root consolidated report records the complete multi-agent native target count and features. There are no pending failures in this report's native/browser/Node/shim or ProcessGroup cross-check results.

The final read-only integration review rechecked renderer traversal/allocation/drop, glyph rendering, interpreter/descendant/temp-file ownership, resident output, virtual TCP/UDP queue behavior, Windows fixture portability, and CPU browser routing. No additional concrete defect was found in those changes. A remaining false-valued live-model opt-in in root-owned `tests/ollama_model_test.rs` was reported to the coordinator for consistency with its shared explicit-opt-in helper.


### Startup ownership integration recheck

After the coordinator introduced the common startup registration guard, an isolated incremental `web/build.sh --dev` rebuilt the current source successfully in **14.32 seconds**. Its cleanup path using `tokio::runtime::Handle::try_current().spawn(...)` compiles against the WASM runtime shim. The rebuilt Node smoke again **passed all 43 synthetic requests**, with no real models, GPU APIs, or external services. Logs: `tmp/audit-2026-10-01/followup/wasm-build-startup-guard.log` and `wasm-smoke-startup-guard.log`. The already-passing CPU Chromium interaction suite was not repeated because no browser interaction source changed.

The resource lifecycle CI job now also selects `startup_cancellation_test`, `non_interactive_run_limits_test`, and the pure `eval_probe_classification_test` scoring fixtures. YAML parsing and `git diff --check` passed after these CI changes.


### Final formatted-source snapshot

After the session restoration guard adopted the same runtime-presence check, the final formatted source rebuilt successfully in **22.38 seconds**, and its Node smoke again **passed 43 synthetic requests**. Both startup ownership and restoration cleanup now compile against the WASM shim. Source hashes for **839 Rust/build inputs** were compared after build and smoke: **zero changes** during verification.

- Exact final commands, exit codes, log paths and generated bundle SHA-256 values: `audit/2026-10-01/wasm-final-verification.json`.
- Full source SHA-256 inventory: `audit/2026-10-01/wasm-final-source-sha256.json`.
- Logs: `tmp/audit-2026-10-01/followup/wasm-build-final-snapshot.log` and `wasm-smoke-final-snapshot.log`.

This supersedes earlier WASM build/smoke snapshots while preserving their diagnostic history. No additional GPU or real-model work was performed.
