# FTP Server E2E Tests

## Test Strategy

Black-box testing using raw TCP connections to verify FTP protocol responses.

## LLM Call Budget

- **Target**: < 5 LLM calls per test
- **Current**: 1 LLM call per test (server setup only)

## Test Cases

| Test | Description | LLM Calls | Expected Runtime |
|------|-------------|-----------|------------------|
| `test_ftp_greeting` | Verify 220 greeting on connect | 1 | ~2s |
| `test_ftp_user_pass` | Verify USER/PASS authentication flow | 1 | ~3s |
| `test_ftp_pwd_quit` | Verify PWD and QUIT commands, and that the connection closes after QUIT | 1 | ~3s |
| `llm_failure_test::test_ftp_answers_421_when_greeting_llm_fails` | 421 + close when the greeting handler fails | 1 | ~2s |
| `peer_injection_test::injected_ftp_response_reaches_raw_peer_and_close_sends_eof` | `send_to_peer` writes an injected reply to a raw socket, counters move, `close_connection` sends EOF | 0 | ~1s |
| `decision_tag_test::test_ftp_backend_failure_is_tagged_and_never_answers_2xx` | USER during a backend outage gets 421 and never a 2xx; log carries `decision=fail_closed_llm_*` and the greeting carries `decision=model_answer` with its reply code | 2 | ~3s |
| `decision_tag_test::test_ftp_close_connection_is_tagged_model_reject` | `close_connection` ends the session with no reply and is tagged `decision=model_reject`, distinct from the 421 above | 3 | ~3s |

`decision_tag_test` is the fail-open guard. FTP's worst possible defect would be a failure
path that answers `230 User logged in`, which would make an LLM outage an authentication
bypass; the first test asserts from the wire that no 2xx can come back when the backend is
down. See `src/server/ftp/CLAUDE.md`, "Failure behaviour", for the full outcome table.

`test_ftp_user_pass` and `test_ftp_pwd_quit` had no mock rules for the `ftp_command` events,
so the server 421-closed the greeting and the tests bailed out before `verify_mocks` — they
failed for as long as the server refused unanswered greetings. They now mock each command and
assert the reply codes; `read_reply` panics on silence instead of printing a note.

## Mock Configuration

All tests use mock LLM responses via `.with_mock()` builder:
- No actual Ollama required for CI
- Deterministic responses for predictable testing
- Mock expectations verified with `.verify_mocks().await?`

## Running Tests

```bash
# Run with mocks (default, no Ollama needed)
./test-e2e.sh ftp

# Run with real Ollama
./test-e2e.sh --use-ollama ftp

# Run with cargo
./cargo-isolated.sh test --no-default-features --features ftp --test server::ftp::test
```

## Known Issues

1. **Control Channel Only**: Tests only verify FTP control channel responses
2. **No Data Transfer Tests**: LIST/RETR/STOR data transfer not tested (no data channel)

## FTP Response Codes Tested

- 220: Service ready (greeting)
- 221: Service closing (QUIT)
- 230: User logged in (after PASS)
- 257: Pathname created/current directory (PWD)
- 331: User name okay, need password (USER)

## `connection_bounds_test.rs` — 5 in-process tests, **0 LLM calls** (the backend is a dead port)

The read deadlines in `src/server/ftp/mod.rs` — `FIRST_COMMAND_READ_TIMEOUT` (300s) and `IDLE_BETWEEN_COMMANDS_TIMEOUT` (300s) — driven from the wire. No
mock: these assert on *clocks*, not on answers, and a reachable backend would only add noise.
Loopback only.

**What each test is for.** A peer that has connected and said nothing must eventually be let go
of, because nothing else in the process will close that socket. A connection whose answer is
parked for a human must **not** be let go of, which is what stops the lazy fix of wrapping the
answer in the deadline as well as the read. The two bounds are different claims, so one test
drives a connection into the post-answer state and checks it is governed by `idle_timeout_secs`
rather than by the first-byte one; the connection past `MAX_CONNECTIONS` is answered `421` and then a clean EOF.

**The last test is the regression, and it is deliberately the slow one.** The first-byte bound
was 60 seconds, and NetGet's own FTP client is precisely a peer that bound stranded:
`src/client/ftp/mod.rs` reads the `220` in its read loop and writes nothing until an action or `[ send message ]` says to, and a client made from the dashboard is routed `*` → manual — so it connects and
waits for a person, who gets 300 seconds (`src/state/intercepts.rs`). It is now 300s. Proving
that means holding a silent peer open **past 60 seconds with no startup parameters passed at
all**, so the wait cannot be made cheaper than the claim.

Every other test passes a short override instead of waiting the default out, which is also what
proves `first_byte_timeout_secs` and `idle_timeout_secs` are read rather than merely declared:
a parameter that was ignored would leave the 300-second default in force and the test would time
out. Each bound was verified by removing it and watching its test fail, and the default was
verified by putting 60 back and watching the regression test fail.

## `one_reply_test.rs`

One command, one completion reply: a greeting answered with three `220`s reaches the client as
the first only, `USER` is answered by its own `331` (the rule matches only when the event's
`answer_with` names 331), and the drop is logged `decision=duplicate_response_dropped`. Verified by
removing the check in `OneReply::admit`.
