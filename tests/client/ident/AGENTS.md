# Ident Client Test Strategy

```bash
./cargo-isolated.sh test --no-default-features --features ident \
    --test client::ident::e2e_test -- --test-threads=100
```

8 tests, all passing, **3 LLM calls total**.

## What the peer is, and what each peer is worth

Read this before treating any of it as maturity evidence.

| Peer | Tests | What it proves |
|---|---|---|
| **NetGet's own ident server** (`ServerForm`, in-process) | USERID reply, all four ERROR tokens, the follow-up chain | The two halves agree. **Same-project evidence** — the circular-evidence class the root `CLAUDE.md` names — not that either matches RFC 1413 |
| **Hand-written loopback peer** (`spawn_raw_ident_peer`) | port-pair mismatch, whitespace on the wire | An independent reading of the wire format, not an independent implementation. Same class as `dhcp`'s in-test RFC 2131 decoder |
| **`parse_ident_reply` directly** | whitespace, seven malformed shapes, oversized, `resolve_target` | Exact and cheap; where a parse regression surfaces first |

There is no third-party ident server to point this client at, and the reason is structural, not
a matter of effort: RFC 1413 has no notion of a configurable port, so nothing can be aimed at an
ephemeral loopback port and 113 needs root. `src/server/ident/CLAUDE.md` records the full search
(crates.io, Homebrew, PyPI, macOS binaries). **Do not repeat it, and do not read this suite as
Beta evidence.**

## Why the mismatch test cannot use NetGet's server

`a_reply_about_a_different_port_pair_is_rejected` is the test that matters most — it is the one
real correctness property an ident client has (RFC 1413 §3: the client matches a reply to its
query by the port pair; a reply about a different pair is an answer about somebody else's
connection).

NetGet's own ident server is **structurally incapable of producing that frame**:
`enforce_port_pair` in `src/server/ident/mod.rs` deliberately rewrites a wrong pair back to the
queried one and logs a WARN. So the peer here is hand-written, answers a query about
`6193 , 23` with a well-formed USERID line about `9999 , 8888`, and the test asserts both
halves: `ident_reply_mismatch` fires with both pairs recorded, **and**
`ident_response_received` never fires. The negative assertion is the load-bearing one — without
it, a client that raised both events would pass.

The whitespace-on-the-wire test uses the same peer for the same reason: NetGet's server emits
one canonical spacing, so it cannot exercise tolerance of another.

## The LLM budget, and how the zero-call tests stay honest

Five of the eight tests are pure or use only static handlers and make **no** LLM call. They
point `AppState` at `http://127.0.0.1:1`, which is unreachable on purpose: a routing hole does
not quietly fall through to the model, it fails.

That is not merely a safety net — it is what makes those tests assert anything. A client-side
access-log entry is recorded on the *handled* path or after a *successful* LLM call, so if any
event had escaped its static rule the call would error, no entry would be written, and
`await_client_event` would panic naming the events it did see. Passing therefore proves the
deterministic path ran.

Server-side, `instruction: Some(String::new())` is what keeps the model out: `ServerForm::create`
substitutes a default instruction for `None`, and any non-empty instruction makes the server
consult the model whatever the comments say.

`the_model_can_chain_a_second_query` is the only test with a model, through the in-process
`MockOllamaServer`: **3 calls** (one `ident_connected`, two `ident_response_received`).

## Mocking notes worth keeping

**One rule per event id.** `ident_response_received` fires twice with different data, and the
single rule branches on `event["server_port"]` rather than being two rules — two rules on one
event is the mocking mistake this repo makes most often (the first answers every occurrence,
the second reports zero calls).

**A static server handler does not need to echo the pair.** It cannot read the event, but the
server's own `enforce_port_pair` readdresses each reply to whichever pair was queried. So one
fixed `send_ident_userid` action answers both the `6193,23` query and the `7000,24` follow-up
correctly — which incidentally exercises the server's pair enforcement against a real client.

## What `the_model_can_chain_a_second_query` guards

The most common client defect in this repo: asking the model what to do and discarding the
answer. The follow-up query is a **new TCP connection** (RFC 1413 is one exchange per
connection) that **raises its own event**, so the assertion is that a second
`ident_response_received` arrives carrying `server_port: 7000` — the pair the model chose on
its second turn. A client that dropped `result.actions`, or that ran the follow-up without
raising an event (the shape `whois` has), leaves this red.

It also asserts `ident_reply_mismatch` never fires, i.e. the follow-up's reply is matched
against the follow-up's *own* query rather than the original one.

## Startup parameters under test

Every test connects with `remote_addr: "127.0.0.1"` — host only, no port — and supplies
`ident_port` in `startup_params`. That is deliberate: it is how a real caller would reach a
non-standard port, and it exercises `resolve_target`'s default-113 path being overridden rather
than a port smuggled through `remote_addr`. `the_default_port_is_113_and_ident_port_overrides_remote_addr`
pins the precedence directly, including the IPv6 bracketing.

## Not covered

No real `identd`, for the reason above. No IPv6 on the wire (only `resolve_target`'s
bracketing). No `response_timeout_secs` expiry — it would cost a real wall-clock wait, and the
path it guards is a plain `tokio::time::timeout`. No injected-command test
(`AppState::send_to_client` with `send_ident_query`); the command channel is registered and
shares `apply_action` with the LLM path, but nothing here proves the dashboard button end to
end. `tests/client/whois/command_channel_test.rs` is the pattern to copy if that gap is worth
closing.
