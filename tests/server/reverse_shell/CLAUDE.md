# Reverse-Shell Listener E2E Tests

Two tests in `test.rs`, both driving the real `netget` binary with a **raw TCP client** — exactly
what an operator's `nc`/`socat` is. There is no protocol framing, so the client is just a
`TcpStream`.

## Strategy

**Assert the model-supplied output on the wire.** The emulation's whole point is that the shell
output is fictional text the model produced (NetGet executes nothing), so the test sends a command
line and asserts the mocked model's answer appears. `read_until` accumulates bytes across reads
with a 15 s timeout per read.

**Assert the fail-closed path structurally.** The second test answers the command event with only
`show_message` (a valid action that yields no protocol output). The server must treat this as "no
usable answer" and **half-close** the socket; the test asserts a subsequent read returns EOF (0
bytes). Equating "no answer" with "empty output, keep going" would be the fail-open shape that bit
OAuth2 — an LLM outage would look like a working silent shell.

**Assert the failure notice carries a category and nothing else.** The same test collects the
bytes written before the FIN and asserts they contain `[netget] request could not be processed`
— the `WireFailure::Unavailable` text — and none of the tokens that leaked in the incident
`tests/wire_failure_test.rs` documents (`retries`, a backend URL, a model name, a `/Users/`
path, `LLM`, `ollama`). A dropped shell session that says nothing is indistinguishable from the
far-end implant dying; a session that says *why* in netget's own words is the bug that guard
exists to stop. The wording, not just the presence, is what the assertion pins.

**Event rules before the instruction rule.** Rules match in order; `on_instruction_containing`
would otherwise answer a network event with `open_server`.

## LLM call budget

**Total: 6.**

| Test | Calls |
|---|---|
| `test_reverse_shell_command_output` | 3 (start + session_opened + command) |
| `test_reverse_shell_fails_closed_on_no_answer` | 3 (start + session_opened + command) |

Every rule is `expect_calls(1)`, so a call the server should not have made fails the run.

## Not covered

PTY/terminal semantics, multiple concurrent operators, script/static handler mode, and a real
operator toolchain other than a raw socket. Binds to `127.0.0.1` only.
