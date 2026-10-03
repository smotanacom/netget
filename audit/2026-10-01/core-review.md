# Shared core, build tooling and CI review — 2026-10-01

## Scope and method

The coordinator reviewed shared LLM bookkeeping and parsing, configuration persistence,
credential display, logging/utilities, the executable/build surface, scripts, CI and the
whole-tree coverage mechanisms. Other reports cover client/server implementations,
runtime management, UI/browser code, scripts/MCP execution and test infrastructure.

The inventory snapshot contains **3,342 tracked files and 1,250,366 text lines**. These
include documentation, test fixtures, corpus inputs, vendored source and implementation;
the line total is not a count of production code. The whole-source heuristic scan covers
**811 tracked Rust source files and 512,054 source lines** at its checkpoint, including
unchanged files. New source introduced afterward is also covered by targeted review and
the final changed-file ledger. The inventory records paths, byte counts, line counts and
hashes; the static scan records review-signal line numbers without exposing local runtime
configuration or logs.

This is complete inventory/section coverage plus targeted deep review, not a claim that
1.25 million lines were each manually verified or every protocol behavior was executed.
The risk scans include comments and intentional constructs: their counts are leads, not
defect counts. No unrelated untracked user files were removed, staged or included as audit
artifacts. No commit, push, deployment, publication or third-party message occurred.

## Implemented improvements

### C1. Save/load preserves directory components

`normalize_filename` previously used `Path::file_stem` without restoring its parent path.
Saving `/chosen/location/session.json` therefore wrote `session.netget` in the process
working directory; loading through the same input path could likewise read the wrong
file. Replacing only the extension preserves absolute and relative parent directories,
including a filename already ending in `.netget` and the hidden `.netget` filename.
Existing whitespace trimming and ordinary basename extension replacement remain.

Files: `src/utils/save_load.rs`, `tests/utils_save_load_test.rs`.

### C2. Saved instances retain routing and feedback behavior

Server/client save actions omitted `event_handlers` and `feedback_instructions`. Reloading
a saved deterministic/manual instance could therefore change who answers its events.
Both the all-instance and individual-instance paths now serialize the configured handler
array and feedback instructions. Optional absent values remain absent. The existing
action parser accepts these fields, including the serialized wildcard representation.

Individual saves previously manufactured partial `ServerInstance`/`ClientInstance`
objects with many defaulted fields. They now use the actual state snapshot through the
read APIs, removing the divergence that would otherwise keep losing future fields.

The regression creates server/client state without opening sockets or contacting a model,
saves each separately and together into a temporary directory, loads all three files,
checks the returned paths and feedback fields, and reparses the restored handler rules.

### C3. Queued requests recheck the token budget

The limiter checked token capacity before waiting for concurrency. A queued request could
pass that check, wait while the preceding request spent the remaining allowance, and
still receive a permit. The budget check now also occurs after the concurrency permit is
acquired. Network requests return the existing typed TokenLimit error and release their
permit; user requests retain the existing wait-for-budget behavior.

The regression holds the sole permit, waits until a second request is demonstrably queued,
records the first request's final usage, and verifies that releasing the first request
causes the second to be refused exactly once with no leaked queue slot.

### C4. Extreme usage/window values remain safe

Backend token counters and aggregate window totals now saturate instead of overflowing.
A very large reported count cannot wrap into apparent free capacity or panic a debug
build. Window filtering compares elapsed ages to the configured duration instead of
subtracting an arbitrary u64 duration from Instant, which could panic for huge windows.
The regression supplies u64::MAX usage and a u64::MAX window, verifies saturated totals,
and requires the exhausted budget to stay closed.

### C5. Limiter diagnostics avoid nested lock ordering

Configuration and usage are copied/read before taking statistics locks; the refusal path
does not hold the statistics mutex while awaiting the configuration read lock. This removes
the previous inversion with statistics readers and a queued configuration writer. Network
and model waits continue to happen outside these locks. An independent subagent reviewed
permit release and lock lifetimes in the final diff.

Files for C3–C5: `src/llm/rate_limiter.rs`, `tests/llm_rate_limiter_test.rs`.

### C6. Credential redaction includes bearer and header credentials

The shared redactor covered passwords/secrets/API keys but missed access/refresh/identity/
auth tokens, generic `token`, HTTP Authorization/Proxy-Authorization and cookies. It now
covers those names, camelCase token spellings and hyphenated header names. Counters such
as `input_tokens`/`max_tokens` and endpoint metadata such as `token_url` remain useful.
Redaction still copies values for display: it does not alter credentials sent on the wire.
Null values and the existing recursion-depth guard retain their behavior.

Files: `src/utils/redact.rs`, `tests/secret_redaction_test.rs`.

### C7. Reference extraction treats block contents as opaque

Nested tag-like content could produce overlapping removal ranges. Removing the inner
range invalidated the outer indices and could panic while processing model text.
Matched outer reference bodies now remain opaque, including nested tags and Unicode.
Extraction also tracks quoted JSON content and escaped quotes, preserving embedded
placeholders such as `"prefix <script1> suffix"` before an external block definition.

### C8. Reference resolution is literal, deterministic and valid JSON

Repeated HashMap-driven replacements could interpret placeholders inside an inserted
script, making results depend on iteration order. One regex replacement pass touches only
the original placeholders. A shared lazily initialized regex avoids recompilation on every
extraction/detection call. JSON serialization escapes the entire ASCII control-character
range, not just newline/tab/quote/backslash, so NUL and other controls no longer corrupt
resolved action JSON.

Regressions cover nested blocks, Unicode, every JSON control character, nonrecursive
replacement, mixed inline placeholders and escaped JSON quotes through the complete
extract/resolve/parse path.

Files for C7–C8: `src/llm/reference_parser.rs`, `tests/reference_parser_test.rs`.

### C9. Tiny conversation windows and empty messages respect storage limits

The per-message cap had a 256-byte minimum regardless of the configured history window,
and appended a truncation marker outside the cap. A small/zero history could therefore
exceed its own limit. The cap now fits the configured window and includes the marker when
space permits. Empty messages no longer accumulate indefinitely while consuming zero
bytes of the eviction budget. UTF-8 truncation remains character-safe.

Tests exercise zero/tiny/ordinary limits with mixed Unicode and repeated long messages,
then add thousands of empty messages and require history to stay empty.

Files: `src/llm/conversation_state.rs`, `tests/prompt_growth_test.rs`.

### C10. Fuzz CI includes every declared harness

`zabbix_packet`, `gearman_packet` and `nostr_message` were declared in the fuzz manifest
but absent from the dispatched matrix. All 29 declared targets now appear. A Rust test
parses the actual TOML and YAML, requires equal nonempty target sets and rejects duplicate
matrix entries. The test is explicitly wired into the blocking source-ratchet job.
Fuzz searches themselves were not launched during this audit.

### C11. Workflow inputs remain data instead of shell source

Manual nightly eval and soak inputs were directly interpolated into generated shell text.
The fuzz budget took the same unsafe route indirectly through a GitHub env expression.
They now enter shell scripts through environment variables and quoted argument expansion;
the protocol list is split into a Bash array without evaluating its contents. Fuzz budgets
must be positive decimal integers. Matrix target names remain workflow-owned constants.
The soak's descriptor limit now runs in the same shell as the workload, so it actually
applies; the previous separate step's shell limit was discarded on exit. Soak commands
also use the lockfile explicitly.

Files for C10–C11: `.github/workflows/{ci,fuzz,nightly-eval,nightly-soak}.yml`,
`tests/audit_ci_coverage_test.rs`. These workflows were edited and locally checked;
none was dispatched and no live eval was run.

### C12. Build wrappers execute the intended command

`cargo.sh` used post-increment under `set -e`; the first successful cleanup returned an
arithmetic status of one and terminated before Cargo ran. Assignment arithmetic fixes the
control flow. `cargo-isolated.sh` now respects an explicitly empty RUSTC_WRAPPER as a
request to disable wrapping. The sccache wrapper resolves the root wrapper through the
correct two-parent path and its missing-sccache fallback actually disables sccache.

### C13. Generated cache credentials are quoted literal values

The interactive cache setup scripts wrote credential input into double-quoted shell source
and later sourced it. Embedded quotes, command substitutions or backticks could change
its meaning on reload. Bash printf `%q` writes literal values. A restrictive umask applies
before file creation; the existing chmod remains. Reads preserve backslashes, and the
Redis credential prompt suppresses echo. No real credential setup was executed.

Five Python fixture tests use copied wrappers, stub Cargo/ps commands and temporary config
paths. They prove cleanup reaches Cargo, explicit wrapper disablement, fallback path and
argv preservation, failed Cargo status propagation, and exact round-trip of shell-special
credential text without executing it. They also assert 0600 permissions. The fixture suite
is wired into CI and passed locally.

Files for C12–C13: root Cargo wrappers, `scripts/sccache/*.sh`,
`tests/build_scripts_test.py`.

## Validation and review evidence

- Baseline offline CPU-feature `cargo check --tests` passed in 6m34s before the combined
  regression runs. It compiled tests; it did not execute the live-model suites.
- All 40 tracked shell/Python/JavaScript sources passed Bash syntax, Python AST or Node
  syntax checks. Details: `script-syntax-results.json`.
- Server and client Beta-evidence static checks both passed. Their reported metadata totals
  were server 60 Beta / 105 Experimental / 4 Stable; client 13 Beta / 89 Experimental.
  These are local metadata audit results, not fresh interoperability certification.
- Build script fixture suite: 5 passed.
- Root parser/persistence/limiter changes received independent subagent review; the review
  identified inline JSON extraction and camelCase redaction omissions, which were fixed.
- Native regression, source-ratchet, clippy/format and server checks are recorded centrally
  in the consolidated summary. This section does not infer execution from source review.

## Remaining shared-core risks and boundaries

1. Circuit-breaker acquire clears its open timestamp when allowing a probe; concurrent
   requests can then pass before the probe resolves. A cancellation-safe probe lease needs
   a coordinated API/call-site change and concurrent cancellation tests.
2. Dynamic limiter semaphore replacement can temporarily overlap old and new permit pools.
   User-wait statistics also need cancellation accounting. This batch fixes token-budget
   sequencing and arithmetic, not all hot-reconfiguration semantics.
3. Conversation content caps do not constitute a total heap budget: parsed action metadata,
   tool metadata and protocol documentation tracking are separate allocations. Huge nested
   in-memory JSON remains a different problem from the tested content-byte limit.
4. Several HTTP/model/tool response paths still buffer full response bodies before trimming
   display text. Streaming byte limits and timeout semantics need a consistent API across
   those paths. Local file-read size checks can also race file growth.
5. `Settings` uses a legacy `~/.netget` file while NetGetConfig expects a `~/.netget/`
   directory. A non-destructive migration with rollback and explicit path fixtures is
   required to reconcile existing installations. No migration or direct home-configuration
   edit was attempted; normal application test startup may read its existing settings.
6. Save files are not yet atomically replaced on all targets, and the save representation
   does not capture every runtime capability such as pipes/easy instances/scheduled tasks.
   The added handler/feedback round-trip is intentionally narrower than a full session image.
7. The isolated-kill helper still targets legacy per-session directories while current build
   wrappers share a target directory. It was inspected, not run or broadened to risk killing
   unrelated builds. Current compilation was managed through its exact tool session.
8. GPU/embedded inference, hardware-backed protocols, mobile/Windows builds, real browsers
   with model runtimes, production services and release publishing were not executed.
9. Documentation under archive and historical evaluation outputs is evidence of earlier
   work, not refreshed measurement. No external claims/prices/library releases were inferred
   from it. Dependency versions and lockfiles were not upgraded speculatively.

All remaining items are follow-up findings, not passed checks. Detailed per-protocol and
per-surface limitations are preserved in the other six reports.
