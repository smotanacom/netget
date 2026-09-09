# LLMNR E2E Test Strategy

`e2e_test.rs`, two tests, one file. Run:

```bash
./cargo-isolated.sh test --no-default-features --features llmnr --test server -- llmnr --test-threads=100
```

Both pass. Declared in `tests/server/llmnr/mod.rs`, which is declared in `tests/server/mod.rs`
— check with the `comm` one-liner in the root `CLAUDE.md` if you add a file here.

## The evidence is circular, and that is the point of this section

**These tests build their queries with `hickory-proto`, which is the same crate
`src/server/llmnr/actions.rs` encodes its responses with.**

That is the failure mode the root `CLAUDE.md` names for `webrtc_signaling` and `websocket`:
the "independent peer" turns out to be the library the server itself frames with, so the test
asserts only that one crate round-trips through itself. `tests/server/websocket/e2e_test.rs`
escapes it by hand-writing a raw RFC 6455 client. There is no equivalent escape here, and a
hand-written client would not be one either — the root `CLAUDE.md` is explicit that an in-test
hand-rolled client is an independent *reading* of the spec, not an independent implementation
(the `dhcp` and `usb/serial` cases).

**What was checked before concluding that:**

| candidate | result |
|---|---|
| `resolvectl` / `systemd-resolve` (`--protocol=llmnr`) | not present — systemd, not macOS |
| `avahi-resolve` | not present, and it speaks mDNS anyway |
| Windows resolver | not runnable here |
| crates.io, LLMNR | only `llmnr-poison`, a Responder-style **responder** — the same role as our server, not a querier |
| `hickory-resolver`, `mdns-sd`, `simple-mdns` | DNS and mDNS; neither issues LLMNR queries |

So the protocol is rated **Experimental** and says so in `metadata().notes`. **Do not promote
it to Beta on the strength of these tests.** Promotion needs a query from a Windows host or
systemd-resolved, on a real link, and that means a human with a second machine.

What the tests *do* prove is NetGet's own wiring, which is the part that actually breaks: the
event fires with the right fields, routing reaches it, each action executes, the transaction
ID and question survive the round trip, the header bits are right, and silence is silent on
three separate paths.

## Test 1 — `test_llmnr_answers_only_for_names_it_owns`

One server, four queries, four mock rules — one for startup and **three keyed on names**.
Shared deliberately: the LLM-call budget is per suite, and the three names have no
substring relation (`printer.local.`, `stranger.local.`, `broken.local.`), so
first-match-wins cannot misroute them. That is a property of the names chosen, not
something `and_event_data_contains` enforces — it matches on substring, so a fourth name
like `printer2.local.` would silently be answered by the `printer.local.` rule.

| step | query | expected |
|---|---|---|
| 1 | UDP `printer.local.` A | an answer: ID + question echoed, one A record, TTL 30 |
| 2 | UDP `stranger.local.` A | **nothing** (`decision=model_silent`) |
| 3 | UDP `broken.local.` A | **nothing** — the model asked for RCODE REFUSED and it was suppressed (`decision=model_reject_suppressed_udp`) |
| 4 | TCP `broken.local.` A | RCODE 5, ID + question echoed, no answers |

The port comes from `helpers::get_available_port()` rather than `port: 0`, because step 4
connects to the TCP listener the server binds on the *same* port; a port that was just proven
bindable for TCP makes that collision-free in practice.

### The transaction ID is echoed dynamically

`respond_with_actions_from_event` reads `event["transaction_id"]`. A hardcoded ID is the
documented cause of client timeouts in every UDP suite in this repo — the querier picks the ID
at random and discards a response carrying anything else, which looks exactly like the server
having said nothing.

### Byte-level assertions come first

Before any decoding, on the raw datagram:

```rust
assert_eq!(buf[2] & 0x04, 0, "the LLMNR C (Conflict) bit occupies DNS's AA position");
assert_eq!(buf[2] & 0x01, 0, "the LLMNR T (Tentative) bit occupies DNS's RD position");
```

This is the assertion most worth keeping. Every other DNS builder in this repo sets `AA` on an
authoritative answer, and in LLMNR that bit is `C` — "a conflict was detected on this link".
One copied line from `dns/actions.rs` would change the meaning of every response this server
sends, and a decoder-level assertion would not notice, because `hickory-proto` would happily
report `authoritative: true` as a perfectly valid DNS header.

## Test 2 — `test_llmnr_is_silent_when_the_model_cannot_be_reached`

The one that matters most. A second server whose mock has **no rule for `llmnr_query`**, so the
mock answers HTTP 500, `call_llm` returns `Err`, and the server takes its LLM-failure path —
the same technique as `tests/server/dns/llm_failure_test.rs`, and the same shape as a real
backend outage.

DNS's version of this test asserts a **SERVFAIL packet**. LLMNR's asserts **no packet at all**,
and the inversion is the whole point: an LLMNR response is a name-to-address binding written
into the querier's resolver cache, so a fabricated answer during an outage is cache poisoning,
and the only error frame LLMNR has is an RCODE the RFC forbids in response to a multicast
query. There is nothing legal to send. See `src/server/llmnr/CLAUDE.md`.

## Asserting an absence without asserting a race

A test that sends a datagram, sleeps, and finds nothing has proven nothing: the server may
simply not have got there yet. Every silence assertion here therefore waits for the server's
own decision line first:

```rust
server.wait_for_log("decision=model_silent", 30).await?;
expect_silence(&socket, "unowned name").await?;
```

By the time `expect_silence` runs, the server has finished deciding, so its 1.5s timeout means
"it decided to say nothing", not "it was still thinking". This also pins the *reason*:
`model_silent`, `model_reject_suppressed_udp` and `fail_closed_llm_error` are byte-identical
on the wire — all three are zero bytes — so the log token is the only thing that distinguishes
a correct refusal from a backend outage. Conflating them is the OAuth2 defect the root
`CLAUDE.md` records, and it would hide a total outage as protocol-correct behaviour forever.

## Mock budget

| | calls |
|---|---|
| test 1 | 1 startup + 4 events = **5** |
| test 2 | 1 startup + 1 unmatched (the deliberate 500) = **2** |

Seven, under the ~10 guideline. Both tests finish with `wait_for_mocks(30)` and then
`verify_mocks()` — without the latter the tests assert nothing about LLM interaction at all.

## Not covered

* **Multicast.** The tests are unicast to loopback. Joining `224.0.0.252` on loopback is
  unreliable on macOS, so the server treats a join failure as non-fatal and the tests do not
  depend on one. Nothing here exercises a datagram that actually arrived via the group.
* **IPv6.** No `FF02::1:3` path is tested; `join_multicast_v6` has never run.
* **PTR and AAAA answers. Nothing in the tree executes either, at any level.** This entry
  used to claim `executable_examples_test` covered them; it does not. That test runs each
  action's own `example`, and `send_llmnr_response`'s example is `record_type: "A"`. The
  AAAA JSON exists only as an `EventType` *alternative* example, which nothing executes
  (`protocol_examples_test` checks it has a `"type"` field and stops there), and PTR
  appears in no example anywhere. So `parse_record_type`'s AAAA and PTR arms and their
  rdata construction have never been run by a test — the A path is the only path.
* **The `C`/`T` bits in an inbound query.** They are parsed into the event and never asserted
  on; no test sets them.
* **QDCOUNT/OPCODE discard rules.** Implemented and logged, untested.
