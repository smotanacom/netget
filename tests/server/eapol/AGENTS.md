# EAPOL / 802.1X Tests

## Strategy: split by what is actually knowable

EAPOL's real transport is raw Ethernet and needs `/dev/bpf*` or `CAP_NET_RAW`. Nothing in this
repository has that, and the privilege gate in `server_startup` is **per protocol, evaluated
before startup parameters are read** — so an `open_server` for EAPOL is refused on an
unprivileged host *even when `transport: "udp"` is requested*, and the usual child-process
harness cannot start this server at all. The suite is split the way
`tests/server/bluetooth_ble_beacon/` and `tests/server/lldp/` are:

| File | Runs everywhere | What it proves |
|---|---|---|
| `codec_test.rs` | yes | The frame format against literal IEEE 802.1X / RFC 3748 bytes, and MD5 against RFC 1321's own suite |
| `e2e_test.rs` | yes | The whole event → handler/LLM → action → frame path, over the UDP test transport, in-process |
| — | — | **Nothing here proves anything about pcap, and no third-party supplicant has ever spoken to this server.** See `src/server/eapol/CLAUDE.md`. |

**44 tests**: 33 codec, 11 end to end. About 14s at `--test-threads=100`, almost all of which
is the two tests that talk to a mock model.

```bash
./cargo-isolated.sh test --no-default-features --features eapol \
    --test server -- server::eapol --test-threads=100
```

## LLM budget: 5 calls, in 2 tests

| Test | Calls | Why |
|---|---|---|
| `the_full_exchange_admits_a_verified_supplicant` | 3 | Start, identity, method — the model has to be asked at each step for the exchange to be real |
| `silence_and_denial_both_deny_but_are_distinguishable` | 2 | The two halves of the OAuth2 regression, one per supplicant |
| everything else | **0** | Either a static handler (no call by construction) or `UNREACHABLE_LLM` |

`UNREACHABLE_LLM` is `http://127.0.0.1:1`, where nothing listens. A test using it proves the
outcome happened *without a successful model call* — not merely that none was counted. That is
strictly stronger than an expectation of zero and costs nothing, and it is the trick
`bluetooth_ble_beacon` and `lldp` use.

## `codec_test.rs` — literal bytes, not round-trips

Every expected byte string is written out literally with a field-by-field derivation in the
comment above it, and **both directions are asserted against that literal**: `decode(LITERAL)`
must yield the fields, `encode(fields)` must yield the literal. Neither is allowed to define
the other. Round-tripping an encoder through its own decoder proves only that one function
inverts the other — the circularity the root `CLAUDE.md` names, and the reason `rss` sat at
Experimental for months.

Coverage worth keeping if these are ever rewritten:

- **`success_and_failure_differ_only_in_the_code_octet`.** The property a reader most wants
  confirmed: the two eight-octet frames are identical but for byte 4, so nothing else about a
  frame encodes the admission decision and nothing else can leak it. This is the test that
  makes "success and failure share no code path" checkable rather than a comment.
- **EAP `length` covers the whole packet, header included.** The field everyone gets wrong
  first. Asserted on every frame, and `rejects_an_eap_length_below_the_header` pins the
  refusal.
- **A Success or Failure is *exactly* four octets** (RFC 3748 §4.2). A five-octet "Success"
  with a trailing byte is what a sloppy parser accepts, and accepting it means accepting a
  frame nobody specified.
- **Ethernet padding must be ignored, not rejected.** An EAPOL-Start is 4 octets and every
  frame carrying one is padded to 60, so a strict `declared == remaining` check would reject
  every real frame on the wire. `decoding_ignores_ethernet_padding` is the regression.
- **The RFC 1994 digest order**, `MD5(Identifier || Secret || Challenge)`. Pinned against a
  literal computed with Python's `hashlib`. `Secret || Identifier || Challenge` is the classic
  slip: self-consistent, and rejects every real supplicant.
- **`md5_verification_accepts_only_the_right_password`** checks five separate ways to fail —
  wrong password, wrong identifier (so a replay under another identifier fails), wrong
  challenge, empty response, truncated response. The truncation case matters because a
  comparison that stopped at the shorter length would accept a one-octet "digest".
- **`md5_matches_the_rfc_1321_test_suite`** — seven digests published by the IETF, written by
  neither this file nor `codec.rs`. MD5 now comes from the `md-5` crate, so this is no longer
  an oracle for an implementation of ours; **keep it anyway**, because it is an oracle for our
  *use* of one. It would catch a wrapper that fed the hasher the wrong buffer, dropped an
  update, or returned the digest with the wrong endianness — all of which are live mistakes
  and none of which the crate can prevent. The padding-boundary cases (55/56/57/64 octets)
  are retained for the same reason.

## `e2e_test.rs` — in-process, over the UDP transport

Builds a real `SpawnContext` and calls `Server::spawn` directly. `state.set_ollama_model(...)`
is set before spawning: without it `ensure_model_selected` tries to auto-select against
`localhost:11434` and the test would depend on the developer's machine.

The supplicant is a plain UDP socket speaking the declared test framing —
`[supplicant MAC (6 octets)][EAPOL frame]` in both directions — and it asserts every reply is
addressed back to it. The frames themselves are built and decoded with the same `codec.rs` a
real supplicant would face.

### `assert_not_a_success` is the point of the file

Most tests here could be written as "the reply is an EAP-Failure". That is the same statement
made twice and would pass on a server that answered nothing at all. `assert_not_a_success`
instead says the **Success code is absent** from whatever came back, whatever shape it took,
and it is called on every reply in every negative test. The `EAP-Failure` assertion is made
*separately*, so "denied" and "did not admit" are two independent claims.

### The tests, and why each exists

| Test | The point |
|---|---|
| `the_full_exchange_admits_a_verified_supplicant` | The only full-length exchange. Start → Request/Identity → Response/Identity → MD5-Challenge → verified → Success, every step asserted on the wire, including that the challenge is 16 octets and **not** all-zero (generated, not defaulted) and that the Success echoes the Response's identifier |
| `silence_and_denial_both_deny_but_are_distinguishable` | **The OAuth2 regression.** Both halves against the same server and the same routing table, told apart only by supplicant MAC |
| `an_llm_outage_denies_and_never_admits` | A backend that cannot answer must deny — not admit, and not go silent and leave the supplicant to time out |
| `nothing_a_supplicant_sends_can_produce_a_success` | Ten hostile or malformed inputs, including a forged EAP-Success, with the model unreachable |
| `a_success_is_refused_until_an_identity_exists` | The identity gate, as a gate rather than a blanket refusal |
| `a_response_with_the_wrong_identifier_is_discarded` | RFC 3748 §4.1, against a routing table that admits everything |
| `logoff_deauthorizes_before_asking_and_then_may_stay_silent` | The one event where silence is the correct fail-closed answer, and the proof that it is safe |
| `the_raw_transport_refuses_rather_than_pretending` | The ARP/DataLink/ICMP/IS-IS defect — a server in `Running` having captured nothing. Fixed four separate times elsewhere |
| `unusable_startup_parameters_are_refused` | An unknown transport and an out-of-range version each refuse the start; the error names the value and the acceptable ones |
| `an_undeclared_parameter_names_the_declared_ones` | The error lists what IS accepted, so a model can correct itself |
| `a_registry_instance_cannot_encode_any_frame` | Gate 1, plus that an unknown action reads as *unknown* rather than as a missing context |

### Three that are worth understanding before editing

**`silence_and_denial_both_deny_but_are_distinguishable`** runs both halves against **one**
server with **one** routing table, distinguished by `and_event_data_contains("source_mac", …)`.
Two separate runs that happened to agree would not prove they are distinguishable; one run
that produces both, and reads the two decision lines back out of the status stream by MAC,
does. Both frames on the wire are asserted byte-for-byte equal to
`eapol_eap_failure_frame(2, id)` — EAPOL has one way to say no, and that is correct. The
distinction is asserted only in the log, which is where it belongs.

**`a_success_is_refused_until_an_identity_exists`** uses a single `*` → `send_eap_success`
static rule and asserts it produces a **Failure** on `eapol_start` and a **Success** on
`eapol_identity_response`. Same action, same rule, two outcomes: the difference is session
state. Written as two separate servers it would prove much less.

**`nothing_a_supplicant_sends_can_produce_a_success`** splits its inputs into *answered* and
*ignored* rather than asserting one blanket outcome, because the two are different claims. A
forged EAP-Success from the supplicant, a forged Failure, an EAP-Request, an EAPOL-Key, a
truncated frame and an invalid version must all be **dropped in silence** — an authenticator
does not answer frames only an authenticator may send. An EAPOL-Start and an
`EAP-Response/Identity` must be **answered with a denial**. An unsolicited MD5 response is
answered (it *is* a Response, and the session exists) and denied, because nothing was verified.

### Timing

No fixed sleeps anywhere. `wait_for_status` polls the status stream against a deadline; every
socket read is a `tokio::time::timeout`. "Nothing was sent" assertions use a short (2s) read
timeout and are made after the frame that *was* expected has already arrived, so they are not
racing something still in flight. `drain_status` is only ever called after the decision under
test has been observed.

## What still has no coverage

- No frame this code produced has reached a real supplicant. `wpa_supplicant` over a `feth`
  pair is the experiment that would change that; `src/server/eapol/CLAUDE.md` records the exact
  commands, and **it has not been run** — it needs root.
- `pcap::Capture::open`, the `ether proto 0x888e` filter compile, `sendpacket` and the capture
  loop have never executed. Their error paths are reached only through the "no such
  device"/"no capture privilege" case, which fails *before* the privileged step.
- The self-frame filter (ignoring our own injected frame captured back off the wire) cannot
  happen on the UDP transport, so it is exercised only in principle.
- EAP-TLS and PEAP are sent as a bare Start flag and the response flags are reported; no
  handshake is carried, so there is nothing further to test and nothing is claimed.

A green run here means "the bytes are right and the failure discipline is honest". It does not
mean 802.1X works.
