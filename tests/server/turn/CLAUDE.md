# TURN E2E Tests

## What this suite is for

**Proving the relay relays.** For most of its life the TURN server answered Allocate,
Refresh and CreatePermission with well-formed packets and forwarded nothing — no relay
socket was ever bound. A suite that only asserts "NetGet replied 0x0103 with an
XOR-RELAYED-ADDRESS attribute" passes perfectly against that server, which is why the
previous ten tests here were worth very little.

So the load-bearing test is `test_turn_relays_payload_between_two_peers`: a second,
ordinary `tokio::net::UdpSocket` sits on the far side of the relayed transport address,
and a payload has to cross in **both** directions.

- peer → client: the peer sends to the relay address; the client must receive a Data
  indication whose XOR-PEER-ADDRESS names the peer and whose DATA is byte-identical.
- client → peer: the client sends a Send indication; the peer must receive the raw bytes
  **from the relay address**, which also proves they left the allocation's own socket and
  not the TURN listener.

Neither assertion can pass unless a socket was really bound and datagrams were really
forwarded. Nothing in the suite asserts that NetGet's encoder agrees with NetGet's decoder.

## Why the tests encode the wire format themselves

Requests are built and responses parsed in the test file from RFC 8656 / RFC 8489, with
message types written as the literal constants from RFC 8656 section 17. `webrtc-turn`'s
client would be the more independent choice, but its `RelayConn`'s `send_to`/`recv_from`
come only from `impl util::Conn`, and `webrtc-util` is not a dependency of this crate —
see the library-choice section of `src/server/turn/CLAUDE.md`.

One consequence: a symmetric bug in this file's encoder and NetGet's would hide itself.
Two things guard against that:

- the ground truth is outside the encoder/decoder pair — the relay address has to be a
  socket that actually receives the peer's datagrams, so a wrongly decoded address fails;
- `allocate()` asserts the XOR-RELAYED-ADDRESS bytes for 127.0.0.1 against the literal
  values RFC 8489 section 14.2 mandates (`[0x7F^0x21, 0x00^0x12, 0x00^0xA4, 0x01^0x42]`),
  which catches "the XOR was never applied" — a bug that round-trips fine.

## Tests and LLM call budget

| Test | Calls | What it pins down |
|---|---|---|
| `test_turn_relays_payload_between_two_peers` | 3 | Relaying in both directions; unpermitted peer traffic dropped before CreatePermission; relay socket is distinct from the listener |
| `test_turn_relays_over_a_bound_channel` | 3 | ChannelBind, then ChannelData framing in both directions; a channel bind implies permission |
| `test_turn_stops_relaying_after_the_lifetime_expires` | 3 | Relay works, then stops within the granted 5s lifetime — well before the 30s cleanup tick |
| `test_turn_refuses_an_invented_relay_address` | 2 | A model-chosen relay address is refused with 508 and no XOR-RELAYED-ADDRESS; a bad magic cookie is dropped silently |
| `test_turn_denied_allocation_and_refresh` | 3 | The model's refusal reaches the client as 486; nothing is relayed without an allocation; a granted Refresh reports the model's lifetime |
| `static_default_test::test_turn_grants_nothing_without_policy_and_needs_no_llm` | 1 | With no operator policy, Allocate is fail-closed **and silent**, with zero LLM calls (`expect_calls(0)` on the event rule) |
| `llm_failure_test::test_turn_answers_the_client_with_a_category_when_the_llm_errors` | 1 | With a policy configured but the backend erroring, the client gets a 500/508 Allocate error response whose reason phrase is a `WireFailure` category and leaks nothing |

**16 LLM calls total** (each row includes its server-startup call). Slightly over the ~10
guidance, and the reason is that the four behaviours worth separating — relaying, channel
framing, expiry, refusal — cannot share one server: the expiry test needs a 5-second
allocation and the refusal tests need a model that says no.

Every mock for a TURN event uses `respond_with_actions_from_event`, because the transaction
ID **and** the relay address must be echoed from the event. A static mock with a hardcoded
relay address is now a test of the refusal path, not of allocation (that is exactly what
`test_turn_refuses_an_invented_relay_address` does).

Every test ends with `test_state.verify_mocks().await?`.

## Traps worth knowing

- **Permissions match on IP address only** (RFC 8656 section 9). On loopback every socket
  shares 127.0.0.1, so a "stranger" socket is indistinguishable from a permitted peer. The
  unpermitted-peer assertion therefore runs *before* CreatePermission, not after. An
  earlier version of this test asserted the opposite and failed correctly.
- **Timing.** `expect_silence` windows are 1–1.5s and the expiry test sleeps 6s against a
  5s allocation, so it survives a loaded machine; the "relay works" half runs immediately
  after CreatePermission. Response waits are 10s because the netget binary is a subprocess.
- Relayed payloads are asserted byte-identical: no truncation, no padding leaking into DATA.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features turn \
    --test server -- --test-threads=100 turn
```

Runtime is about 9s with the mock LLM (the expiry test's 6s sleep dominates).

## Not covered

- A real TURN client (`turnutils_uclient`, libwebrtc, Pion) — the biggest remaining gap,
  and the reason the protocol is `Experimental` rather than `Beta`.
- Authentication: none is implemented, so there is nothing to test.
- TCP allocations, IPv6 relay addresses, RESERVATION-TOKEN, EVEN-PORT, DONT-FRAGMENT.
- Permission expiry (5 minutes) and channel expiry (10 minutes) — the timers exist and are
  enforced on the relay path, but a test would have to sleep for minutes.
- The 256-allocation cap and its 508 refusal.
- The LLM-error path for Refresh / CreatePermission / ChannelBind — they share
  `call_llm_for_event`, so `llm_failure_test` covers the branch through Allocate only.
- Concurrent relaying throughput.
