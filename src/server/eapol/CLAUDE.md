# EAPOL / 802.1X Server (IEEE 802.1X-2004, RFC 3748)

Port-based network access control, in the **authenticator** role — NetGet as the switch port.
The model makes the admission decision; NetGet owns the wire, the session machine and all of
the cryptography.

Files: `codec.rs` (pure codec, no I/O), `actions.rs` (LLM vocabulary + executor), `mod.rs`
(transports, session loop, and the fail-closed rule).

## The single most important property: it fails closed

**An `EAP-Success` frame is an admission decision.** It is the octet that opens a switch port.
So the OAuth2 post-mortem in the root `CLAUDE.md` applies here with more force than anywhere
else in the tree: there, "the LLM returned nothing" fell through to a hardcoded access token,
and a model's explicit denial became indistinguishable from its silence.

In `EapolServer::decide`:

| Situation | Wire result | Logged decision |
|---|---|---|
| Model returns `send_eap_success` on a session with an identity | EAP-Success | `decision=model_admit` |
| Model returns `send_eap_failure` | EAP-Failure | `decision=model_reject` |
| Model returns a Request (identity / method / notification) | that Request | `decision=model_challenge` |
| Model returns no usable action | **EAP-Failure** (synthesised) | `decision=fail_closed_no_action` |
| Model's action fails to encode | **EAP-Failure** (synthesised) | `decision=fail_closed_action_error` |
| LLM call errors or times out | **EAP-Failure** (synthesised) | `decision=fail_closed_llm_error`, plus `category=overloaded`/`unavailable` from `WireFailure::classify` |
| Model returns `send_eap_success` with **no identity on the session** | **EAP-Failure** (synthesised) | `decision=fail_closed_no_identity` |
| EAPOL-Logoff, no answer | **nothing** | `decision=fail_closed_no_action` |

### Why that is structural rather than aspirational

**There are three independent gates on a Success, and all three must pass.**

1. **No request context, no frame.** The registry's `EapolProtocol` is context-free and cannot
   encode anything — every EAP frame must echo an identifier only a received packet supplies.
   `mod.rs` builds one `EapolProtocol::for_request(...)` per received frame. A context-free
   instance says so, and the word "context" in that message is load-bearing:
   `tests/executable_examples_test.rs` keys its "needs a live request" exemption on it.
2. **No identity, no admission.** `RequestContext::identity_established` is true only when the
   session already holds an `EAP-Response/Identity`. `execute_action`'s `send_eap_success` arm
   refuses otherwise, naming the reason. An identity is a *claim*, not evidence — but a port
   admitted with no identity attached to it at all is worse than either.
3. **`decide` re-checks the same thing.** If a Success arrives classified from the executor on
   a session with no identity, it is replaced with a denial and logged
   `decision=fail_closed_no_identity`. Gate 2 makes that unreachable today, which is the
   point: if it ever fires, an authentication bypass is being *reported* instead of served.

**Success and failure share no code path anywhere.**
`codec::eapol_eap_success_frame` and `codec::eapol_eap_failure_frame` are eight literal octets
each, written out separately. Neither takes a code, a boolean, or anything else that could
select the other, and there is deliberately **no** `encode_eap_result(code, id)` — that
function would be shorter and one wrong argument wide of an authentication bypass. The
codec test asserts the two frames differ in exactly one octet and that it is the code.

**Every fail-closed exit calls the failure builder directly**, never the action executor. No
model output, no error path, no timeout and no default can steer it.

**`grep -rn EAP_CODE_SUCCESS src/ --include='*.rs'` finds five code sites, and exactly one of
them writes the octet:**

| Site | What it does |
|---|---|
| `codec.rs` `pub const EAP_CODE_SUCCESS` | the constant |
| `codec.rs` `eapol_eap_success_frame` | **the only producer in NetGet** |
| `codec.rs` `EapPacket::decode` | recognises a four-octet terminal frame |
| `codec.rs` `eap_code_name` | names it for a log line |
| `mod.rs` `classify_frame` | reads an already-encoded frame |

Four of the five only ever *read*. If that grep returns a sixth site, or a second one that
constructs a frame, read it very carefully.

### The fail-closed answer is a frame, not silence

A supplicant that gets nothing retries and eventually times out, which on a real switch port
is indistinguishable from a broken cable. So every denial is written, with one exception:
**EAPOL-Logoff**, where the session is destroyed *before* the model is consulted. Ending
access is not a decision to delegate — a backend outage must not be able to hold a port open —
so by the time the model is asked there is nothing left to deny and a fabricated frame would
assert something untrue. `Prepared::silence_is_safe` marks exactly that one case. It is the
same line `radius` draws with `packet::is_authorization_request` for accounting.

## What the cryptography actually does

Claiming crypto that is not performed is a documented failure of this codebase's auth family.
This list is exhaustive in both directions.

**Implemented and exercised by tests:**

- **MD5-Challenge, both halves.** NetGet generates a 16-octet challenge from `rand`
  (`EapolServer::fresh_challenge`) and verifies the supplicant's digest itself
  (`codec::md5_response_matches`), constant-time. The digest is
  `MD5(Identifier || Secret || Challenge)` per RFC 1994 §2.2 — the order matters, and
  `Secret || Identifier || Challenge` is the classic slip that is self-consistent and rejects
  every real supplicant. Pinned against a Python-computed literal.
- **MD5 itself** comes from the **`md-5` crate**, the same one `src/server/radius/packet.rs`
  uses — one MD5 in the tree, one place for it to be wrong. `codec::md5` is a thin wrapper
  kept so the RFC 1321 §A.5 vectors and the 55/56/57/64-octet padding boundary can be
  exercised directly; those tests now assert that this file *drives the crate correctly*,
  which is still the thing that can break.

**Not implemented, and claimed nowhere:**

- **EAP-TLS, PEAP.** NetGet sends the RFC 5216 §3.2 Start flag and reports the flags on
  whatever fragment comes back. No handshake is carried, no certificate is validated.
- **MSCHAPv2.** Deliberately absent from `method_type_value`, which refuses it by name.
  NetGet cannot verify an MSCHAPv2 response (it needs the NT hash, and there is no MD4 here),
  and offering a method it cannot check would mean admitting on unverified evidence.
- **EAPOL-Key / MKA (802.1X-2010 MACsec).** Received frames are logged and dropped.
- **RADIUS pass-through.** See below — the EAP payload is not forwarded anywhere.
- **VLAN assignment / dynamic authorization.** Nothing beyond Success and Failure.

### Why `expected_password` is not "asking the model for cryptographic material"

The model supplies the password it *expects* an identity to hold, exactly as it supplies one
for RADIUS PAP. It is ordinary structured text that an operator instruction naturally carries
("admit alice with password hunter2"). It is never asked for a hash, a nonce or a key, and it
never sees the challenge. What it gets back is a verdict:

| `md5_verification` | Meaning |
|---|---|
| `verified` | the digest matches. The **only** value that is evidence of anything |
| `mismatch` | the supplicant does not hold that password |
| `not_checked_no_expected_password` | no password was supplied, so **nothing was verified** |
| `malformed` | the response could not be parsed |
| `unsolicited` | no challenge was outstanding for this session |

`md5_verified` is a boolean that is true for `verified` and false for every other case,
*including* the "we never checked" one. That asymmetry is the whole point: the absence of
information must never read as a positive assertion, which is precisely what OAuth2's
`{"active": true}` did.

`expected_password` is read out of the model's action JSON rather than the encoded frame,
because it is deliberately the one field an MD5 challenge does **not** transmit — the
challenge goes to the supplicant, the password stays here so NetGet can do the comparison.
`Answer::expected_password` carries it from `decide` to `record_outbound`. This was wrong in
the first draft — the password was taken from `Prepared`, which holds the *previous*
challenge's — and the symptom was `md5_verification: not_checked_no_expected_password` on a
correctly-answered challenge, i.e. a supplicant that could never be admitted. Fail-closed, so
it presented as a stuck exchange rather than a bypass, and the e2e test caught it.

## The EAP identifier

RFC 3748's identifier rules are the difference between an authenticator that works and one
that hangs, because **a supplicant discards a mismatched frame in silence**.

- A new `EAP-Request` uses `RequestContext::request_identifier`, allocated by the session.
- An `EAP-Success` or `EAP-Failure` uses `result_identifier`, which is the identifier of the
  Response being answered (§4.2).
- A `Response` whose identifier does not echo the outstanding Request's is **discarded** with
  a WARN (§4.1). An unsolicited Response — none outstanding — is accepted and logged at DEBUG.

**The model cannot set any of these.** No action takes an identifier parameter; they come from
the received frame. Two overlapping exchanges therefore cannot pick up each other's.

## Transports

| `transport` | What it is |
|---|---|
| `raw` (default) | libpcap capture + injection on EtherType 0x888E over the bound interface. BPF filter `ether proto 0x888e`. Frames from our own MAC are skipped, because injected frames come back through the capture handle |
| `udp` | The test transport. Each datagram is `[supplicant MAC (6 octets)][EAPOL frame]`, in **both** directions — the six octets are always the *supplicant's* address, which is the only piece of Ethernet framing the events carry |

Both funnel into `EapolServer::handle_eapol`. There is exactly one copy of the session machine
and one codec; the UDP transport is not a simulation of the protocol, it is the same protocol
over a different link.

### Privilege

`PrivilegeRequirement::PacketCapture` — libpcap capture/injection, never a `SOCK_RAW`.
`RawSockets` would refuse to start on a host with `/dev/bpf*` access but no root, which is the
"don't claim more than you need" rule that `ospf` got wrong by declaring `Root` when it wanted
`CAP_NET_RAW`. `arp`, `datalink`, `isis`, `lldp`, `stp` and `cdp` all declare the same variant.

**The gate is per protocol and `server_startup` evaluates it before reading startup
parameters**, so `open_server` is refused on an unprivileged host *even when
`transport: "udp"` is requested. Declaring `None` to dodge that is the tempting fix and the
wrong one: the transport anyone actually deploys is raw Ethernet and the metadata has to say
so. The tests therefore build a `SpawnContext` and call `Server::spawn` directly, as
`bluetooth_ble_beacon` and `lldp` do.

`spawn_raw` additionally probes `SystemCapabilities` itself and returns `Err` naming the
missing privilege, so a *direct* spawn refuses honestly too rather than sitting in `Running`
having captured nothing — the ARP/DataLink/ICMP/IS-IS defect that was fixed four separate
times elsewhere.

## Events and actions

Four events, all raised by `mod.rs`, all carrying `.with_actions(...)`:

| Event | Raised when | Actions offered |
|---|---|---|
| `eapol_start` | EAPOL-Start received | request identity / request method / notification / **failure** |
| `eapol_identity_response` | `EAP-Response/Identity` | success / failure / request method / notification |
| `eapol_method_response` | any other `EAP-Response` | success / failure / request method / notification |
| `eapol_logoff` | EAPOL-Logoff received | notification / failure |

**`eapol_start` deliberately does not offer `send_eap_success`.** A device that has not
claimed an identity is not a device you can admit, and the narrowest vocabulary that can
express the right answer is the safest one. Servers narrow (unlike clients, which union), so
this really is what the model is shown. Gates 2 and 3 above enforce the same thing even if it
somehow produced the action anyway.

EAPOL-Key and Encapsulated-ASF-Alert raise **no** event, because nothing could answer one.
Declaring an event that never fires — or one whose only answer would be a lie — is the defect
`tests/event_emit_sites_test.rs` exists to catch.

No action takes raw bytes or base64, and no event carries any. `type_data_length` is a count,
not a payload: a model cannot read a wire payload reliably and must not be asked to.

## Sessions and the connection rail

Sessions are keyed by supplicant MAC in a `std::sync::Mutex<HashMap<...>>`. The `std` mutex is
deliberate — its guard is `!Send`, so holding one across an `.await` in a spawned task is a
*compile error* rather than a review finding. `prepare` takes and releases it internally and
returns owned data.

`metadata()` deliberately does **not** set `connectionless()`. The 10-second idle sweep exists
for UDP/raw servers whose per-remote entries nothing ever closes; an EAPOL exchange parked on
a manual handler waiting for a human routinely outlives 10 seconds, and the sweep would draw
it `(closed)` while it was still live — the telnet bug the root `CLAUDE.md` records. Sessions
are ended explicitly instead, on Success, Failure or Logoff.

## Pairing with `radius` — the complete NAC lab

This protocol and `src/server/radius/` together are a full network-access-control bench:
**EAPOL on the wire, RADIUS behind it, the model deciding admission.** That is why EAPOL is
the highest-value item in its roadmap tier.

Today the two are **independent**: EAPOL terminates EAP locally and decides for itself, and
`radius` decodes `EAP-Message` (attribute 79) to opaque hex and tells the model
`auth_method: "eap"` precisely so it knows it has *not* been given a verified identity.
Nothing forwards between them.

Wiring them properly means implementing RFC 3579 pass-through, and the shape is:

1. The authenticator stops answering `eapol_identity_response` itself and instead relays the
   EAP payload to a RADIUS server as `Access-Request` + `EAP-Message` + `NAS-Port-Type`
   (15, Ethernet) + `Calling-Station-Id` (the supplicant MAC, which the event already carries
   in exactly the right format).
2. `Access-Challenge` + `EAP-Message` comes back and becomes the next `EAP-Request` — the
   `State` attribute round-trip `radius` already implements is what ties the continuation
   together, and `tests/server/radius/e2e_test.rs::access_challenge_state_round_trips`
   already proves that half works.
3. `Access-Accept` becomes `EAP-Success`, `Access-Reject` becomes `EAP-Failure`.

Two things to get right before starting, both of which this design is already shaped for:

- **The fail-closed rule has to survive the extra hop.** A RADIUS timeout must map to
  `EAP-Failure` and a distinct `decision=fail_closed_radius_timeout`, not to silence and not
  to the existing `fail_closed_llm_error`. The `Decision` enum is the place to add it.
- **RFC 3579 §3.2 Message-Authenticator is mandatory for EAP over RADIUS**, and `radius`
  explicitly does **not** implement it (its CLAUDE.md says so under "Not implemented, and
  claimed nowhere"). A real NAS or a real RADIUS server configured to require it will discard
  the exchange. That is the first thing to build, not the last.

## Maturity: `Experimental`, and precisely why

**Proven by tests:**

- The codec, byte for byte against literal IEEE 802.1X / RFC 3748 frames.
- MD5 and the RFC 1994 digest, against RFC 1321's own published suite and a Python literal.
- The whole event → LLM/handler → action → frame path, over the UDP transport.
- The fail-closed discipline, including that no supplicant input can produce a Success.

**Not proven by anything:**

- **The raw-Ethernet transport has never been executed.** No `pcap::Capture::open`, no BPF
  filter compile, no `sendpacket`, no capture loop. It needs privilege this environment does
  not have.
- **No third-party supplicant has ever spoken to this server.** Every frame it has answered
  was one this repository also wrote.
- The self-frame filter (skipping our own injected frame captured back off the wire) cannot
  happen on the UDP transport, so it is exercised only in principle.

A green test run here means "the bytes are right and the failure discipline is honest". It
does not mean 802.1X works.

### The path to Beta, which has NOT been run

This machine has the `feth` driver (`net.link.fake.txstart: 1`), so a real Ethernet pair
exists without hardware:

```bash
sudo ifconfig feth0 create && sudo ifconfig feth1 create
sudo ifconfig feth1 peer feth0
# NetGet as authenticator on one end:
netget --server eapol --interface feth0        # needs root for the pcap handle
# wpa_supplicant as the real supplicant on the other:
sudo wpa_supplicant -i feth1 -c eap-md5.conf -d
```

`wpa_supplicant` is the peer that would make a Beta claim real: it is an independent
implementation, it is what actually runs on every Linux client, and EAP-MD5 is the one method
here that it can complete end to end. **This has not been run** — it needs root, which this
environment does not have — so nothing above is a claim about it. Do not promote on the
strength of the plan.

Note also the bar the root `CLAUDE.md` sets and the mistake it records: `wireguard` was
demoted Stable→Beta for never having been validated against a real client, which is *also* the
definition of Beta, and nobody noticed for months. If this protocol is ever demoted, check
which rating the evidence actually supports rather than stepping down one notch by reflex.

## Why MD5 comes from a crate, recorded because the first version did not

The first version of `codec.rs` hand-rolled RFC 1321 MD5, because `md-5` was an optional
dependency gated on the `radius` feature and `eapol` did not pull it in. It was pinned to the
RFC 1321 §A.5 vectors and the padding boundary, and it passed.

**That was still the wrong call, and the reasoning is worth keeping.** Published vectors prove
the happy path, not the edge cases, and the next person to touch a hand-rolled digest will not
have the context that made it look safe. Hand-rolled cryptography is a liability even when it
is tested. The fix was a *feature edge*, not a new dependency — `md-5` was already in the tree
for `radius`, so the supply chain did not change at all:

```toml
eapol = ["pnet", "dep:pcap", "dep:md-5"]
```

`codec::md5` is now a thin wrapper over `md5::Md5`, and `md5_challenge_digest` feeds the
hasher in three ordered `update` calls the way `radius`'s authenticators do — the
concatenation *is* the specification, so writing it as ordered updates makes the order the
thing a reader checks. The vector tests were deliberately **kept**: they no longer test an
implementation, they test that this file drives the crate correctly.

The general rule: when a protocol needs a primitive another protocol already depends on, add
the feature edge rather than the implementation.

## Startup parameters

| Name | Values | Read in |
|---|---|---|
| `transport` | `"raw"` (default) / `"udp"` | `spawn_with_llm_actions` |
| `eapol_version` | 1, 2 (default), 3 | `spawn_with_llm_actions` |

Both are read; neither is advertised and inert. An unknown transport or an out-of-range
version **refuses the start** rather than falling back to a default, and the error names the
value and the acceptable ones. Errors propagate with `?` and are never unwrapped — a panic
here over MCP would kill the per-request task before it could reply and hang the caller.

Without a forced `eapol_version`, replies echo whatever version the supplicant used, which is
what a real authenticator does.

## Supplicant role

Not implemented. `codec.rs` has everything it would need — `eapol_start_frame`,
`eapol_logoff_frame`, `eap_response`, `eap_response_identity` all exist and are tested, and
the tests use them to play a supplicant — but there is no `src/client/eapol/`. It did not fall
out cheaply enough to be worth half-doing: a supplicant is a different state machine with
different failure semantics (there, *accepting* a Success too readily is the dangerous
direction), and shipping a half-written one would be worse than shipping none.
