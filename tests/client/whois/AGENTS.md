# WHOIS Client E2E Tests

## Test Strategy

**Loopback only, against NetGet's own WHOIS server.** Nothing here contacts a public
WHOIS server, and nothing may: the repo rule is "bind to localhost only; never contact
external endpoints".

| File | Covers | LLM calls |
|---|---|---|
| `e2e_test.rs` | the model-driven round trip | 4 (two startups, one server-side query, one client-side response) |
| `command_channel_test.rs` | dashboard injection — `[ query_whois ]` and `[ disconnect ]` through `AppState::send_to_client` | 0 |

## What this file used to say, and why it is worth recording

This document previously described three tests — `test_whois_query_example_com`,
`test_whois_query_verisign`, `test_whois_auto_disconnect` — against `whois.iana.org`
and `whois.verisign-grs.com`, complete with rate-limit advice, an uptime table and
"Debugging tips: check network connectivity (`ping whois.iana.org`)".

All three were `#[ignore]`d. They had no `.with_mock()`, so they also needed
`--use-ollama`. They therefore **ran nowhere and asserted nothing**, while this file
and the test names together read as coverage of the client's model-driven path — which
had none at all. The only thing actually exercising the client was
`command_channel_test.rs`, which deliberately avoids the LLM.

An `#[ignore]`d test is not evidence. Neither is a document describing one.

## `e2e_test.rs` — the round trip

A NetGet WHOIS server and a NetGet WHOIS client, both on mocked models, on loopback:

1. the server is told to answer `example.com` with a record and `close_connection`
   (RFC 3912: the client reads to EOF, so the server has to close);
2. the client's `whois_connected` is routed by a **static** handler to
   `{"type": "query_whois", "query": "example.com"}`;
3. the mock rule on `whois_response_received` captures the event and answers
   `disconnect`.

**The capture is the point.** `respond_with_actions_from_event` runs its closure inside
this test process, so what it stores is the model's actual view of the event, not a
reconstruction. That is the only way to assert the fields the client puts on the event:
`response` (the record verbatim, both name-server lines present, proving the reply was
read all the way to EOF), `query` (the query that actually went on the wire, which the
client tracks whether the model or an injection sent it) and `truncated`.

**Two defects this test found on its first run**, both of which made it fail loudly
before it passed:

- The static handler naming `query_whois` was **rejected at startup** —
  `Unknown action "query_whois" in the static handler for event 'whois_connected'`.
  `events::handler::action_catalog_for_pattern` builds its catalog from
  `get_sync_actions()` plus the matching event's own `.with_actions(…)`, and reads
  `get_async_actions()` not at all. The whois client declared its verbs async-only and
  attached none to its events, so no static or script handler could ever name one.
- `get_event_types()` returned two freshly-built `EventType`s with the right ids but no
  parameters and `{"type": "placeholder"}` as the example action, rather than the
  statics `mod.rs` emits.

Neither is visible from a test that does not route deterministically, which is why
three external tests could sit here for a long time without either being noticed.

## Rules worth keeping

- **Wait for conditions.** `wait_for_mocks(30)` before `verify_mocks()` on both
  processes; the last event routinely lands after any fixed sleep under
  `--test-threads=100`.
- **`verify_mocks()` on both.** The harness panics on drop if either is skipped, which
  is what stops a test from asserting nothing about LLM interaction.
- **Each rule is first-match-wins.** One rule per distinct event here; if a "then"
  is ever needed, use one rule with `respond_with_actions_from_event` branching on the
  event rather than two rules the matcher cannot tell apart.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features whois \
    --test client -- client::whois --test-threads=100
```
