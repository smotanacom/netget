# Finger client tests

`e2e_test.rs`, four tests, **10 mocked LLM calls total**, ~1.8s at `--test-threads=100`.

```bash
./cargo-isolated.sh test --no-default-features --features finger --test client -- finger --test-threads=100
```

Note the target: `--test client` with `finger` as a **filter**. There is no
`--test client::finger::e2e_test` target — `tests/client.rs` is the single binary and the module
path is a name filter, not a target name.

## Two peers, because they prove different things

| Peer | Used by | What it proves | What it does **not** prove |
|---|---|---|---|
| **NetGet's own Finger server** | round-trip test | the two halves agree end to end: a real server answer reaches the model as text, with `eof`, `query` and a labelled `best_effort` block | anything about RFC 1288. This is **same-project evidence** — the circular-evidence class the root `CLAUDE.md` names |
| **A raw `TcpListener`** (`spawn_fake_fingerd`) | the other three | the literal bytes this client emits, and — the load-bearing case — that it emitted **none** | that any real daemon accepts them |

`spawn_fake_fingerd` is a fifteen-line probe, not an implementation, and is not offered as
independent evidence. It records every query line it receives and closes after answering, which
is the property the client depends on: a finger client reads to EOF, so a peer that never closed
would hang every test here and say so.

## The four tests

1. **`test_finger_client_round_trip_against_netget_server`** — 2 server calls (startup +
   `finger_query`), 2 client calls (startup + `finger_response_received`). The assertion is not
   "it connected": the mock's response generator runs **in this test process**, so the captured
   `finger_response_received` is the model's actual view. It asserts the raw block survived
   (`Login: alice`, `Name: Alice Smith`, the multi-line `Plan:`), that CRLF was normalised, that
   `query`/`username`/`verbose`/`forward_host` name the query that produced it, `eof == true`
   (the server closed — what a finger client reads to), and that `best_effort.note` says
   `GUESS`.
2. **`test_finger_client_query_forms_and_followups_on_the_wire`** — 4 client calls. All three
   RFC 1288 query forms compared **literally** against the raw peer: `alice`, `/W bob`, and the
   empty line (`{C}`-only, meaning "everyone"). Because the peer sees three separate
   connections carrying one query each, this simultaneously proves the follow-up chain opens a
   **new** connection per query and that each answer raises `finger_response_received` again.
3. **`test_finger_client_refuses_forwarding_by_default`** — 1 call (startup). A static handler
   answers `finger_connected` with a `forward_host` query. Asserts `decision=forward_refused` in
   the log **and** that the raw peer received nothing at all. Asserting only the log would not
   catch a client that logged the refusal and sent the query anyway.
4. **`test_finger_client_forwards_only_when_explicitly_enabled`** — 1 call (startup). The mirror
   of (3): same handler, same peer, `startup_params: {"allow_forwarding": true}`, and
   `alice@relay.example.invalid` must arrive exactly. **This test is what makes (3) mean
   something** — without it, "nothing on the wire" is equally consistent with a client that
   cannot build a `{Q2}` query at all.

## Things that cost time here, or would have

- **The connected event is a `static` handler in every test**, so it costs zero LLM calls. That
  is what keeps the budget at 10. An empty-or-static handler genuinely suppresses the model call
  (`tests/empty_static_handler_test.rs` measures it directly).
- **One branching rule, never three rules on the same event.** Test 2 answers three
  `finger_response_received` events with a single `respond_with_actions_from_event` that counts
  its own calls. Three indistinguishable rules would be first-match-wins: the first would answer
  all three and the other two would report zero calls. A stateful generator is safe because the
  mock renders each response exactly once per request.
- **The peer's read is bounded at 2s** and test 3 additionally passes
  `response_timeout_secs: 2`. Without either, the refusal test — where no query is ever sent —
  would park until the suite timeout rather than finishing in two seconds.
- **`NetGetClient` has no `wait_for_log`** (only `NetGetServer` does). Use
  `wait_for_any(&[...], n)` then `output_contains`.
- Test 4 polls the shared query list against a deadline instead of sleeping, per the "never wait
  on a fixed sleep" rule; every test ends with `wait_for_mocks(30)` before `verify_mocks()`.

## What is deliberately not tested, and why

There is **no test against a real finger daemon**, and no `#[ignore]`d one either. An ignored
test proves nothing and a skip-when-missing gate is a silent pass. No daemon exists here to run:
macOS ships `finger(1)` but no `fingerd`, Homebrew has no `bsd-finger`/`fingerd`/`netkit`
formula, and there is no `socat`/`xinetd`/`inetd` to serve an inetd-style one on a high port.
See `src/client/finger/CLAUDE.md` for the full check and for why this is nonetheless the
*achievable* path to Beta — a daemon can be told which port to bind, even though `finger(1)`
cannot be told which port to dial.

There is also no test that parses the response into fields, because the client deliberately does
not do that. If you add one, you are testing a heuristic that is wrong against half the world's
finger daemons.
