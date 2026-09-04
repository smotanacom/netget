# GTP E2E Tests

## Strategy

**The transport really executes.** Both GTP ports are above 1023, so nothing needs privilege
and nothing has to be simulated: `e2e_test.rs` starts a real netget process, binds a real UDP
socket on 127.0.0.1, sends real GTP datagrams into both planes and decodes what comes back.
The only mocked component is the model.

Two files:

1. **`codec_test.rs`** — the wire format against **literal, hand-written octets** taken from
   TS 29.060 §6 / §7.7 and TS 29.274 §5.1 / §8. Zero LLM calls, no server, no sockets.
2. **`e2e_test.rs`** — a **hand-written GTP peer** driving the running server. Its builders and
   parsers (`v1_message`, `parse_v1`, `v2_message`, `parse_v2`, its own `tbcd`, `apn_labels`
   and its own fixed-IE-length table) share no code with `src/server/gtp/codec.rs`.

### Why the assertions are not round trips

Encoding with our encoder and decoding with our decoder proves only that one implementation
agrees with itself — the circular-evidence trap the root `CLAUDE.md` names, and the reason
`rss` sat at Experimental for months. Every codec assertion here is against a byte vector
written out by hand, so a change of interpretation fails the test rather than passing quietly
because both sides moved together.

The e2e peer is held to the same standard: `parse_v1` re-derives the E/S/PN rule and its own
fixed-length table rather than calling into the server's codec.

### And why that still only buys Experimental

A hand-written peer is an independent **reading** of the specification, not an independent
**implementation**. That is exactly the `dhcp` and `usb/serial` situation the root `CLAUDE.md`
records, and it is why GTP is rated `Experimental` despite a fully exercised transport. See
"Maturity" in `src/server/gtp/CLAUDE.md` for what would change that (`sgsnemu` from Osmocom's
libgtp, or an open5gs node) and what was looked for on this machine.

## The UDP rule

The root `CLAUDE.md` requires UDP-style protocols to echo the client's transaction identifier
**dynamically**, via `respond_with_actions_from_event`, because a static mock with a hardcoded
identifier causes client timeouts and the usual "fix" is to weaken the assertion until it
passes.

**Every event rule in `e2e_test.rs` uses `respond_with_actions_from_event` and takes
`sequence` straight from the event.** The assertions then check the sequence came back:
`0x1234` on the Echo Response, `1` and `3` on the GTPv1 session messages, and the 24-bit
`0x0A0B0C` / `0x0A0B0D` on the GTPv2 ones.

Note this server would survive a hardcoded mock — `sequence` is an *optional* action parameter
and `mod.rs` echoes the request's own when the model omits it, which is what makes static and
script handlers usable. Driving it explicitly is what makes the echo *asserted* rather than
merely working.

## How the tests prove the request reached the model

The strongest assertions here are indirect on purpose:

- **Subscriber fields** — the GTPv1 session rule matches on
  `and_event_data_contains("imsi", …)`, `("msisdn", …)` and `("apn", "internet")`. If TBCD
  decoding, the MSISDN's leading numbering-plan octet or the labelled APN failed to decode, the
  rule does not match, the request falls through to the fail-closed refusal, and the cause
  assertion fails loudly. A literal mock would pass against a broken decoder.
- **The inner IP header** — the G-PDU rule builds its reply *out of* the decoded fields:
  `format!("reply to {source} proto {protocol_name} dport {destination_port}")`. The test then
  asserts the exact string `"reply to 10.45.0.2 proto UDP dport 53"` arrives inside the
  returning G-PDU. That single assertion covers the IPv4 header parse, the port extraction, the
  protocol-name table, **and** the `"encoding": "hex"` path — the mock hex-encodes the string,
  so if the executor put the hex text on the wire literally (the `send_tcp_data` bug) the bytes
  would not match.
- **GTPv2 RAT type** — the accept rule additionally matches `("rat_type", "EUTRAN")`, so the
  numeric-to-name mapping is asserted the same way.

## Mock expectations and the LLM call budget

| Test | Startup | Events | Total |
|---|---|---|---|
| `test_gtpv1_session_lifecycle_over_real_udp` | 1 | 4 | **5** |
| `test_gtpv2_create_session_accepted_and_refused` | 1 | 2 | **3** |
| `test_unsupported_version_is_answered_without_consulting_the_model` | 1 | 0 | **1** |
| `codec_test::*` (8 tests) | 0 | 0 | **0** |

**Total: 9**, under the ~10 target. The third test's zero is the point of it: TS 29.060
§11.1.1 makes Version Not Supported mechanical, so it must not cost a model call — the test
asserts the mock saw exactly one request, the startup one.

Rules are first-match-wins, so the two `gtp_create_session_request` rules in the GTPv2 test are
distinguished by APN (`internet` vs `forbidden`). Both tests end with `wait_for_mocks(30)` and
then `verify_mocks().await?`.

## What is covered

**Codec (literal octets):**

- GTPv1 header bit layout: `0x30` for a bare G-PDU, `0x32` with a sequence number, `0x20` for
  GTP' (PT=0), and the Length field counting the whole four-octet optional block
- **The E/S/PN all-or-nothing rule**, in both directions: PN alone still emits four octets; and
  a decode where a junk octet sits in the N-PDU position, asserting the body starts after four
  octets rather than two
- Extension headers: the 4-octet-unit length, the chained next-type octet, and the round trip
- Decode rejections: short datagram, GTPv0, a Length field larger than the datagram
- **The fixed (<128) vs TLV (128+) IE split**, on a hand-written Create PDP Context Request
  body; an unknown fixed type stopping the parse; a truncated TLV; the length table itself
- GTPv2 header: no TEID when T is clear, TEID between length and sequence when set, the 24-bit
  sequence, the spare octet, TLIV IEs, the instance nibble, grouped IEs
- TBCD (odd and even digit counts), labelled APNs, End User Address, PAA, F-TEID, PCO DNS
  containers
- Inner IPv4/UDP header decoded into fields, and a non-IP payload rejected
- The cause table: the acceptance boundaries in both versions, no duplicate codes, the
  fail-closed causes asserted to be refusals in both versions, and the spellings a model
  produces (`"Request accepted"`, `"MISSING-OR-UNKNOWN-APN"`)

**GTPv1 over real UDP:** Echo Request → Echo Response with the request's sequence and the
model's Recovery counter; Create PDP Context Request → accepted response whose header TEID is
the peer's IE 17 (not the request header's zero), with Cause 128, the mandatory Reordering
Required IE, both TEIDs, the Charging ID, the End User Address, both PCO DNS containers, two
GSN Addresses, **and ascending IE order**; a G-PDU on the user plane → a G-PDU back on the
TEID the peer advertised; Delete PDP Context Request → accepted, honouring the model's explicit
`teid` override.

**GTPv2-C over real UDP:** Create Session Request → Create Session Response with the 24-bit
sequence intact, the header TEID taken from the peer's Sender F-TEID, a two-octet Cause 16, the
PAA, the PCO, the PGW control F-TEID at instance 1 with interface type 7, and a Bearer Context
carrying the EBI from the request and a user-plane F-TEID with interface type 5. Then the same
request with a different APN → Cause 77, **and no address and no F-TEID**, which is the
assertion that a refusal hands out nothing.

**Decision logging:** both tests assert `decision=model_accept` / `decision=model_reject`
appears and that `decision=fail_closed` does **not**. That is what keeps the fail-closed path
from silently becoming the normal path — a regression that made every request fall through
would still produce plausible-looking refusals on the wire.

**Startup parameters:** the GTPv2 test starts with `enable_user_plane: false` and asserts the
server says so, and the version test passes `user_plane_port: 0`. Both parameters are
therefore exercised rather than merely declared.

## Coverage gaps

- **No third-party peer.** The single most important gap, and the reason for the maturity
  rating. `sgsnemu` (Osmocom libgtp) and open5gs would each be a real one; neither is installed
  here and nothing was installed for these tests.
- **Wireshark is not wired into the suite** — deliberately. `tshark`'s `gtp`/`gtpv2`
  dissectors *were* run by hand against four of this server's packets and accepted all of them
  with no expert warnings (the table is in `src/server/gtp/CLAUDE.md`), but automating it would
  mean a test that skips when `tshark` is absent, and the root `CLAUDE.md` is explicit that a
  real tool behind a skip-when-missing gate is a silent pass rather than evidence.
- **No fail-closed test.** Nothing here forces an LLM error or an unusable action, so the
  synthesised refusal and its `fail_closed_*` tokens are asserted only *negatively* (they must
  not appear). A positive test would need a mock that errors on a specific event.
- **`send_gtp_error_indication` and `no_response` are never exercised as model choices.** Both
  are executable (the ratchets check that) but no test drives them over the wire.
- **`gtp_update_context_request` has no e2e test.** Update PDP Context and Modify Bearer are
  implemented and their response encoder is shared with Create, but nothing sends one.
- **IPv6 is not tested** anywhere, in either the codec or the e2e path.
- **No extension header travels over the wire** — they are covered only in `codec_test.rs`.
- **The GTP-U port discovery reads a log line.** `user_plane_port()` parses
  `"GTP-U user plane bound to …"`. That line is deliberately worded to avoid the phrase
  "listening on", which the harness scans for when resolving a port-0 server's real port — a
  second match there would hand the harness the user-plane port for the control-plane server.
  If that log line is reworded, this helper and that constraint both need revisiting.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features gtp \
    --test server -- --test-threads=100 gtp
```

About one second; everything is loopback UDP and an in-process mock. 13 tests.

The whole-tree ratchets need a client protocol compiled in to pass their own coverage
assertions, so run them with a companion feature:

```bash
./cargo-isolated.sh test --no-default-features --features gtp,tcp \
    --test event_action_declarations_test --test event_emit_sites_test \
    --test startup_param_drift_test --test wire_failure_test -- --test-threads=100
```

`executable_examples_test`'s *audit* passes at this feature set, but its
`the_example_audit_has_something_to_inspect` guard demands >900 examples and therefore only
passes at `--all-features`. That is a property of the guard, not of GTP.

## Failure modes seen so far

One, and it was the harness rather than the protocol: the user-plane startup line originally
said "GTP-U server listening on …", which the e2e harness matched as a second port
confirmation. It is now worded differently on purpose — see the last coverage-gap bullet.
