# Pseudo-Terminal (PTY) E2E Tests

## Strategy

**One file, `e2e_test.rs`, 1 test** — covering the pseudo-terminal server.
This doc did not name it until 22 September 2026, which is the starkest form of a test
directory's doc drifting: a single file, and the doc describing it without ever saying so.

Black-box, prompt-driven, validated against a **real terminal client** — never
NetGet-against-NetGet. The test process opens the slave PTY device (through the server's
`link_path` symlink) with `std::fs::OpenOptions` and drives it exactly as `screen`/`cat`/a shell
would: it reads what the model puts on the terminal and types input back.

## What the test asserts (bytes on the terminal, not "it opened")

`test_pty_prompt_and_command` starts the NetGet binary with a mocked LLM and drives one shell
role-play:

1. Mock rule #0 answers startup with an `open_server` for `PTY`, `startup_params.link_path` +
   `send_first: true`.
2. Mock rule #1 answers the `pty_opened` event with `write_pty_output` `netget$ `. The test opens
   the slave and reads it — asserts the banner (master→slave direction).
3. The test writes `whoami\n` to the slave (slave→master). Mock rule #2 matches
   `pty_input_received` (`data` contains `whoami`) and answers `root\n`. The test reads it —
   asserts the command answer.
4. `verify_mocks()` confirms all three rules fired exactly once.

## Why raw mode matters for the assertions

The slave is `cfmakeraw`'d by the server, so there is no local echo (the test does not read back
its own `whoami`) and no canonical line buffering (the newline-less `netget$ ` prompt arrives
immediately). Reads are therefore exactly the model's output.

## Why blocking I/O on `spawn_blocking` + timeout

PTY `read()` blocks until data, so the whole open/read/write/read dance runs on a blocking thread
wrapped in a 15s `tokio::time::timeout`. The server holds its own slave fd open, so the master
never returns EIO while the client is attached, and the banner written on connect is buffered
until the client reads it.

## LLM call budget

1 test x 3 mocked LLM calls (startup + pty_opened + one input) = **3 calls**, under the 10-call
budget.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features pty --test server pty -- --test-threads=100
```

## Notes / known limitations

- `link_path` is `./tmp/netget-test.pty` relative to the NetGet process cwd. The test
  `mkdir -p ./tmp` and removes the stale symlink before/after.
- Not covered: window-size / SIGWINCH, ANSI escape sequences via hex encoding, multiple concurrent
  clients, and client detach/reattach (the server tolerates it via the held slave fd, but it is
  not asserted here).
