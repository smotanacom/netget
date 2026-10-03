# DICT tests

Run everything:

```bash
./cargo-isolated.sh test --no-default-features --features dict --test server -- dict:: --test-threads=100
```

## Strategy

The evidence is `real_client_test.rs`, which points the real `dict(1)` client at the server.
Everything else is NetGet reading bytes NetGet wrote, and exists for what `dict(1)` cannot send
or cannot see: malformed commands, pipelining, bounds, failure paths, the model-call count.

Most suites start the server **in process** through `ServerForm` (the dashboard's path) with a
static or Python script handler and a dead model endpoint, so they are deterministic and cost no
model call. `e2e_test.rs` and one case in `real_client_test.rs` use the spawned binary with the
mock model.

## Files

| File | What it proves | Model calls |
|---|---|---|
| `common.rs` | helpers: in-process server, raw line peer, `DICTIONARY_SCRIPT` (a deterministic Python dictionary whose `glimmer` definition contains a line starting with `.` and a line that is `.`) | — |
| `real_client_test.rs` | `dict(1)` parses and prints: two definitions with the stuffed lines un-stuffed and the block not ended early (`dict glimmer`); `-m -s prefix`; `-D`; `-S`; `-i fantasy`; `-I`; `-M` (MIME header + blank line precede each definition — dict prints it raw); `nosuchword` → "No definitions found", non-zero exit; and one case where a **mocked model** writes the definition. Fails, never skips, without `dict`. | 2 (mocked case) |
| `e2e_test.rs` | one raw-socket session against the mocked model: banner shape, CLIENT, DEFINE/MATCH/SHOW DB/SHOW INFO exact bytes, 500/501/502/503, STATUS, HELP, a pipelined `STATUS`/`OPTION MIME`/`QUIT`, and that everything NetGet answers itself costs no model call (`expect_calls`) | 5 |
| `wire_test.rs` | proptests: a text block round-trips through RFC unstuffing and never ends early, no line exceeds 1024; a quoted parameter parses back to itself; an atom is always one parameter; the MIME preface lands once per block even when text starts with `151 `. Plus fixed cases for `split_args`, `parse_command`, and one rendered definition byte for byte. | 0 |
| `connection_bounds_test.rs` | 1024-byte line answered, 1025 (with 200 pipelined commands behind it) refused with `500` + clean close + `decision=fail_closed_line_too_long` and no handler; 64 KiB without newline refused; first-command deadline; idle deadline (a different number); a `manual`-parked command outlives both; the 257th connection gets `420` and the slot returns | 0 |
| `llm_failure_test.rs` | dead backend → `420` + close + `decision=fail_closed_llm_error`, no leaked error text; empty handler → `420` + `decision=model_silent`; a `152` reply to DEFINE → `420` + `decision=fail_closed_mismatched_reply`; a model `550` is sent and logged `decision=model_reject`, and the session continues | 0 (the failing one never reaches a model) |
| `peer_inject_test.rs` | `send_to_peer` writes a framed, dot-stuffed `114` block, and `close_connection` reaches the peer as EOF | 0 |

## How each guard was shown to matter

All at once, in one mutated build, then restored:

| Guard removed | Test that failed, and how |
|---|---|
| both `MAX_LINE_BYTES` checks in `LineReader::next_line` | `a_1024_byte_line…` (1025-byte line answered `150`), `a_line_with_no_newline…` (no reply in 10 s) |
| `tokio::time::timeout` around the read | `a_greeted_peer…` (still open after 46 s), `an_answered_peer…` (never closed) |
| the connection cap (limiter ×1000) | `the_connection_past_the_cap…` (over-cap peer neither answered nor closed) |
| dot-stuffing in `wire::stuff` | both proptests on blocks, the byte-exact rendering, peer inject, and **the real `dict(1)`**: `Unexpected status code 600 (line), wanted 151`, exit 30 |
| the reply-fits-command check | `a_reply_that_does_not_fit…` (the `152` reached the client) |
| `linger` after the final reply (separately) | `a_1024_byte_line…`: `ConnectionReset` instead of the `500`, because 200 pipelined commands were still unread at close |

## Notes

- No pcap oracle: tshark has no DICT dissector.
- `dict(1)` is at `/opt/homebrew/bin/dict` here (`brew install dict`); CI's `registry-audit`
  installs the Ubuntu package `dict` and runs `dict::real_client_test` in its evidence loop.
