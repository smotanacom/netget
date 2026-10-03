# BGP Server Tests

Five files, several different jobs. Derive the counts with `cargo test`, not from this table —
they drift.

| File | Kind | Needs a NetGet process |
|---|---|---|
| `test.rs` | Wire-format conformance. Pure functions, no socket, no model. | no |
| `e2e_test.rs` | Full socket path against a mocked model. | yes |
| `static_default_test.rs` | The handshake completes with **zero** LLM calls when no operator policy is configured. | yes |
| `peer_inject_test.rs` | `AppState::send_to_client` injection into a live session. | in-process |
| `llm_failure_test.rs` | The backend errors while a peering policy is configured: the peer must get NOTIFICATION Cease, not the configured OPEN. | yes |

All are declared in `tests/server/bgp/mod.rs`, which `tests/server/mod.rs` gates under
`#[cfg(feature = "bgp")]`.

```bash
./cargo-isolated.sh test --no-default-features --features bgp --test server -- --test-threads=100 bgp
```

## What these tests are actually for

BGP's failure mode is not a crash. It is emitting bytes that look right, that the implementation
happily parses back, and that every real router silently drops. OSPF in this repo did exactly
that for years with a wrong checksum while its docs claimed router interoperability.

**No BGP daemon (`bgpd`, `bird`, `bird2`, `frr`, `gobgp`) is installed on the development
machine.** Nothing here has been peered against a real implementation. Read every claim below
with that caveat.

Since NetGet encodes with `netgauze-bgp-pkt`, a test that encodes with netgauze and decodes with
netgauze establishes nothing about RFC conformance — only that netgauze agrees with itself. So
`test.rs` uses two layers and is explicit about which one carries the weight:

1. **RFC-derived literal bytes (the real oracle).** Every expected vector is written octet by
   octet from RFC 4271 sections 4.1-4.5 and RFC 6793 section 4, with the field decode in a
   comment on each line. These come from neither implementation. If NetGet and netgauze were
   wrong in the same way, these would still fail.
2. **Inbound parsing of hand-written messages (a genuine cross-check).** The same hand-derived
   vectors go into the receive path and the decoded fields are asserted. The input is not
   NetGet's output, so this direction really does compare two independent readings of the spec.

When you add a case, write the expected bytes from the RFC. Do **not** capture what the code
currently produces — that turns the suite into a change detector and loses the only property
that makes it worth having.

## `test.rs` — conformance

Reaches the implementation through two public seams:

- `BgpProtocol::execute_action(...)` → `ActionResult::Custom { name: "bgp_message", data }`
- `netget::server::bgp::wire::{encode_intent, decode, parse_header, update_to_json}`

That is the same pair the session uses, so the tests exercise the production path rather than a
parallel one. `wire.rs` is `pub` for this reason.

Covered:

- **Header framing** — marker, the `[19, 4096]` bound, and the per-type minimums. Length 18 is
  tested specifically because `len - 19` underflows without it; 4097 because the length field is
  otherwise an attacker-chosen allocation.
- **OPEN** — byte-exact, with the four-octet AS capability; and the AS_TRANS form for an ASN
  above 65535, which additionally asserts the old truncated value (AS 60416 for 4200000000) does
  not appear anywhere in the output.
- **KEEPALIVE, NOTIFICATION** — byte-exact. The NOTIFICATION case asserts `"00b4"` reaches the
  wire as two octets and that the four ASCII characters do **not**, because the parameter is
  documented as hex and the executor has to actually decode it.
- **UPDATE** — byte-exact in both AS_PATH widths, plus the AS4_PATH form, withdrawal-only (no
  path attributes at all), prefix length in bits with host bits masked, `/0`, an empty AS_PATH
  as a zero-length attribute rather than an absent one, and refusal above 4096 octets.
- **Inbound** — OPEN with and without capabilities, UPDATE in both widths, withdrawal,
  End-of-RIB, and the `(code, subcode)` each malformed input earns: 2/1 bad version, 2/6 bad
  hold time, 2/3 bad BGP identifier, 1/1 bad marker.
- **Action validation** — every rejection asserts the *message* names the offending field, so a
  handler gets a usable error instead of silence.
- **Round-trip** — everything NetGet emits reparses, which catches a length field that disagrees
  with the body it precedes.

## `e2e_test.rs` — session

Six tests, **16 mocked LLM calls total** (6 startups, 5 `bgp_open`, 4 `bgp_established`, 1
`bgp_update`; two tests deliberately expect zero or one event call). Over the ~10 guideline because
each test needs its own server to isolate a distinct session outcome, and reusing one would mean
several peers racing on shared mock counters.

| Test | Asserts |
|---|---|
| `session_establishes_and_delivers_routes` | OPEN → KEEPALIVE → Established → UPDATE, all four byte-exact. The point of the protocol. |
| `two_octet_peer_gets_two_octet_as_path` | Same action, peer without the capability, byte-exact two-octet AS_PATH. |
| `invalid_open_is_refused_without_consulting_the_model` | Version 3 earns NOTIFICATION 2/1 and the `bgp_open` mock is `expect_at_most(0)`. |
| `model_can_refuse_to_peer` | `send_bgp_notification` yields NOTIFICATION 6/5 and no OPEN. |
| `keepalive_cadence_and_hold_timer_expiry` | With hold 3, unsolicited keepalives arrive, then NOTIFICATION 4/0. |
| `peer_update_is_decoded_into_structured_event` | Mock matches on `nlri`, `next_hop` and `as_path`, so a regression to a hex blob fails it. |

`expect_at_most(0)` is how "the model must not be asked" is expressed; there is no
`expect_never`. `and_event_data_contains` is how event-shape regressions are caught — the mock
simply never fires and `verify_mocks` reports it.

Every test ends with `server.verify_mocks().await?`. Without it the test asserts nothing about
the model interaction.

### Timing

`LLM_TIMEOUT` is 60s, not the 120s the old suite used, because the mock answers immediately and a
two-minute timeout only makes a hang expensive to discover. The hold-timer test negotiates a
3-second hold (the RFC minimum) so expiry is observable in seconds rather than three minutes.

## History

This directory previously held **eight tests that were four tests duplicated** — `test.rs` and
`e2e_test.rs` declared the same four function names with near-identical bodies. Three of the four
could not fail: `test_bgp_notification_on_error`, `test_bgp_keepalive_exchange` and
`test_bgp_graceful_shutdown` each accepted every outcome, including a timeout, and printed a ✓
either way. `test.rs` was rebuilt as the conformance suite rather than deleted, and the four E2E
tests were rewritten with assertions that can fail.

The suite was mutation-checked afterwards. Dropping the post-OPEN KEEPALIVE, disabling
hold-timer expiry and forcing four-octet AS_PATH regardless of negotiation each made the
corresponding tests fail; a fourth mutation, removing the host-bit mask in
`wire::ipv4_unicast`, did **not** fail anything, because netgauze masks on write as well. That
mask is therefore defence in depth, not the thing the test is pinning.

## Not covered

Interoperability with a live daemon. Multi-peer anything (there is no RIB, so there is nothing to
propagate). IPv6 and MP-BGP on the send path — inbound MP_REACH/MP_UNREACH parse but are reported
by name only. Route refresh beyond "does not tear the session down". Graceful restart, add-path,
extended messages.
