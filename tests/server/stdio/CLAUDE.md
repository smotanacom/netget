# stdio (pipe-filter) E2E Tests

## Strategy

stdio owns the process's own stdin/stdout, so the shared harness (prompt-as-arg, stdout read as
logs, stdin never piped) cannot drive it. This test therefore **spawns the NetGet binary directly
as a real child** (`env!("CARGO_BIN_EXE_netget")`) with piped stdin/stdout — the actual
`prog | netget | prog` use case — points it at an in-process `MockOllamaServer`, feeds a line on
stdin, and asserts the model's bytes on the child's real stdout. LLM interaction is still asserted
via `mock_server.verify_calls()`.

## What the tests assert

`test_stdio_pipe_filter`:

1. Launches NetGet via **actions-JSON** (`{"actions":[{"type":"open_server","base_stack":"stdio"}]}`
   as the trailing arg).
2. Writes `hello\n` to the child's stdin.
3. The mock matches the `stdio_input_received` event (`data` contains `hello`) and answers
   `write_stdout` `HELLO\n`.
4. Reads the child's stdout and asserts it contains `HELLO` (tolerant `contains`, stderr nulled).
5. `verify_calls()` confirms the one event call.

`test_stdio_server_flag_clean_stdout` (the IMPROVEMENTS-item-12 regression test):

1. Launches via **`--server stdio -- <instruction>`** — the recommended pipe-filter shape — with a
   **live** piped stdin. This exercises the bootstrap-does-not-drain-stdin fix: if it regressed,
   the process would hang at startup and the model would never be called, failing the test.
2. Writes `hello\n`; the same mock answers `write_stdout` `HELLO\n`.
3. Captures stdout AND stderr separately, and asserts:
   - stdout is **pristine** — the `HELLO` payload and nothing else; **no** NetGet status vocabulary
     (`[SERVER]`, `[STATUS]`, `Using model`, `started`, `Waiting for connections`, `is running`,
     `Server stopped`).
   - the status text (`Using model` / `Waiting for connections`) appears on **stderr**, proving it
     was rerouted, not dropped.
4. `verify_calls()` confirms the one event call.

`test_stdio_llm_failure_writes_category_to_stderr_only`:

1. Launches the same `--server stdio` shape, but the mock answers `stdio_input_received` with
   prose the action parser cannot use, so `call_llm` returns `Err` after its retries.
2. Asserts a category-only line (`could not be processed` / `backend at capacity`) reaches
   **stderr** — a backend failure must not be silence.
3. Asserts that line does **not** reach stdout: the payload stream stays uncorrupted.
4. Asserts the `netget: ` line carries none of the historically leaked tokens (`✗`, `retries`,
   `http://`, `11434`, `qwen`, `/Users/`) — the per-protocol counterpart of
   `tests/wire_failure_test.rs`.

## Bootstrap facts (both now fixed — see `src/server/stdio/CLAUDE.md`)

- **stdin is not drained when a trailing prompt or `--load` is present.** `Args::get_actions_json`
  / `get_prompt` (`src/cli/args.rs`) only call the blocking `piped_stdin()` when stdin is genuinely
  the input source (no trailing prompt). `--server` returns even earlier, before prompt resolution.
  So `--server stdio -- ...` and `netget "be a filter"` both leave a live stdin for the server,
  while `cat prompt.txt | netget` (no trailing prompt) still reads stdin.
- **status is routed off stdout for stdio** (to stderr), so the payload stream is clean. Hence the
  second test can assert exact cleanliness rather than tolerating interleaving.

## LLM call budget

2 happy-path tests x 1 mocked LLM call each (the one stdin line; startup is deterministic), plus
the failure test's single event, which the retry loop repeats a few times before `call_llm` gives
up = **~3-5 calls**.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features stdio --test server stdio -- --test-threads=100
```

## Notes

- `test_stdio_pipe_filter` nulls stderr and uses `contains`; `test_stdio_server_flag_clean_stdout`
  pipes stderr and asserts stdout is exactly the payload.
- Not covered: `stdio_started` (send_first), `stdio_input_closed` (EOF), `write_stderr`,
  `close_stdio`, and the refusal paths (TTY / `--mcp`), which are enforced in `spawn()`.
