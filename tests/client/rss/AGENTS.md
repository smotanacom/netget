# RSS client test strategy

The RSS client had **no tests** before August 2026, and could not have had any: it was
commented out of `client_registry.rs` and `src/client/mod.rs` from the moment it was written
(`e16ffb2d`, 2025-11-09, "client needs API updates") and nothing unregistered runs.

## Peer honesty

Two halves, with different strength:

- **The feed bytes are independent.** `FEED_XML` was hand-written from the RSS 2.0
  specification, not emitted by the `rss` crate the client parses with. So the parse is
  checked against bytes the parser did not produce, which is the part that matters.
- **The HTTP server is not independent-ish so much as trivial.** It is ~20 lines in the test
  file that answers any GET with a fixed response. It is not a real HTTP server and does not
  exercise chunked encoding, redirects, compression or keep-alive. Pointing the client at a
  real feed server would be stronger; pointing it at a *public* feed would violate the
  "localhost only, never contact external endpoints" rule, so it is not an option.

## Layer 1 — URL resolution

`resolves_paths_against_the_base_and_leaves_absolute_urls_alone`, 0 LLM calls.

Small but worth a test: the model may give `/news.xml`, `news.xml` or a full URL, and getting
this wrong sends the request to the wrong host — a failure that looks like a network problem
rather than a bug.

## Layer 2 — E2E, LLM mocked

`fetches_and_parses_a_feed_into_structured_items`: **3 LLM calls** (startup, `rss_connected`,
`rss_feed_fetched`).

The interesting bit is how it asserts. RSS's whole point in NetGet is that the model receives
**structured items, never raw XML**, so the mock's `rss_feed_fetched` branch checks
`feed_title`, `item_count`, the first item's `title` and its `author`, and only then answers
`disconnect`. If any of those is wrong it instead answers `fetch_rss_feed` for
`/PARSE-WAS-WRONG.xml` — which the in-test server records, so the assertion on the requested
paths fails with a name that says exactly what went wrong.

That indirection exists because the alternative — asserting on the client's log output —
would pass for a client that fetched the feed and then dropped it on the floor.

The path assertion is also what proves URL resolution worked end to end: the server records
`/tech-news.xml`, which only happens if the bare path was joined to the client's base address.

## Not covered

- **Chained fetches.** The client will queue a further `fetch_rss_feed` returned from
  `rss_feed_fetched`, capped at 16 per client (`MAX_CHAINED_FETCHES`). Neither the chaining
  nor the cap is tested.
- **HTTPS.** `reqwest` handles it, but no test uses it.
- **Failure paths**: non-200 responses, unparseable XML, connection refused. The client logs
  and sets `ClientStatus::Error`; nothing asserts that.
- **Atom, RSS 1.0, enclosures, categories, conditional requests.** Not implemented — the
  `if_modified_since` parameter that used to be declared was *removed* when the client was
  re-enabled, precisely because nothing read it (a declared-but-unread parameter is dead
  weight the model will try to use).

## Running

```bash
CARGO_TARGET_DIR=/tmp/tgt ./cargo-isolated.sh test --no-default-features --features rss \
    --test client::rss -- --test-threads=100
```

## `command_channel_test.rs`

Covers `AppState::send_to_client` injecting an action into a running rss client (the
dashboard's `[ send ]`). **Zero LLM calls**: the client's LLM points at
`http://127.0.0.1:1`, so its connected-event call fails and the loop must tolerate that —
part of what the test verifies. It always `wait_for_client_handle`s before sending, which is
the regression guard for "register the command channel *before* the connected-event LLM
call"; register it after and a client whose connect event parks on a manual rule reads "no
command channel" for the whole park.

Asserts the exact `ClientSendOutcome` variant. A successful request is
`Executed { detail }`, **not** `Sent` — reqwest/h3 own the socket and report no wire byte
count, so a byte count would be invented; the detail carries what actually came back
instead. An unknown action must be `Rejected` (not silently swallowed), and `disconnect`
must be `Disconnected` and leave the client with no command handle.

The peer is a NetGet RSS server of our own with a `*` static handler generating a two-item
feed, so `-> 2 items` in the outcome detail proves the feed was fetched *and* parsed by the
`rss` crate, and the server's access log is checked for the requested path.
