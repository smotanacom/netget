# VRRP / CARP test strategy

Two files with different jobs, and the split is the whole strategy:

| File | Proves | Needs |
|---|---|---|
| `codec_test.rs` | the VRRP/CARP codec against **literal specification bytes**, both directions, plus that we drive SHA-1/HMAC-SHA1 correctly against published vectors | nothing — no socket, no LLM, no privilege |
| `e2e_test.rs` | the full advertisement → event → model → action → packet path, the silence guarantee, and startup-parameter handling | a mock Ollama on loopback |

29 tests, all passing, ~5 s at `--test-threads=100`.

```bash
./cargo-isolated.sh test --no-default-features --features vrrp \
    --test server -- vrrp:: --test-threads=100
```

**LLM call budget: 4.** One per exchange test
(`an_advertisement_produces_the_advertisement_the_model_decided_on`,
`a_priority_zero_advertisement_raises_the_master_resigned_event`,
`no_advertisement_transmits_nothing_and_is_logged_as_a_decision`,
`a_carp_server_decodes_and_answers_in_carp`). The silence test makes zero calls reach a mock
*by design* — its backend is a closed port.

## Why the literals, and why they are not circular

Encoding with our encoder and decoding with our decoder proves only that the two agree with
each other. The root `CLAUDE.md` names that as circular evidence, and it is exactly the mistake
that held `rss` at Experimental while its test round-tripped one crate through itself.

There is no third-party VRRP or CARP codec in this tree and no peer that can be run here — the
raw transport needs root — so the external reference is **the specification's field table,
written out by hand as octets, with every checksum worked through by hand in the comment above
it**.

**Provenance, stated plainly:** every byte string in `codec_test.rs` was assembled from
RFC 3768 §5.1 (VRRPv2), RFC 5798 §5.1 (VRRPv3) and OpenBSD's `sys/netinet/ip_carp.h`
(`struct carp_header`). They are **not** extracted from a named packet capture, and the tests
do not claim they are. What makes them evidence is that the offsets, the byte order, the
interval scaling, the checksum scope and the pseudo-header were written out here independently
of the encoder — an encoder that gets any of them wrong disagrees with this file rather than
with itself.

If you can get a real capture, replacing these with bytes from one is a strict improvement and
worth doing. Say where they came from when you do.

### The hash vectors changed job, and were kept

SHA-1 used to be hand-written here and the vectors proved it was correct. It is now the `sha1`
crate's (`vrrp = ["socket2/all", "dep:sha1"]`), with `codec::sha1` a thin adapter and only the
RFC 2104 HMAC construction still local — no HMAC crate is reachable from this feature.

Every vector was kept, and **what they assert changed**: not that SHA-1 is correct, which is
the crate's problem, but that this code **drives** it correctly. That failure mode did not go
away with the hand-rolled implementation. An adapter that hashed the wrong buffer, dropped an
`update`, truncated the digest or returned a stale array would pass every other test in the
file and fail these immediately.

* `sha1_matches_the_published_vectors` — FIPS 180 / RFC 3174 §7.3, including the million-'a'
  vector, which now catches an adapter that silently truncates its input rather than a broken
  block loop.
* `sha1_agrees_with_the_streaming_api_at_every_padding_boundary` — the 0..=130 sweep, kept, with
  its oracle changed. Comparing against `Sha1::digest` would now be tautological, so the
  reference feeds the hasher **one octet at a time**: streaming and one-shot are different code
  paths through the crate, and a wrapper that passes a truncated slice or copies out part of a
  digest shows up as a disagreement between them. It also asserts all 131 digests are distinct,
  which catches a wrapper whose output does not depend on its whole argument.
* `hmac_sha1_matches_the_rfc_2202_vectors` — cases 1, 2, 3 and 6 (the 80-octet key RFC 2104 §2
  requires to be hashed down rather than truncated), run through **both** `codec::hmac_sha1`
  **and** the test's own `reference_hmac_sha1`. The construction is the part still written by
  hand, and these are exactly what catch swapping the `0x36`/`0x5c` pads, skipping the
  over-long-key hash, or concatenating inner and outer the wrong way round.

Running the vectors through the test's own reference too is what lets it serve as the oracle
for the CARP input ordering below: the RFC pins both, so neither is trusted on the other's
say-so.

### The four literal packets

* **`VRRP_V2_ADVERTISEMENT`** — 20 octets: version/type `0x21`, VRID 1, priority 100, one
  address, auth type 0, interval `0x01` (one **second**), checksum `0xb952`, and the two zeroed
  authentication-data words RFC 3768 keeps at the end. The checksum arithmetic is in the doc
  comment: `0x2101 + 0x6401 + 0x0001 + 0xc0a8 + 0x0101 = 0x1_46AC`, folded, complemented.
* **`VRRP_V2_RESIGNATION`** — the same group with priority 0, checksum `0x1d53`.
* **`VRRP_V3_ADVERTISEMENT`** — 12 octets: version/type `0x31`, interval `0x0064` (one hundred
  **centiseconds**), checksum `0x06b6`, no trailing authentication data. The checksum is
  computed over the RFC 5798 §5.2.8 pseudo-header
  (`c0a80102 | e0000012 | 00 | 70 | 000c`) *and* the message, both sums written out.
* **`CARP_ADVERTISEMENT`** — 36 octets of `struct carp_header`: `0x21`, vhid 1, advskew 0,
  authlen 7, demote 0, advbase 1, checksum `0xdef6`, zero counter, zero HMAC.

The first three of those are re-declared in `e2e_test.rs` and sent over the socket, so what
crosses the wire in the e2e tests is independently pinned bytes rather than whatever the
encoder produced that day.

### The assertions worth keeping

**Seconds versus centiseconds.**
`the_advertisement_interval_is_seconds_in_v2_and_centiseconds_in_v3` asserts `0x01` at v2's
octet 5 and `0x00 0x64` at v3's octets 4-5, *and* asserts v3 is **not** `0x00 0x01`. The
negative half is what makes a failure legible: without it a regression reports `1 != 100` and
the reader has to work out which side is wrong. It also asserts v3's four reserved bits stay
clear — the same octet is v2's authentication type, a different field entirely.
`the_v3_interval_field_is_twelve_bits_and_refuses_what_it_cannot_hold` walks the ceiling
(40.95 s = `0x0fff`; 40.96 s refused, not wrapped to zero) and
`vrrpv2_cannot_express_a_sub_second_interval_and_says_so` checks the refusal points at the
version that *can* express it — a refusal that does not say why trains people to round the
number instead.

**The two checksum scopes.**
`the_v3_checksum_covers_the_pseudo_header_and_the_v2_checksum_does_not` asserts four things:
the same v3 body addressed to two destinations differs *only* in the checksum octets; a v3
packet validated without its pseudo-header fails (what a v2-only routine would do to it); a v2
packet's checksum is unchanged whether or not a pseudo-header is supplied; and a v3 encode with
no pseudo-header **errors** rather than emitting a message-only checksum. That last one
matters because a silently-wrong checksum is discarded by every conformant peer, which presents
as the server being down.

**CARP is not VRRP.** `a_carp_advertisement_misdecodes_as_a_vrrpv2_resignation` is the test to
read first. It feeds the real CARP literal to `VrrpAdvertisement::decode` and asserts it
**succeeds** — CARP's `authlen` (7) lands on VRRP's address count and 8 + 7·4 is exactly 36, so
the length check passes — and that the result claims seven virtual addresses and reads as a
master *resigning*, because CARP's `advskew` (0) lands on VRRP's priority. That is why the
server takes a `variant` startup parameter instead of sniffing the first octet, which is `0x21`
for both.

**The CARP HMAC.** `the_carp_hmac_is_hmac_sha1_over_the_openbsd_input` rebuilds the key and the
message by hand and runs them through `reference_hmac_sha1`, so it pins that `carp_hmac` really
is HMAC-SHA1 over `version || type || vhid || addresses || counter` keyed with the passphrase
zero-padded to 20 octets. What is being checked here is the **input construction**, not the
HMAC — both sides now compute the hash with the same crate, and the reference is trustworthy
as an oracle only because the RFC 2202 vectors above are run through it as well.

It then varies each of the four inputs and asserts the result changes — a field that
authenticates nothing would pass a single-vector test — and checks that a passphrase longer
than 20 octets is truncated rather than hashed, matching `CARP_KEY_LEN`.

What this does **not** prove is that OpenBSD agrees about the input ordering. There is no CARP
RFC, the construction is a reading of `ip_carp.c`, and no `carpd` has ever accepted a packet
from this code. That caveat is unchanged by the move to the `sha1` crate and is the reason the
protocol stays `Experimental`.

## Why `e2e_test.rs` does not use the harness

Every other server suite starts the `netget` binary and lets `server_startup` bring the
protocol up. **That cannot work here**, and the reason is worth knowing before you try:

`server_startup`'s privilege gate is **per-protocol, not per-transport**. VRRP declares
`PrivilegeRequirement::RawSockets`, and `requires_privileges` is `!privilege_met` for that
variant, evaluated *before* the startup parameters are read. So an unprivileged `start_server`
is refused even with `transport: "udp"`, which needs no privilege at all. Declaring anything
weaker would be a lie about the raw transport, which is the real one. `stp` has the same
constraint for the same reason.

So these tests build a `SpawnContext` by hand and call `Server::spawn(ctx)` directly. That
still exercises everything this protocol owns — startup-parameter parsing, the bind, the
decode, the event, the dispatch, action execution, the re-encode and the transmit. Only
`server_startup`'s own gate is bypassed, and that gate is not this protocol's code.

The harness pieces that *are* used directly are `MockOllamaServer` and `MockLlmBuilder`, the
same way `tests/empty_static_handler_test.rs` uses them. The state is built with
`AppState::new_with_options(false, mock.base_url())` plus `set_llm_client`, and
**`set_ollama_model(Some("mock-model"))`** — without pinning the model, `ensure_model_selected`
falls back to probing a real Ollama on `localhost:11434` and the test starts depending on the
developer's machine.

`wait_for_expectations(30)` then `verify_calls()` on every test that uses a mock, per the root
`CLAUDE.md`: waiting on the expectations waits on the exchange, and `verify_calls` is the thing
that actually asserts.

## The UDP transport in tests

One **complete VRRP or CARP message per datagram** — the same octets the raw transport would
put on the wire, with only the IP layer simulated. The reply comes back to the datagram's
sender.

Because VRRPv3 folds the IP source and destination into its checksum, the transport
reconstructs that pseudo-header by convention: inbound, the datagram's sender and
`224.0.0.18`; outbound, the bound address (`127.0.0.1` here) and whatever `destination` the
action resolved to. **The inbound literals in this suite are deliberately VRRPv2 and CARP**,
whose checksums cover the message alone, so no test depends on that convention holding. The
*outbound* v3 checksum is asserted against it explicitly in the first test.

The group is configured with `priority: 150`, `advert_interval: 3` and two virtual addresses —
none of which is the protocol default or anything the tests' actions ask for, so an assertion
on those values proves the startup parameter was read rather than that a constant matched.

## What each e2e test is for

**`an_advertisement_produces_the_advertisement_the_model_decided_on`** — the whole path. The
model is told to take the gateway with priority 200, which is the actual hazard this protocol
makes possible, and its action names **only** the priority. The reply is decoded and asserted
field by field: priority really is 200, and the version, VRID, interval and virtual addresses
all came from the startup parameters rather than from a constant *or* from the inbound packet
(which was v2, interval 1 s, one address — all different). That last group is the `ospf` defect
the root `CLAUDE.md` records: four of its six parameters were advertised to the model and
reached the wire from nowhere. The reply's checksum is then validated under the v3
pseudo-header.

**`a_priority_zero_advertisement_raises_the_master_resigned_event`** — the event split. A
priority-0 advertisement must raise `vrrp_master_resigned`, not `vrrp_advertisement_received`;
routing it to the wrong event would make an operator's handler for it never match, and a
resignation is precisely the moment a takeover succeeds.

**`no_advertisement_transmits_nothing_and_is_logged_as_a_decision`** — an explicit refusal
produces no packet **and** a `decision=model_reject` line. On the wire this is identical to
every other silence, so the tag is the only place the difference survives.

**`an_llm_failure_puts_nothing_on_the_wire`** — the one that matters most. The backend is
`http://127.0.0.1:1`, a closed port.

A bare "no packet arrived" would prove nothing: it is equally consistent with a server that
never received the advertisement, which is the shape of assertion the root `CLAUDE.md` warns
about under the empty-static-handler investigation. So this asserts a **pair**: the
`decision=fail_closed_` status line proves the packet was decoded, the event raised and the
model asked; the absent packet proves the failure produced no output.
`an_advertisement_produces_the_advertisement_the_model_decided_on` is the positive control for
the same path. Budget: 90 s for the decision line, because the LLM client retries before giving
up, then 5 s to confirm nothing arrived. The order matters — the no-packet check runs *after*
the decision is known to have been made.

**`a_carp_server_decodes_and_answers_in_carp`** — the `variant` parameter really switches both
directions. The inbound literal is a genuine `struct carp_header` (which a VRRP decoder would
misread as a master resigning), and the reply must be 36 octets of CARP with the configured
vhid and advbase, the action's advskew and counter, a valid message-only checksum, and an HMAC
that **differs** from the one an empty passphrase would produce — otherwise `carp_passphrase`
would be a parameter that does nothing.

**`execute_action_infers_the_variant_when_the_configuration_is_not_in_scope`** — this exists
because of a real bug found by the CARP test above. `execute_action` is called with nothing but
the action, so it cannot know a CARP server is asking; the first version validated against
`VrrpGroupConfig::default()` (variant VRRP) and rejected the model's `advskew` as "a CARP field
in a VRRP advertisement", making the whole CARP path unreachable. The fix infers the variant
from the action's own fields when it does not name one. The test pins both directions plus the
contradictory case (`priority` *and* `advskew`), which must still be refused.

**`the_declared_startup_parameters_are_exactly_the_ones_the_server_reads`** — a local echo of
`tests/startup_param_drift_test.rs`, pinning the set to the nine
`VrrpGroupConfig::from_startup_params` reads, plus a check that an undeclared key is refused by
name.

**`an_interval_the_configured_version_cannot_encode_refuses_to_start`** — the cross-field
check. `advert_interval: 60` is legal under VRRPv2 (a one-octet field of whole seconds) and
impossible under VRRPv3 (a 12-bit centisecond field topping out at 40.95 s), so a
per-parameter range check cannot catch it. The test asserts v3 refuses at `spawn()` naming the
unit and the ceiling, **and that the identical value starts fine under v2** — without that
second half it would be indistinguishable from a range check on one parameter. It also covers a
malformed virtual address being refused by name.

**`the_raw_transport_never_reports_success_without_a_raw_socket`** — the ARP/DataLink/ICMP
regression guard, done locally because the shared
`tests/capture_startup_reports_failure_test.rs` is not this protocol's file to edit. Opening
`SOCK_RAW` on IP protocol 112 unprivileged must return `Err` naming `CAP_NET_RAW`, the protocol
number, and the `udp` transport that needs none of it — not a server sitting in `Running`
having received nothing. Skipped when the host *does* have raw-socket access, where the open
legitimately succeeds and there is nothing to assert that would hold everywhere.

## Two pre-existing failures you will see, and why they are not yours

At `--no-default-features --features vrrp` these two fail, and they fail identically at the
already-landed `--features stp`:

* `event_action_declarations_test::the_client_audit_reports_its_own_coverage` — no *client* is
  registered in a server-only feature set, so the client audit inspects nothing.
* `executable_examples_test::the_example_audit_has_something_to_inspect` — it requires >900
  examples in scope, which needs a wide build.

Both are coverage self-checks about the *feature set*, not about the protocol. The ratchets
themselves (`no_registered_protocol_emits_an_event_without_actions`,
`no_advertised_example_is_rejected_by_its_own_executor`, `startup_param_drift_test`,
`event_emit_sites_test`, `wire_failure_test`) all pass.

## What these tests do NOT prove

* **The raw IP-protocol-112 transport has never been executed.** No test here runs privileged.
  The socket creation, the multicast join, the IPv4-header strip and the `sendto` are untested
  code.
* **No third-party VRRP or CARP peer has ever spoken to this server.** Not `keepalived`, not
  `frr`, not an OpenBSD `carp` interface. The codec is checked against the specification;
  nothing has checked it against another implementation's *reading* of the specification.
* **CARP's HMAC input construction** is a reading of OpenBSD source rather than of a
  specification, and nothing has accepted it.

That is why the protocol is `Experimental`, and the codec tests are not grounds to promote it.
See `src/server/vrrp/CLAUDE.md` for the `feth`-pair recipe that would give a real Ethernet
segment on this machine, with `keepalived` as the peer — **nobody has run it.**

## Adding a test here

Reuse the literals. If you need a new packet shape, write the bytes out by hand from the
specification with an offset table and the checksum arithmetic in a comment, the way the four
existing ones are — do **not** generate it with `encode()` and paste the result, which
reintroduces exactly the circularity this file exists to avoid.
