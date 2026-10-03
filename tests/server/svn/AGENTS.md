# SVN E2E Test Documentation

## What actually proves anything here

**`real_client_test.rs` is the evidence. Nothing else in this directory is.**

The other three files write `"<command>\n"` themselves and read the reply with
`read_line`. That is a line-oriented protocol which only NetGet speaks: ra_svn
frames on **tuple structure**, and a real client's first message ends in a space
with no newline anywhere in it. Those tests were green for months against a
server no `svn` client could talk to at all — `svn info` failed, not just
`svn checkout`. If you take one thing from this document, take that: **a
protocol's own tests agreeing with it is not evidence.**

| File | What it covers | LLM calls | Needs |
|---|---|---|---|
| `real_client_test.rs` | the real `svn` 1.14.5 binary completes `svn info`, `svn ls` and `svn log` | 8 + 7 + 8, mocked | the `svn` binary — **hard-fails without it** |
| `framing_test.rs` | ra_svn framing and its bounds, from a raw socket | 0 (static handlers) | nothing |
| `e2e_test.rs` | five mocked command cases | 13 mocked | nothing |
| `llm_failure_test.rs` | a dead backend produces a well-formed `failure` tuple carrying only a `WireFailure` category, then EOF | 0 (backend unreachable by design) | nothing |
| `peer_inject_test.rs` | `send_to_peer` writes a success tuple to a raw socket, the counters move, `close_connection` sends EOF | 0 | nothing |

**Model**: the in-process mock (`tests/helpers/mock_ollama.rs`). No Ollama, no
`--ignored`; the whole directory compiles and runs under
`--no-default-features --features svn`.

## 1. `real_client_test.rs` — the real Subversion client

Three tests, one server each so a failure names the subcommand that caused it.
Every one takes `svn` through a complete ra_svn session:

```
greeting -> capability tuple -> auth-request -> ANONYMOUS token
         -> auth success + repos-info -> the subcommand's commands
```

| Test | Commands the client issues | Asserted |
|---|---|---|
| `test_svn_info_against_real_svn_client` | get-latest-rev, stat, get-latest-rev, get-lock | `Revision: 42`, the repository UUID, `Node Kind: directory`, `Last Changed Author: netget`, `Last Changed Rev: 42` |
| `test_svn_ls_against_real_svn_client` | get-latest-rev, stat, get-dir | `trunk/`, `branches/`, `README.txt` |
| `test_svn_log_against_real_svn_client` | get-latest-rev ×3, log | `r42`, `netget`, `first commit` |

Four things about how these are written are deliberate:

- **It fails, it does not skip.** A `SKIP: svn is not installed` that returns
  `Ok(())` is a silent pass wherever the binary is missing, which is most CI
  runners — and it would leave SVN's Beta rating resting on nothing. The error
  names the install command.
- **`tokio::process::Command`, never `std::process`.** `#[tokio::test]` is a
  current-thread runtime, so a blocking `output()` parks the only worker that
  could drain the child's pipes, and the test deadlocks rather than failing.
- **Each assertion names a field from a *different* tuple.** The revision came
  from `get-latest-rev`, the UUID from the repos-info that follows the auth
  success, the kind and author from the `stat` dirent, the trailing `/` on
  `trunk/` from the `kind` word in the get-dir entry. Asserting only "exit 0"
  would pass against a client that gave up politely.
- **One mock rule for `svn_command`, branching on the command.** Rules are
  first-match-wins, so two rules on the same event with nothing to tell them
  apart leave the second at zero calls — the most common mistake in this repo.
  The command counts are `expect_at_least`, because the sequence is the client's
  business and a later version may reorder it; what is pinned exactly is the
  handshake, which is ours.

`repository_root` is derived from the `url` the `svn_auth_response` event
carries. It has to be: `svn` requires the root to be a prefix of the URL it
opened, and that URL holds an ephemeral port no static handler could know. This
is also how a model would have to do it.

## 2. `framing_test.rs` — the framing, without needing `svn`

Zero LLM calls; every event is answered by a static handler and the peer is a raw
socket. It holds on a runner with no Subversion installed, which is what makes it
the regression test rather than the evidence.

| Test | What it pins |
|---|---|
| `a_tuple_with_no_trailing_newline_and_a_newline_inside_a_string_is_framed` | the two shapes a line reader cannot survive: a capability tuple ending in a space, and a counted string containing `\n`. Also that a completed session's command reply carries the trivial auth-request prefix, byte for byte |
| `a_peer_that_skips_the_handshake_gets_an_unprefixed_reply` | the prefix is *not* written to a peer that never sent a capability tuple, which is what keeps `nc` and the four mocked files working |
| `a_depth_bomb_is_refused_in_svns_own_vocabulary` | `MAX_TUPLE_DEPTH`; the refusal is apr-err 210004 and the connection then closes |
| `a_string_length_larger_than_the_cap_is_refused_before_it_is_read` | the declared length is bounded, not the delivered one — twelve bytes claiming a gigabyte |

Both bounds were checked by removing them. What happens then is the finding: the
two tests fail on their **own 20-second deadline** waiting for a refusal that
never arrives, not on an assertion — an unbounded parser has no failure to
report. It goes on reading, and with the declared-length check gone it first goes
on to allocate the gigabyte the peer promised in twelve bytes.

## 3. `e2e_test.rs` — the mocked command cases

Five cases: greeting, `get-latest-rev`, `get-dir`, a failure response, and
connection-stat tracking. Each starts its own server, connects, writes one
command **with a trailing newline it supplies itself**, and matches keywords in
the reply.

Read that limitation twice. These tests cannot see a framing defect, because the
newline they add is the one the real client never sends; and their assertions are
`contains("success") || contains("42")`, which passes on a great many wrong
answers. They are useful for checking that an action still encodes what it used
to, and for nothing else.

The expected wire forms, for reference:

- greeting — `( success ( 2 2 ( ANONYMOUS ) ( edit-pipeline svndiff1 ) ) )`
- get-dir — `( success ( 0 ( ) ( ( 5:trunk dir 0 false 1 ( ) ( ) ) … ) ) )`,
  **counted strings, not quoted ones.** svn has no quoting mechanism: a string is
  its byte length, a colon, then the raw bytes. This document showed `"trunk"`,
  which is five characters and two stray quote marks that no svn parser accepts.
- failure — `( failure ( ( 210005 14:Path not found 0: 0 ) ) )`, the tuple being
  `( apr-err message:string file:string line:number )`.

## LLM call budget

| File | Calls |
|---|---|
| `real_client_test.rs` | 23 across three tests (1 startup + 3 handshake + 3–4 commands each) |
| `e2e_test.rs` | 13 across five tests |
| the other three | 0 |

The handshake is three of every connection's calls — greeting, capabilities,
auth — so a test that opens two connections pays for six. Use static handlers
(as `framing_test.rs` does) wherever the handshake is scaffolding rather than the
thing under test.

## Test Infrastructure

```bash
# --test names a target, so `--test server` then filter by module path.
./cargo-isolated.sh test --no-default-features --features svn \
    --test server -- server::svn --test-threads=8
```

**Requirements**: the `svn` binary, for `real_client_test.rs` only —
`brew install subversion` or `apt-get install subversion`. CI's `registry-audit`
job installs it. Everything else needs nothing external: the mock model runs
in-process, every socket binds `127.0.0.1:0`, and no test is `#[ignore]`d.

**Debugging**: `RUST_LOG=netget=trace` prints every tuple in and out
(`SVN message:` / `SVN response:`).

## Not covered, and worth adding

- **`svn checkout` / `update` / `commit`.** Not a test gap — the editor command
  set and svndiff are not implemented, so there is nothing to test yet. The Beta
  rating is written with that limit attached; see `src/server/svn/CLAUDE.md`.
- **A second independent client.** The Stable bar asks for two, and there is one.
  A `pysvn` or `subversion`-bindings driver would be the cheapest second.
- **Authentication that is refused.** `send_svn_failure` on `svn_auth_response`
  is implemented and unexercised against the real client.
- **Concurrency**: no test opens two sessions at once.

## References

- [SVN Protocol Specification](https://svn.apache.org/repos/asf/subversion/trunk/subversion/libsvn_ra_svn/protocol)
- `src/server/svn/CLAUDE.md` — the framing, the handshake and the action table
- `src/server/svn/wire.rs` — the tuple reader and its bounds
