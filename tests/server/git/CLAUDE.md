# Git Smart HTTP Protocol E2E Tests

## Test Overview

Tests the Git Smart HTTP server against real Git clients: the system `git` binary for
clone/fsck, and `reqwest` for direct protocol-endpoint inspection. All five tests are
fully mocked (no Ollama required) and assert genuine protocol correctness — a real
`git clone` succeeding with exact file content, not just an HTTP 200 or a non-panicking
response.

**Status (as of the `git_repository` redesign, commit `67c14fd6`)**: all 5 tests pass. See
"History" below for what was wrong before and why it stayed green.

## Actions under test

The server implementation exposes exactly two sync actions, both documented in
`src/server/git/actions.rs`:

- `git_repository` — declares `files` ({path, content, executable?}), `branch`,
  `commit_message`, `author_name`, `author_email`, `timestamp`. The server computes every
  object ID and builds a real pack file; the model/mock never supplies bytes or hashes.
- `git_error` — `message`, `code` (HTTP status).

Two events are raised: `git_info_refs` (`GET .../info/refs?service=git-upload-pack`) and
`git_upload_pack` (`POST .../git-upload-pack`). **A clone is these two requests answered
separately** — if a `git_repository` mock for one event doesn't return byte-identical
content to the mock for the other, the SHA advertised by `info/refs` won't match the
commit actually inside the pack, and `git clone` fails with "did not send all necessary
objects". Every mock pair in this suite that answers both events for the same repository
uses one `serde_json::json!(...)` value, `.clone()`d into both `respond_with_actions(...)`
calls, specifically to guarantee that.

## Mocking strategy

Mocks match on `.on_event("git_info_refs" | "git_upload_pack")` plus
`.and_event_data_contains("repository", "<name>")`, **not** on prompt substrings. The
event id in `LlmContext.event_type` is the raw `EventType` id (`git_info_refs`,
`git_upload_pack` — see `tests/helpers/mock_ollama.rs::extract_event_type` and
`src/llm/prompt.rs::build_event_trigger_message_with_id`), so this is robust to wording
changes in event descriptions or prompt templates. Matching on prompt text (e.g. "Git
client is requesting references") is what broke this suite in the first place — that
exact phrase never existed in any prompt template shipped alongside `git_advertise_refs`
in this codebase; it was stale even at the time it was written. Match on the event id.

## This suite is the Beta rating

`git` is the only Beta protocol in the version-control family, and this file is the
evidence. Two properties are what make it evidence rather than decoration, and both
must survive any edit:

- **Not `#[ignore]`d.** Every test runs in an ordinary `cargo test`.
- **It fails when `git` is absent.** `run_git_command` spawns the binary and returns
  `Err` if the spawn fails; every caller propagates with `?`. There is no
  `SKIP: git is not installed` branch. Root `CLAUDE.md` records four protocols
  (`kubernetes`, `oci_registry`, `maven`, `websocket`) held back from Beta precisely
  because their real-client tests return `Ok(())` when the binary is missing, which
  is a silent pass. Do not add such a branch here.

## Readiness, not sleeps

`start_netget_server` returns when startup is *parsed*, not when the listener is
bound, so each test waits for the server's own `Git server listening on` log line
before the first request, and calls `wait_for_mocks(30)` before `verify_mocks()`.
They previously slept a fixed 500ms and verified immediately: enough when a test
runs alone, not when a hundred run together, and a connection refused rather than a
slow test when it is not.

## LLM Call Budget

- `test_git_clone_with_system_git()`: 1 mock call (startup) + 2 mock calls (info/refs,
  upload-pack) = 3
- `test_git_info_refs_endpoint()`: 1 (startup) + 1 (info/refs) = 2
- `test_git_repository_not_found()`: 1 (startup) + 2 (one 404, one success) = 3
- `test_git_multiple_repositories()`: 1 (startup) + 4 (info/refs × 2 repos, upload-pack ×
  2 repos) = 5
- `test_git_with_scripting()`: 1 (startup only — see below)

**Total: 14 mocked calls, 0 real Ollama calls.** All of these are in-process mock
responses (`MOCK_OLLAMA_BASE_URL`), not network calls to a model; "budget" here is about
keeping the mock surface small and each rule individually asserted, not latency.

## Scripting

`test_git_with_scripting()` configures the server's `open_server` action with an
`event_handlers` entry of `type: "script"` (Python), per `src/scripting/CLAUDE.md`. Once
that handler is registered, **`git_info_refs` and `git_upload_pack` never reach the mock
LLM at all** — `src/llm/action_helper.rs::call_llm_with_actions` tries the configured
event handler first and only falls back to the LLM if none matches or the interpreter is
unavailable. Accordingly:

- Only one mock rule exists in this test (server startup). No rule is registered for the
  two network events.
- If scripting silently failed to route (e.g. `python3` missing, so
  `ScriptingEnvironment::is_available` returns false) the request would fall back to the
  LLM, hit "no mock rule matched", and the test would fail loudly — not pass vacuously.
  This is deliberate: the original version of this test defined mock rules for the
  network events *and* used scripting, so a routing regression could never have been
  caught by it (see History).
- The script emits a fixed `git_repository` action regardless of which event or
  repository it was called for; that's what the "same content for both events" rule
  requires, and it holds here trivially since the script ignores its input.
- The test also performs one real `git clone` against the scripted repository, not just
  three timed HTTP GETs, so it exercises the same pack-construction and
  `SHA1(info/refs) == SHA1(git-upload-pack)` guarantee as the other tests, driven from a
  script instead of a mock.

Requires `python3` on `PATH`; if it's absent the test will fail with "no mock rule
matched" per the paragraph above, rather than hang or silently pass.

## Client Library

- **`git` (system command)** — real Git client for `clone`, `fsck`, `log`, `branch`,
  `show`. Used wherever an assertion can be made through Git itself instead of by
  reimplementing SHA/pack logic in the test (`test_git_clone_with_system_git` and
  `test_git_with_scripting` both run `git fsck --full` / `git log -1 --format=%s` on the
  clone rather than only checking the command's exit code).
- **`reqwest`** — direct HTTP inspection of `/info/refs`, used where the point is the wire
  format itself (`test_git_info_refs_endpoint`'s pkt-line parser) rather than what a full
  client does with it.
- **`tempfile`** — isolated per-test clone directories, auto-cleaned.

`git2` is not used; the system binary is more representative of what an actual user
invokes and (unlike `git2-rs`) needs no extra crate wiring to add here.

## Test Cases

### `test_git_clone_with_system_git`

Real `git clone` of a repository with a top-level file, a nested path (`src/main.rs`),
and an executable file (`bin/run.sh`, `executable: true`) — the three properties
`src/server/git/CLAUDE.md` calls out as verified against the real `git` binary. Asserts,
in order: the clone command itself succeeds (no "acceptable failure" branch — a failure
here means clone doesn't work, full stop); `git fsck --full` passes; `git log -1
--format=%s` and `git branch --show-current` match what the mock specified; `git show
HEAD:README.md` and the on-disk file content match exactly; `src/main.rs` content matches
exactly; and on Unix, `bin/run.sh`'s mode has an executable bit set after checkout.

### `test_git_info_refs_endpoint`

No clone — a direct `GET /info/refs?service=git-upload-pack`, checked for exact
`Content-Type`, then decoded with a hand-rolled pkt-line parser
(`parse_pkt_lines` in the test file) that itself asserts framing invariants (valid hex
length header, no truncation, no trailing bytes) rather than merely not crashing on
malformed input. Structural assertions on top: exactly 5 pkt-lines
(service-announcement, flush, HEAD-ref, branch-ref, flush), the HEAD line has the form
`<40-hex-sha> HEAD\0...symref=HEAD:refs/heads/main...`, and the branch line repeats the
identical SHA. Does not assert an exact SHA value (that would duplicate `pack.rs`'s
hashing in the test); it asserts the shape the Smart HTTP spec requires.

### `test_git_repository_not_found`

Two repositories configured on the same server: `nonexistent` (mocked with `git_error`,
code 404) and `existing-repo` (mocked with `git_repository`). Asserts the missing
repository returns exactly 404 with the configured message in the body, **and** that the
repository which does exist returns 200 — proving the server discriminates by name rather
than refusing everything, which a single-repository test cannot distinguish from a
generally-broken server.

### `test_git_multiple_repositories`

Two repositories (`frontend`, `backend`), each with a uniquely-named file and unique
content, each mocked identically across `git_info_refs`/`git_upload_pack`. Both are
**actually cloned** (not just GET-compared by response length, as the pre-redesign
version did) into separate temp dirs; asserts each clone has its own file with exact
content and does *not* have the other repository's file.

### `test_git_with_scripting`

See "Scripting" above. Three timed GETs (<100ms, proving no LLM round trip occurred) plus
one real clone (proving the script-driven repository is actually valid, not just that the
server answered fast).

## History

Before this rewrite, the suite mocked two actions — `git_advertise_refs` and
`git_send_pack` — that commit `67c14fd6` removed from `src/server/git/actions.rs`
entirely, replacing them with `git_repository`/`git_error`. `git_send_pack` had asked the
model for a base64-encoded pack (`pack_data`), which is both unimplementable by an LLM
(a valid pack needs zlib streams and a SHA-1 trailer) and forbidden by the root
`CLAUDE.md`'s "no bytes in action parameters" rule; `git_advertise_refs` had the model
invent SHAs with no relationship to `git_send_pack`'s (nonexistent) pack contents. A real
`git clone` against that design could never have succeeded — the old
`test_git_clone_with_system_git` in fact accepted clone failure as "acceptable for MVP"
and asserted nothing about content on success.

Separately, the old mocks matched on prompt substrings — `"Git client is requesting
references"`, `"Git client is requesting a pack file"` — that do not appear anywhere in
this codebase's prompt templates or event descriptions (see "Mocking strategy" above);
`git log -p` shows they were already stale at the commit that introduced them. Once
`67c14fd6` changed the event descriptions, every one of those rules stopped matching,
`server.verify_mocks()` failed on all five tests, and the whole suite went red — which is
how this rewrite was triggered. The current suite matches on event id instead, which is
the same signal the mock harness itself uses to classify a request
(`tests/helpers/mock_ollama.rs`), so it cannot drift out of sync with prompt wording
again.

## References

- [Git Smart HTTP Protocol](https://git-scm.com/docs/http-protocol)
- [Git Pack Protocol](https://git-scm.com/docs/pack-protocol)
- [Pkt-Line Format](https://git-scm.com/docs/protocol-common#_pkt_line_format)
- `src/server/git/CLAUDE.md` — implementation, what's verified against the real `git`
  binary, and what's deliberately not implemented (push, tags, deltas, shallow clones).
