# Named Pipe (FIFO) E2E Tests

## Strategy

Black-box, prompt-driven, validated against a **real independent FIFO peer** — never
NetGet-against-NetGet. The test process itself opens the FIFO paths with `std::fs` and drives
them exactly as a shell `echo > fifo` (writer) and `cat fifo` (reader) would.

## What the test asserts (protocol-level bytes, not "it opened")

`test_named_pipe_request_response`:

1. Starts the NetGet binary with a mocked LLM. Mock rule #0 answers the startup instruction with
   an `open_server` for `NAMED_PIPE`, passing `startup_params.pipe_path` and `response_pipe_path`.
2. A real `std::fs` **writer** opens the input FIFO and writes `PING\n`.
3. Mock rule #1 matches the `named_pipe_data_received` event (`data` contains `PING`) and answers
   with `write_named_pipe_data` `PONG\n`.
4. A real `std::fs` **reader** opens the response FIFO and reads — the test asserts it carries
   `PONG`.
5. `verify_mocks()` confirms both rules fired exactly once.

## Why the blocking I/O is on a `spawn_blocking` + timeout

FIFO `open()` blocks until a peer is present, and `read()` blocks until data arrives, so the
writer/reader dance runs on a blocking thread wrapped in a 15s `tokio::time::timeout`. The server
holds both FIFOs open `O_RDWR`, so the test's opens return immediately and the `PONG` the server
writes is buffered in the kernel until the test reads it.

`test_named_pipe_llm_failure_answers_with_a_category_only`:

1. Same startup, but **no rule matches the `named_pipe_data_received` event**, so the mock
   answers HTTP 500 and `call_llm` returns `Err` after its retries — the real backend-failure
   path, not a simulated one.
2. The real `std::fs` reader must still get bytes: an attributed `netget: …` line carrying a
   `WireFailure` category. Silence would hang a FIFO reader forever (`cat` has no timeout).
3. It also asserts the line contains none of the tokens that historically leaked onto the wire
   (`✗`, `retries`, `http://`, `11434`, `LLM`, `ollama`, `/Users/`), so a future edit cannot
   reintroduce the interpolated-error bug through this path.

Unmatched calls do not fail `verify_mocks()` — it checks each declared rule's own call count —
so the deliberate miss is safe.

## LLM call budget

2 tests: 2 mocked calls in the round-trip (startup + one event), plus 1 mocked startup call and
a handful of deliberately-unmatched retry calls in the failure test = **well under** the 10-call
budget.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features named_pipe --test server named_pipe -- --test-threads=100
```

## Notes / known limitations

- FIFO paths are `./tmp/netget-test-fifo.{in,out}` relative to the NetGet process cwd (the repo
  root under cargo). The test `mkdir -p ./tmp` and removes stale nodes before and after.
- Not covered: read-only mode (no `response_pipe_path`), hex-encoded binary payloads, and
  concurrent writers. The round-trip is the one that proves the plumbing both ways.
- The failure test uses its own `./tmp/netget-test-fifo-fail.{in,out}` nodes so the two tests
  cannot collide when run in parallel. Both sets are still fixed paths, so two concurrent runs
  of this suite would.
