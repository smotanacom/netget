# Beanstalkd tests

Run everything:

```bash
./cargo-isolated.sh test --no-default-features --features beanstalkd --test server -- beanstalkd:: --test-threads=100
```

## Strategy

The evidence is `real_client_test.rs`, which points the Python client library `greenstalk`
(`pip install greenstalk`) at the server. Everything else is NetGet reading bytes NetGet wrote,
and exists for what greenstalk never sends or cannot see: malformed and unknown commands, a body
without its CRLF, pipelining, bounds, failure paths, the model-call count.

Most suites start the server **in process** through `ServerForm` (the dashboard's path) with a
static or Python script handler and a dead model endpoint, so they are deterministic and cost no
model call. `e2e_test.rs` and one case in `real_client_test.rs` use the spawned binary with the
mock model.

## Files

| File | What it proves | Model calls |
|---|---|---|
| `common.rs` | helpers: in-process server, raw peer with `reply()` (reads byte-counted payloads), `QUEUE_SCRIPT` (a deterministic Python queue: job 42 whose body is non-ASCII and contains `\r\nINSERTED 9`; job 404 missing; tube `empty` leaves a reserve waiting; tube `nosuch` missing) | — |
| `real_client_test.rs` | greenstalk parses: `use`/`watch`/`ignore` via its constructor, `using()`, `watching()`, `put` → id, `BURIED <id>` → `BuriedError.id`, `reserve` → the exact multi-byte body with its embedded CRLF, `touch`/`release`/`bury`/`kick_job`/`kick`/`delete`, `NotFoundError`, `peek`, `reserve_job`, `stats`/`stats_tube`/`stats_job` through its own YAML reader (ints as ints), `tubes()`, `pause_tube`, `NotIgnoredError`, `TimedOutError` from a model-left-waiting `reserve(timeout=1)`, `JobTooBigError` for 65536 bytes and a `put` after it still in step; plus one case where a **mocked model** answers put and reserve. Fails, never skips, without python3/greenstalk. | 3 (mocked case) |
| `e2e_test.rs` | one raw-socket session against the mocked model: exact bytes for `put` (the model sees priority/ttr/body/body_bytes), `RESERVED 12 8` for `résumé`, `DELETED`, `stats` and `list-tubes` YAML; NetGet's own `USING`/`WATCHING`/`NOT_IGNORED`/`UNKNOWN_COMMAND` (upper case, empty line)/`BAD_FORMAT` (six malformed lines, a 201-byte tube)/`EXPECTED_CRLF`/`JOB_TOO_BIG` with the body skipped; a pipelined `list-tube-used`/`watch`/`quit`; and that none of those cost a model call (`expect_calls`) | 6 |
| `wire_test.rs` | proptests: any body round-trips through its byte count and leaves nothing behind; any valid stats report parses back key for key; any valid tube name parses and lists; the parser never panics. Tables: command parsing (u32 overflow, `+1`, upper case, non-ASCII tube), `reply_fits` for 25 command/reply pairs, and the renderers refusing ids of 0, oversize bodies, NetGet-only words, newlines and non-ASCII in stats | 0 |
| `connection_bounds_test.rs` | 224-byte line answered, 225 (with 32 KiB pipelined behind it) refused `BAD_FORMAT` + clean close + `decision=fail_closed_line_too_long` and no handler; 64 KiB without newline refused; a 65535-byte job accepted and 65536 `JOB_TOO_BIG` with the session in step; `put` declaring 4 GB or past `u64` with no body answered at once and closed; first-command deadline; idle deadline (a different number); a waiting reserve outlives a 3 s idle bound, `reserve-with-timeout 2` gets `TIMED_OUT` at ~2 s and `0` at once; a `manual`-parked command outlives both deadlines; the 257th connection reads `OUT_OF_MEMORY` and the slot returns | 0 |
| `llm_failure_test.rs` | dead backend → `INTERNAL_ERROR` (or `OUT_OF_MEMORY`) + `decision=fail_closed_llm_error`, no leaked error text, session continues; empty handler → `INTERNAL_ERROR` + `model_silent`; `RESERVED` to `delete`, bare `KICKED` to `kick`, and a wait on `delete` → `INTERNAL_ERROR` + `fail_closed_mismatched_reply`; a model `NOT_FOUND` is sent, `model_reject`, session continues | 0 (the failing one never reaches a model) |
| `peer_inject_test.rs` | a job injected with `send_to_peer` answers a waiting `reserve-with-timeout 4` and no `TIMED_OUT` follows past the 4 s; the session reads commands again; `close_connection` reaches the peer as EOF; an injected stats report is framed like the model's | 0 |

## How each guard was shown to matter

Removed together in one mutated build, then restored (the byte count and `linger` separately):

| Guard removed | Test that failed, and how |
|---|---|
| both `MAX_LINE_BYTES` checks in `Framer::next_line` | `a_224…` (225-byte line answered `UNKNOWN_COMMAND`), `a_line_with_no_newline…` (no reply in 10 s) |
| the declared-size check (`n <= MAX_JOB_BYTES` → `u32::MAX`) | `a_job_of_max_job_size…` (65536 answered `INSERTED 1`), `a_put_declaring_gigabytes…` (no reply in 10 s), e2e (70000-byte put answered), greenstalk (`too_big` was `null`) |
| `tokio::time::timeout` around line reads (→ 3600 s) | `a_peer_that_says_nothing…` (still open after 46 s), `an_answered_peer…` (never closed) |
| the idle exemption for a waiting reserve (idle timer on plain reserve) | `a_worker_waiting_in_reserve…` (answered `TIMED_OUT` while waiting) |
| the connection cap (limiter ×1000) | `the_connection_past_the_cap…` (over-cap peer neither answered nor closed) |
| the reply-fits check | `a_reply_that_does_not_fit…` (`RESERVED 5 23` reached the client), `kicked_without_a_count…` (`KICKED` reached it) |
| the `WriteWatch` checks in the reserve wait | `a_job_injected…` (`TIMED_OUT` followed the injected `RESERVED`) |
| byte count (`body.len()` → `chars().count()`) | the body proptest (payload not followed by CRLF), e2e, and **greenstalk** |
| `linger` after the final reply | `a_224…`: `ConnectionReset` instead of `BAD_FORMAT`, 3 of 4 runs |

## Notes

- No pcap oracle: tshark has no beanstalkd dissector.
- greenstalk 2.1.1 is imported by the `python3` on `PATH` (here Homebrew's 3.10); CI's
  `registry-audit` installs it with `pip install --user greenstalk==2.1.1` and runs
  `beanstalkd::real_client_test` in its evidence loop.
- The installed `beanstalkd` binary is a *server* and is not evidence for this server.
