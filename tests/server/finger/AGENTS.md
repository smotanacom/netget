# Finger E2E Testing

## Test Strategy

`tests/server/finger/e2e_test.rs`. Four tests, **8 LLM calls total**, all driving
a raw `TcpStream`.

**No test drives the real client, and that is the reason the protocol is rated
`Experimental` rather than `Beta`.** It is worth stating precisely, because the
shape of the gap is not "nobody wrote the test":

`finger(1)` is installed on this machine (`/usr/bin/finger`). Its usage is

```text
finger [-46gklmpsho] [user ...] [user@host ...]
```

There is **no port option**. `man finger` documents none, `finger --help` prints
that usage and nothing else, and `user@host:port` is rejected before any socket
is opened:

```
$ finger 'testuser@127.0.0.1:7979'
finger: 127.0.0.1:7979: nodename nor servname provided, or not known
$ finger 'testuser@127.0.0.1'
finger: connect: Connection refused        # went to 79, not to the listener on 7979
```

It resolves the `finger` service (`/etc/services`: `finger 79/tcp`) and always
connects to TCP 79. The `FINGER` environment variable carries options, of which
none is a port. So a real-client test needs a run privileged enough to bind port
79, which nothing in this suite does.

**There is deliberately no `#[ignore]`d root test standing in for it.** The root
`CLAUDE.md` is explicit that an `#[ignore]`d test proves nothing "however good
the reason", and that a skip-when-missing gate is a silent pass — two protocols
are stuck at Experimental for exactly that, and adding a third would be
pretending. What would actually move this to Beta is one privileged run of
`finger alice@127.0.0.1` against the server on port 79, asserting the block it
prints.

What a socket **can** prove is asserted rather than assumed:

- **The exact bytes.** `test_finger_user_query` uses `assert_eq!` on the whole
  response, not `contains`: the column-40 padding before `Name:`, office and
  phone sharing a line, the presence line assembled from `login_time` + `tty` +
  `idle`, `Project:`/`Plan:` under their own headings, CRLF throughout. A
  `contains` assertion would pass on a response with a bare LF in it, and a bare
  LF in a line protocol is exactly the kind of thing a real client would reject.
  `src/server/finger/CLAUDE.md` quotes this block, so the doc cannot drift.
- **That the server closes.** The helper reads to **EOF**, not one `read()`.
  Every finger client reads until the connection closes, so this is the property
  a real client depends on most, and it is asserted on all four tests. If the
  server ever grew WHOIS's keep-reading loop, all four would hang and the
  timeout message names the cause.
- **That forwarding never reaches the model.**

## The forwarding test is the load-bearing one

`test_finger_forwarding_is_refused_by_default` puts `expect_calls(0)` on the
`finger_query` rule. That, not the wire assertion, is what catches a regression:
if forwarding ever started consulting the model, the rule would fire, the count
would be 1, and `verify_mocks()` would fail. Asserting only on the response text
would not catch it, because a model *could* be prompted to produce the same
refusal.

`test_finger_forwarding_answered_locally_when_opted_in` is its mirror: with
`startup_params: {"answer_forward_queries": true}` the event fires exactly once
and its generator **echoes `forward_host` and `username` back into the
response**, so the assertion proves the fields reached the model rather than
merely that something was answered. Both tests additionally wait for the
`decision=forward_refused` / `decision=forward_answered_locally` log line, so the
outcome is distinguishable in the log as well as on the wire.

## Mock expectations

One rule per event, never two. `test_finger_verbose_and_list_all` sends two
different queries and uses a **single** `respond_with_actions_from_event` rule
that branches on `list_all` — two rules on `finger_query` with no way to tell
them apart is first-match-wins, the second would report zero calls, and that is
the most common mock mistake in this repo. The non-`list_all` branch echoes
`verbose` and `username` back into the response, so the assertions prove `/W bob`
was *parsed* rather than merely answered.

## LLM Call Budget

One startup call plus one per query, except forwarding-by-default which costs
none:

| Test | Calls |
|---|---|
| `test_finger_user_query` | 1 + 1 = 2 |
| `test_finger_verbose_and_list_all` | 1 + 2 = 3 |
| `test_finger_forwarding_is_refused_by_default` | 1 + **0** = 1 |
| `test_finger_forwarding_answered_locally_when_opted_in` | 1 + 1 = 2 |

**Total: 8.**

## Runtime

~1 second wall clock at `--test-threads=100` (measured: 0.93s and 1.03s over two
runs). Each test starts its own netget subprocess against an in-process mock
Ollama; there is no real model anywhere.

## Privacy / privilege

- Every test binds `127.0.0.1` on an ephemeral port (`"port": 0` in the
  `open_server` action). Nothing leaves the host.
- No test needs privileges. `finger` declares `PrivilegedPort(79)` and the
  preflight fires only when the requested port is actually below 1024.
- Nothing reads the real user database. The server cannot: there is no code path
  to `passwd`, `utmp` or `~/.plan`, which is the whole point of the protocol as
  NetGet implements it.

## Test Execution

```bash
./cargo-isolated.sh test --no-default-features --features finger \
    --test server -- --test-threads=100 finger
```

Note: at `--no-default-features --features finger` the whole-tree ratchet
`event_action_declarations_test::the_client_audit_reports_its_own_coverage`
fails, because a server-only feature set registers no *client* at all and that
test refuses to inspect nothing. It is not a finger failure — add any client
feature (`--features finger,tcp`) and it passes.

## Not covered

- The real client (above).
- `MAX_QUERY_BYTES` overflow (`finger: query too long`) and the malformed/EOF
  read paths. These are deterministic branches with no LLM involvement; they are
  reachable from a socket but each would cost another subprocess start for
  little more than the branch itself.
- Peer injection through the dashboard handle. `tests/server/whois/peer_inject_test.rs`
  covers the shared `peer_support` machinery, and finger registers the same
  handle the same way.
- Multiple queries on one connection — the server closes after one by design,
  which is what `test_finger_user_query`'s read-to-EOF already asserts.
