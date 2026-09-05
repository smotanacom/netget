# Ident E2E Testing

`tests/server/ident/e2e_test.rs`. Three tests, **9 LLM calls total**, all mocked.

## The ceiling on what this suite can prove

**Every test drives a raw `TcpStream`, and that is not a shortcut — it is the only option.**
There is no runnable third-party ident *client* anywhere to point at the server:

- crates.io has no RFC 1413 client (`rfc1413` and `identd` return empty result sets; the
  `ident` namespace is entirely identity/identifier crates);
- macOS ships no `ident`/`identd`/`oidentd`/`pidentd` client binary and Homebrew has no
  formula for one — `oidentd` is a server and is not in Homebrew at all;
- Homebrew's `libident` is a C library with no CLI;
- PyPI has no working ident package;
- the realistic real-world client is an IRC daemon (`ngircd` links `libident`).

The blocker is structural: **every ident client hardcodes destination port 113, because RFC
1413 has no notion of a configurable server port.** Nothing can be aimed at an ephemeral
loopback port, and 113 needs root and collides process-wide rather than being per-test
isolated. Standing up `ngircd` would not help for the same reason.

So the client in this file is written from the wire format, which the root `CLAUDE.md` is
explicit about: that is "an independent reading of the spec, not an independent
implementation" — the `dhcp` and `usb/serial` case. **It supports `Experimental`, not `Beta`.**
Do not read the green suite as evidence of the latter, and do not add an `#[ignore]`d test
against a client that does not exist in order to look like it does.

## What the socket *can* prove, and where each property is pinned

| Property | Test |
|---|---|
| USERID reply is well-formed and the queried port pair is echoed **verbatim** | `test_ident_userid_reply_echoes_the_port_pair` |
| the three model-chosen error tokens reach the wire | `test_ident_error_tokens_and_port_pair_correction` |
| a **wrong** pair from the handler is rewritten to the queried one | same test, 4th query |
| whitespace around the comma parses; `opsys` defaults to `UNIX` | `test_ident_whitespace_tolerance_and_invalid_port_without_llm` |
| out-of-range / non-numeric / comma-less queries are refused `INVALID-PORT` | same test |
| that refusal costs **no LLM call** | same test — by call count, see below |
| the server closes after answering (RFC 1413) | every test: the helper reads to EOF |

Two of these deserve explanation because the assertion is not where you would expect it.

### "No LLM call" is asserted by count, not by inspection

There is no way to assert a call did not happen by looking at a reply. So
`test_ident_whitespace_tolerance_and_invalid_port_without_llm` sets `expect_calls(1)` on the
`ident_query` rule and then sends **one** well-formed query and **four** malformed ones. If
any malformed query raised `ident_query`, the count would be five and `verify_mocks()` fails
naming the rule. The `wait_for_log` on `decision=invalid_port (rejected in-process, no LLM
call)` is a second, weaker check that the operator can see the same thing.

### The port-pair echo is only meaningful because the mock derives it from the event

A mock answering with a literal `6193 , 23` would pass even if the event carried the wrong
ports — it would be asserting that the test's own constant survived a round trip. Both
success-path rules use `respond_with_actions_from_event` and copy `e["server_port"]` /
`e["client_port"]`, so the assertion actually reaches the event data.

The one exception is deliberate: the 4th query of
`test_ident_error_tokens_and_port_pair_correction` answers with an invented `9999 , 8888`,
which is the mistake a model makes when it fills the numbers in rather than echoing them. The
test asserts the wire still carries `113 , 4`. Without `enforce_port_pair` in `mod.rs` this is
the failure that matters most: a client matches its query to a reply by that pair, so a
mismatched reply is discarded and looks exactly like the server never answering.

## Rules are first-match-wins

`test_ident_error_tokens_and_port_pair_correction` covers four different answers with **one**
`ident_query` rule branching on `e["client_port"]`. Four indistinguishable rules on the same
event would send every query to the first and report zero calls for the other three — the most
common mistake in this repo.

## LLM call budget

One startup call plus one per well-formed query. Nine total, comfortably under the ~10 ceiling.

| Test | Calls |
|---|---|
| `test_ident_userid_reply_echoes_the_port_pair` | 1 + 1 = 2 |
| `test_ident_error_tokens_and_port_pair_correction` | 1 + 4 = 5 |
| `test_ident_whitespace_tolerance_and_invalid_port_without_llm` | 1 + 1 = 2 (four further queries cost nothing — that is the point of the test) |

## Runtime

~1 second for the three tests once built; the servers are mocked and each exchange is two
lines. Measured at 0.93s with `--test-threads=100`.

## Privacy

All tests bind and connect to `127.0.0.1` on an ephemeral port. No external network access,
and nothing looks up a real local user — the server cannot, by construction (see
`src/server/ident/CLAUDE.md`).

## Test execution

```bash
./cargo-isolated.sh test --no-default-features --features ident \
    --test server ident -- --test-threads=100
```

Note `--features ident` alone is a server-only build. Running the whole-tree ratchets at that
feature set fails two **coverage-floor** assertions that are unrelated to this protocol —
`event_action_declarations_test::the_client_audit_reports_its_own_coverage` ("no client is
registered in this build") and `executable_examples_test::the_example_audit_has_something_to_
inspect` ("only 12 examples were checked"). Add a client feature (`--features ident,tcp`) and
both pass; the substantive checks in both files pass either way.

## Not covered

- Any third-party client, for the reasons above.
- Port 113 itself, and therefore the `PrivilegedPort(113)` preflight — it needs root and would
  collide process-wide.
- `close_connection` as the model's sole answer (it produces no output and is filled in as
  `ERROR : UNKNOWN-ERROR`); the `model_silent` and `fail_closed_llm_error` decision paths are
  reasoned about in `src/server/ident/CLAUDE.md` but not exercised here, because provoking a
  backend failure from the black-box harness means breaking the mock rather than the server.
- The 30-second `QUERY_READ_TIMEOUT` for a peer that connects and says nothing.
- Dashboard peer injection (`[ message this peer ]`); the handle is registered but there is no
  `peer_inject_test.rs` for ident yet.
- The 1024-byte oversize-line cap.
