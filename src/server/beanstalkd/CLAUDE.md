# Beanstalkd Protocol Implementation

Beanstalkd work-queue server (upstream protocol.txt from the beanstalkd repository, 1.13). The model is the
queue: it decides which job ids a `put` gets, which job a `reserve` hands out (or that the
worker waits), what `delete`/`release`/`bury`/`touch`/`kick`/`peek` find, and what the stats
say. NetGet stores no jobs and writes every byte of framing.

**State**: Experimental (see Maturity). **Privilege**: `None` — the well-known port is 11300.
**Stack**: `ETH>IP>TCP>Beanstalkd`. **Feature**: `beanstalkd` (no dependencies).

## Library choice

None. Beanstalkd is a CRLF line protocol with one length-prefixed body (`put`, `RESERVED`,
`FOUND`, `OK`), and no maintained Rust crate implements the server side. `wire.rs` is pure
functions; `mod.rs` is the session loop.

## Files

| File | What it holds |
|---|---|
| `mod.rs` | accept loop, session loop, `Framer` (lines, bodies, discards), the reserve wait, `WriteWatch`/`WatchedWriter`, the commands NetGet answers itself |
| `wire.rs` | pure parsing and rendering: `parse_command`, `valid_tube_name`, `render_status`/`render_inserted`/`render_job`/`render_stats`/`render_tube_list`, `reply_fits`, `is_refusal` |
| `actions.rs` | the `Protocol`/`Server` impls, the eight actions, the four events |

## Spec subset

| Command | Answered by | Reply |
|---|---|---|
| `put <pri> <delay> <ttr> <bytes>` + body | model → `beanstalkd_put {tube, priority, delay, ttr, body, body_bytes}` | `INSERTED <id>`, `BURIED <id>`, `DRAINING` |
| `reserve`, `reserve-with-timeout <s>` | model → `beanstalkd_reserve {tubes, timeout_secs?}` | `RESERVED <id> <bytes>` + body, `DEADLINE_SOON`, or wait (see below); `TIMED_OUT` from NetGet |
| `reserve-job <id>`, `delete`, `release`, `bury`, `touch`, `kick-job`, `peek`, `peek-ready`/`-delayed`/`-buried`, `kick <bound>`, `pause-tube` | model → `beanstalkd_job_command {command, job_id?, tube, priority?, delay?, bound?}` | the command's own word, or `NOT_FOUND` |
| `stats`, `stats-tube`, `stats-job`, `list-tubes` | model → `beanstalkd_stats {scope: server\|tube\|job\|tubes, tube?, job_id?}` | `OK <bytes>` + YAML, or `NOT_FOUND` |
| `use`, `watch`, `ignore`, `list-tube-used`, `list-tubes-watched` | NetGet, from the connection's own tube state | `USING`, `WATCHING <n>`, `NOT_IGNORED`, `OK <bytes>` + YAML list |
| `quit` | NetGet | close |
| wrong arity, non-digit number, bad tube name | NetGet | `BAD_FORMAT` |
| anything else (commands are case-sensitive) | NetGet | `UNKNOWN_COMMAND` |

Not implemented: the drain/binlog admin surface (there is no such command in the protocol; a
real server takes it from signals and flags). Tube names follow upstream: 1–200 bytes of
`[A-Za-z0-9+/;.$_()-]`, not starting with `-`.

The one piece of state NetGet keeps is the connection's **used tube** and **watch list** —
state about the connection, not the queue — so `use`/`watch`/`ignore` cost no model call and the
events can say which tube a `put` goes to and which tubes a `reserve` draws from.

## What the model sees and controls

| Action | Renders |
|---|---|
| `insert_beanstalkd_job {job_id, buried?}` | `INSERTED <id>` / `BURIED <id>` |
| `reserve_beanstalkd_job {job_id, body}` | `RESERVED <id> <bytes>\r\n<body>\r\n` |
| `send_beanstalkd_found {job_id, body}` | `FOUND <id> <bytes>\r\n<body>\r\n` |
| `wait_for_beanstalkd_job` | nothing now: the worker waits (reserve only) |
| `send_beanstalkd_status {status, count?}` | one word from `wire::STATUS_WORDS`; `count` only with `KICKED` |
| `send_beanstalkd_stats {stats: {name: value}}` | `OK <bytes>\r\n---\nname: value\n…\r\n` |
| `send_beanstalkd_tubes {tubes: [..]}` | `OK <bytes>\r\n---\n- name\n…\r\n` |
| `close_connection` | close after any reply |

Job bodies reach the model as UTF-8 text (invalid bytes replaced) with `body_bytes` beside it,
and the model gives bodies back as text; no bytes or base64 in either direction.

### NetGet does the framing; the model cannot

* **Byte counts.** `<bytes>` is the body's UTF-8 length, computed by `wire::render_job`, so a
  body containing `\r\nINSERTED 9` is read by the client as body. Counting characters instead
  (the tempting mistake with `✓` or `é` in a body) makes greenstalk's CRLF check fail — measured.
* **YAML.** Stats names must be `[A-Za-z0-9_-]{1,64}`; values numbers, booleans or 1–200
  printable ASCII characters. greenstalk decodes reports as ASCII, and a newline would forge a
  stat line, so both are refused (the model is told why) rather than rewritten. Tube lists
  hold only valid tube names.
* **The reply must fit the command.** `wire::reply_fits` checks the reply's word *and* arity
  against the command: `RESERVED` does not answer `delete`; `BURIED <id>` answers `put` but bare
  `BURIED` answers `bury`/`release`; `KICKED <n>` answers `kick` and bare `KICKED` answers
  `kick-job`; a stats dictionary does not answer `list-tubes`. `OUT_OF_MEMORY`/`INTERNAL_ERROR`
  answer anything. A misfit is answered `INTERNAL_ERROR` and logged
  `decision=fail_closed_mismatched_reply`. Only the first reply is sent if there are several.

### A worker waiting in reserve

`wait_for_beanstalkd_job` is valid only for `reserve`/`reserve-with-timeout` (anything else is a
mismatch). While a reserve waits:

* the **idle deadline does not apply** — the server owes the worker an answer. The connection
  cap is what bounds how many can wait;
* `reserve-with-timeout <s>` is answered `TIMED_OUT` by NetGet at `<s>` seconds
  (`decision=model_wait_timed_out`); `reserve-with-timeout 0` is answered at once;
* a reply written to the connection from elsewhere — the dashboard's `[ message ]`, which goes
  through `peer_support`'s own task and never through this loop — ends the wait. `WatchedWriter`
  counts writes; the wait checks the count, and the `TIMED_OUT` write re-checks it under the
  writer lock, so an injected `RESERVED` is never followed by a stray `TIMED_OUT`;
* bytes from the worker end the wait (the reserve is abandoned, logged) and are read as the
  next command; EOF closes.

A plain `reserve` the model leaves waiting therefore waits until the operator sends it a job
or the worker hangs up. There is no re-poll of the model.

## Failure behaviour

`FailureMode::Answers` (`.answers_on_failure()`). Every failure is a complete beanstalkd answer
to one command, so **the session continues** afterwards:

| Cause | Wire | Log token |
|---|---|---|
| backend failed, unavailable | `INTERNAL_ERROR` | `decision=fail_closed_llm_error category=unavailable` |
| backend failed, at capacity | `OUT_OF_MEMORY` (upstream's "try again later") | `decision=fail_closed_llm_error category=overloaded` |
| handler/model answered nothing | `INTERNAL_ERROR` | `decision=model_silent` |
| reply does not fit the command, or wait on a non-reserve | `INTERNAL_ERROR` | `decision=fail_closed_mismatched_reply` |

A model `NOT_FOUND`/`TIMED_OUT`/`DEADLINE_SOON`/`DRAINING`/error is sent as-is,
`decision=model_reject`; a success is `decision=model_answer`; a wait is `decision=model_wait`.
All failure texts are byte literals, so nothing from the error reaches the peer. NetGet never
invents `DELETED`, `NOT_FOUND` or a job on the model's behalf.

## Bounds

| Bound | Value | Override | Why |
|---|---|---|---|
| `MAX_LINE_BYTES` | 224 incl. CRLF | — | upstream's `LINE_BUF_SIZE`. Over it (or 224 bytes with no newline): `BAD_FORMAT`, close, `decision=fail_closed_line_too_long`, before any handler. |
| `MAX_JOB_BYTES` (= `max_inbound_bytes`) | 65535 | — | upstream's default `max-job-size`. Judged from the **declared** `<bytes>` before any body byte is read: `JOB_TOO_BIG`, `decision=fail_closed_job_too_big`. Up to `MAX_DISCARD_BYTES` (1 MiB) of the body is then read and dropped to stay in step, as upstream does; a larger declaration (or one past `u64`) is answered and closed. Also caps `RESERVED`/`FOUND` bodies the model writes. |
| `FIRST_COMMAND_TIMEOUT` | 300 s | `first_byte_timeout_secs` | Client-speaks-first; 300 s is the `manual` window for a NetGet TCP client parked on its operator. Lower it for a public listener. |
| `IDLE_TIMEOUT` | 300 s | `idle_timeout_secs` | Between commands, and per read of a `put` body. Upstream has none. **Not** applied to a waiting reserve. |
| `MAX_CONNECTIONS` | 256 (house default) | — | The peer past the cap reads `OUT_OF_MEMORY` (upstream's retry-later answer) and EOF. |

**Every close lingers** (2 s / 64 KiB of discarded input after the half-close), so a
`BAD_FORMAT` written just before closing is not destroyed by an RST over unread pipelined input.
Removing `linger` makes the 225-byte test (with 32 KiB pipelined behind the line) fail with
`ConnectionReset` in 3 of 4 runs.

The deadlines wrap the read only, so a command parked for a human under a `manual` rule is
closed by neither.

## Peer handle

Registered before the first command. Injected actions are rendered by the same executor as the
model's; an injected `reserve_beanstalkd_job` answers a waiting reserve (see above).

## Wireshark

No beanstalkd dissector in this Wireshark build (`tshark -G protocols` lists none), so
`src/tui/wireshark.rs` maps `beanstalkd` to plain TCP and there is no pcap-oracle test.

## Maturity

Experimental. The evidence for Beta is in place — `tests/server/beanstalkd/real_client_test.rs`
drives greenstalk 2.1.1 (a Python client with its own reply parser and YAML reader; the server
uses no beanstalk library) through every command it has, asserting on what greenstalk returned
or raised; it is not `#[ignore]`d and fails, never skips, without python3/greenstalk; CI's
`registry-audit` installs greenstalk from PyPI and runs it. Promotion is a separate step.
