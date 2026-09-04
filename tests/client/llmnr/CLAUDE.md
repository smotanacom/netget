# LLMNR Client Test Strategy

Four tests in `e2e_test.rs`, all mocked, all on 127.0.0.1. **4 passed, 0 failed, ~26s** at
`--test-threads=100`.

```bash
./cargo-isolated.sh test --no-default-features --features llmnr --test client -- client::llmnr --test-threads=100
```

Note the target form. `--test client::llmnr::e2e_test` does **not** work — cargo has no target by
that name, and it exits **0** after printing the list of real targets, so it looks like a pass
that ran nothing. The filter goes after `--`.

## What each test is for

| Test | Proves | LLM calls (client / server) |
|---|---|---|
| `accepts_a_matching_response` | a valid answer is accepted and attributed to the host that sent it | 3 / 2 |
| `discards_a_response_that_does_not_match_its_query` | **the client's central property**: wrong transaction ID *and* wrong echoed question are both refused, and each query still ends as an ordinary unanswered query with `discarded_count` 1 | 4 / 3 |
| `treats_no_answer_as_a_normal_outcome` | silence is reported as the expected outcome, `discarded_count` 0; also exercises the `target` override from a client opened on the real multicast group | 3 / 2 |
| `reports_two_responders_that_disagree` | two hosts, two different answers, one query → two `llmnr_response_received` plus `llmnr_conflicting_responses` | 5 / — |

Each test is its own process with its own mock, so the ~10-call suite guidance is met with room
to spare; the largest single process makes 5.

## Everything is unicast, deliberately — do not "fix" this

`PROTOCOL_ROADMAP.md` records the measurement, and it contradicts the usual assumption: bound to
`127.0.0.1`, joining a multicast group **succeeds**; *sending* to the group fails with
**`EADDRNOTAVAIL` (49)**, because loopback carries no multicast route. Bound to `0.0.0.0` both
work.

So no test sends to `224.0.0.252`. Two point `remote_addr` straight at an ephemeral port;
`treats_no_answer_as_a_normal_outcome` opens the client on the genuine group and redirects every
query with `send_llmnr_query`'s `target`, which is what that parameter exists for. If you see a
multicast join in a log here, it is not broken.

## The transaction ID must be echoed dynamically — and here it bites twice

Every UDP suite in this repo carries this warning; this one is the case where a hardcoded ID
fails for the *right* reason at the *wrong* step. The querier really does discard a mismatched
ID, so a static `transaction_id` in a server mock makes the happy-path test fail as a timeout,
looking exactly like the product bug the test is meant to catch. Always:

```rust
.respond_with_actions_from_event(|event| serde_json::json!([{
    "type": "send_llmnr_response",
    "transaction_id": event["transaction_id"].as_u64().unwrap_or(0),   // ← must be dynamic
    "name": event["name"].as_str().unwrap_or("printer.local."),
    "record_type": "A", "address": "192.168.1.42", "ttl": 30
}]))
```

`discards_a_response_that_does_not_match_its_query` inverts this on purpose: it XORs the event's
ID with `0x5555` (stays a valid `u16`, guaranteed to differ) for one query, and echoes a
*different name* for the other.

## `response_wait_secs` is raised to 12 in every test

A query collects for a fixed window and *then* reports; the 2s default is right for a link and
wrong for a hundred test processes sharing a machine, where the mocked model behind the responder
can take longer than that. Every client passes `response_wait_secs` through `open_client`'s
`startup_params`. A window that expires before the peer has spoken presents as "the querier
ignored a valid response" — a product bug, not a timing one, and it would be diagnosed as such.
`bind_address` is passed explicitly in the first test so both declared startup parameters are
exercised somewhere.

## Assertions are on log lines, not on mock counts alone

The counts prove the events fired; the log lines prove *what they said*. Two are worth keeping:

* `"…: no host claims this name (1 datagram(s) discarded)"` — the `1` is the point. "Nobody
  answered" and "somebody answered and was refused" must never look the same to the model, and
  only the count separates them.
* `"(1/1)"` / `"(2/2)"` — `responder_index`/`responder_count`. On a real link the count is the
  thing a reader needs, and asserting it stops a regression that collapses several answers into
  one.

`discards_a_response_that_does_not_match_its_query` deliberately declares **no** rule for
`llmnr_response_received`: if either forgery were accepted, that event would fire, the mock would
answer HTTP 500, and the run would show it.

## The two hand-written responders

`reports_two_responders_that_disagree` needs two hosts answering **one** query, which NetGet's own
responder cannot do — it sends exactly one datagram per query by design (`decide()` takes the
first output and warns about extras). So the test binds two `UdpSocket`s in one task: `a` receives
the query, and both `a` and `b` answer it with different addresses. The client sees two source
addresses for one transaction ID.

Two details that cost time if missed:

* `E2EResult`'s boxed error is **not `Send`**, so holding a `Result` across the `send_to().await`
  makes the whole task non-`Send` and un-spawnable. Hence `let Ok(bytes) = … else { continue };`
  rather than `if let Ok(…)`.
* The responses set `AA` (LLMNR's `C`) and `RD` (LLMNR's `T`) **clear**. Copying a line from any
  other DNS builder in this repo sets `AA` on an authoritative answer, which here asserts a name
  conflict — a different message.

These peers are **not** independent evidence: they use `hickory-proto`, same as both halves of
NetGet. What they test is this client's own decision about multiplicity.

## Circularity, and the one way out

Three tests use NetGet's own LLMNR responder, so both ends are this project's code; and all four
frame with `hickory-proto`, so the codec is asserted against itself. Circular on two axes — the
failure the root `CLAUDE.md` names for `webrtc_signaling`/`websocket`, and the reason the protocol
is rated `Experimental`.

`src/client/llmnr/CLAUDE.md` has the full finding on `llmnr-poison`, which **is** a library
(contradicting the server half's note), depends only on `anyhow` + `tokio` so it shares no codec
with NetGet, and exposes `llmnr_response(query: &[u8], spoof: Ipv4Addr) -> Option<(String,
Vec<u8>)>` as a pure function — the test would keep its own ephemeral unicast socket and let that
crate own the encoding. Its `poison(spoof)` entry point is unusable (no bind address; claims fixed
port 5355 and NBT-NS 137). No dependency was added; the line is recorded there. Even wired up it
would not reach Beta — an independent *encoder* is not a running responder.

## Mock rule ordering

`and_event_data_contains` is a **substring** match and first-match-wins. The names here
(`printer.local`, `spoofed.local`, `mislabelled.local`, `stranger.local`, `contested.local`) share
no substring with one another on purpose. If you add a name, check it is not a prefix of an
existing one — that exact mistake once looped the DNS client into a stack overflow.
