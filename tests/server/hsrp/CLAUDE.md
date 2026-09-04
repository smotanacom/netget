# HSRP E2E Test Strategy

`e2e_test.rs`, three tests, one file. Run:

```bash
./cargo-isolated.sh test --no-default-features --features hsrp \
    --test server -- server::hsrp --test-threads=100
```

All three pass. Declared in `tests/server/hsrp/mod.rs`, which is declared in
`tests/server/mod.rs` — check with the `comm` one-liner in the root `CLAUDE.md` if you add a
file here.

> Note the invocation form: `--test server -- server::hsrp`. `--test server::hsrp::e2e_test`
> names a test *target*, which does not exist — cargo lists the targets and exits.

## The transport really executes, and that is unusual for this tier

**Port 1985 is above 1023**, and joining a multicast group needs no elevation, so HSRP declares
`PrivilegeRequirement::None`. Nothing here is stubbed: the tests bind a real UDP socket, send
real datagrams into the running server, and assert on the real bytes that come back. Its
neighbours in the roadmap's routing/L2 tier (`arp`, `isis`, `ospf`, and the `wireguard` case
the root `CLAUDE.md` records at length) cannot say that — they need root or raw sockets, so
their suites prove the wiring and stop at the socket.

That is the whole of HSRP's advantage, and it is worth being precise about what it buys. It
proves NetGet's own machinery end to end — the event fires with the right fields, routing
reaches it, each action executes, both packet layouts are byte-exact, the reply lands on the
peer's socket, and silence is genuinely silent. It proves **nothing** about interoperability.

## The peer is hand-written, and that caps the rating

There is no HSRP crate on crates.io in any role, and no runnable HSRP speaker on this machine
(`keepalived` and `vrrpd` are absent, and speak **VRRP** — a different protocol — anyway). The
only genuine HSRP peer is a Cisco device.

So the packets below are **hand-written from RFC 2281 and Cisco's HSRPv2 documentation**. The
root `CLAUDE.md` is explicit that this is an independent *reading* of the spec and not an
independent implementation — the same standing as `dhcp`'s in-test RFC 2131 decoder and
`usb/serial`'s USB/IP client. It is the strongest evidence available here, and it is not Beta
evidence. **Do not promote HSRP on the strength of this file.**

### Byte-for-byte, not round-tripped

Every expected packet is a **literal `[u8; N]` array with a comment per field**, compared
against the raw datagram. Nothing is decoded with `codec.rs` first.

That is the entire point. Round-tripping a packet through the server's own encoder and decoder
proves only that the codec agrees with itself — the circular-evidence failure the root
`CLAUDE.md` names for `webrtc_signaling`/`websocket`. Since the peer here is already only a
reading of the spec, letting the *assertions* be circular too would leave nothing at all. If a
field moves, changes width, or changes byte order, these literals fail.

## Test 1 — `test_hsrp_v1_handles_hello_coup_and_resign`

One server, three inbound datagrams, four mock rules. The three event rules key on distinct
event ids, so first-match-wins cannot misroute them.

| in | event | model answers | out |
|---|---|---|---|
| Hello, `active`, priority 120 | `hsrp_hello_received` | `send_hsrp_hello` `listen`/90 | 20 byte-exact bytes |
| Coup, `speak`, priority 200 | `hsrp_coup_received` | `send_hsrp_coup` `active`/250 | 20 byte-exact bytes |
| Resign, `initial` | `hsrp_resign_received` | `no_advertisement` | **nothing** |

This is what makes all three events *and* all four actions reachable in one server. An event
declared and never emitted is a defect `event_emit_sites_test` catches statically; this
catches the runtime half — that the **right** event fires for each opcode.

### The matchers are assertions

`.and_event_data_contains(...)` is not decoration. A rule that does not match falls through to
a real LLM call, the server answers nothing, and `verify_mocks()` reports zero calls — so each
matcher is a hard assertion about what the server decoded:

* `state` = `"active"` — v1 state code **16** decoded correctly.
* `state` = `"speak"` — v1 state code **4** decoded correctly. See the collision below.
* `auth_data` = `"cisco"` — the 8-byte NUL-padded field came back as the bare string, i.e. the
  padding was stripped rather than carried into the value.
* `virtual_ip` = `"192.168.1.1"` — the trailing four bytes are the VIP, not something else.

### Claiming the gateway must be loud

After the Coup reply, the test waits for `"claiming the gateway role"` in the log. That WARN is
the only thing standing between an operator and reconstructing a gateway takeover from a packet
capture after the segment has already been black-holed. It is asserted so it cannot be quietly
dropped.

## Test 2 — `test_hsrp_v2_tlv_format_is_byte_exact`

A second server started with `startup_params: {"version": 2}`, driven with a 52-byte HSRPv2
datagram (a 42-byte Group State TLV plus a 10-byte Text Authentication TLV).

The expected reply pins every v2-specific thing that differs from v1: the TLV framing, the
4-byte priority (one byte in v1), the **millisecond** timers (`3000` = `00 00 0b b8`, not `3`),
the 16-byte virtual IP field with an IPv4 address in the first four bytes, the 6-byte
identifier, and the Text Authentication TLV as a *separate TLV* rather than an inline field.

### The state-code collision is pinned from both sides

This is the assertion most worth keeping.

| state | HSRPv1 code | HSRPv2 code |
|---|---|---|
| Speak | **4** | 3 |
| Standby | 8 | **4** |

Code `4` means **Speak** in v1 and **Standby** in v2. It mis-decodes *silently* — both are
valid states, so nothing errors and no round-trip test notices, but a real segment reads the
opposite of what was meant.

Test 1 sends code 4 to a v1 server and requires `"speak"`. Test 2 sends code 4 to a v2 server
and requires `"standby"`. Either table applied to both versions fails one of them. A single
test, in either direction, would pass with the bug present.

## Test 3 — `test_hsrp_is_silent_when_the_model_cannot_be_reached`

The one that matters most. A third server whose mock has **no rule for `hsrp_hello_received`**,
so the mock answers HTTP 500, `call_llm` returns `Err`, and the server takes its LLM-failure
path — the same technique as `tests/server/dns/llm_failure_test.rs`, and the same shape as a
real backend outage.

DNS's version of this test asserts a **SERVFAIL packet**. HSRP's asserts **no packet at all**,
and the inversion is the point. HSRP has **no negative message of any kind** — no error frame,
no NAK — so the only thing that could be sent is an advertisement, and every advertisement is a
positive claim about who owns the segment's gateway address. A fabricated Hello during an
outage can win an election NetGet cannot serve, and every host on the link then sends its
off-subnet traffic into a black hole. See `src/server/hsrp/CLAUDE.md`.

## Asserting an absence without asserting a race

A test that sends a datagram, sleeps, and finds nothing has proven nothing: the server may
simply not have got there yet. Both silence assertions therefore wait for the server's own
decision line first:

```rust
server.wait_for_log("decision=model_silent", 30).await?;
expect_silence(&socket, "no_advertisement").await?;
```

By the time `expect_silence` runs the server has finished deciding, so its 1.5s timeout means
"it decided to say nothing", not "it was still thinking".

This also pins the *reason*, which is the part that would otherwise rot. `model_silent` and
`fail_closed_llm_error` are byte-identical on the wire — both are zero bytes — so the log token
is the only thing distinguishing a deliberate refusal to join the election from a total backend
outage. Conflating them is the OAuth2 defect the root `CLAUDE.md` records, and here it would be
especially invisible: **a silent HSRP speaker is entirely normal**, so an outage would look like
correct behaviour indefinitely.

## Mock budget

| | calls |
|---|---|
| test 1 | 1 startup + 3 events = **4** |
| test 2 | 1 startup + 1 event = **2** |
| test 3 | 1 startup + 1 unmatched (the deliberate 500) = **2** |

Eight, under the ~10 guideline. All three finish with `wait_for_mocks(30)` and then
`verify_mocks()` — without the latter the tests assert nothing about LLM interaction at all.

## Not covered

* **Multicast, in either direction.** The tests are unicast to loopback. Joining a group on
  loopback succeeds on macOS but *sending* to one fails with `EADDRNOTAVAIL` (measured; see
  `PROTOCOL_ROADMAP.md`), so the server treats a join failure as non-fatal and replies unicast
  to the sender. Nothing here exercises a datagram that actually arrived via a group.
* **IPv6.** `FF02::66`, port 2029 and `join_multicast_v6` have never run.
* **HSRPv2 MD5 authentication.** The parser reports it structurally; no test feeds it a type-4
  TLV.
* **The `identifier` action parameter.** Test 2 asserts it is *decoded* from an inbound packet,
  but every outbound packet uses the all-zeros default.
* **Malformed input.** The unparseable-datagram path (`debug!` and drop, no event, no packet) is
  implemented and untested.
* **Resign as an outbound action.** `send_hsrp_resign` is covered only by
  `executable_examples_test` at the executor level, which is weaker than a wire assertion.
* **A real election.** Priorities, preemption and role changes across datagrams do not exist
  here — by design; see `src/server/hsrp/CLAUDE.md`.
