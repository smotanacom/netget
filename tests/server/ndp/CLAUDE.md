# NDP Tests

## Strategy: split by what is actually knowable

NDP's real transport is a raw ICMPv6 socket and needs root or `CAP_NET_RAW`. Nothing in this
repository has that, and `server_startup` refuses to spawn a protocol whose declared privilege is
unmet — so on any developer machine and any CI runner, the raw path cannot be executed at all. The
suite is therefore split the way `tests/server/lldp/` and `tests/server/bluetooth_ble_beacon/`
are:

| File | Runs everywhere | What it proves |
|---|---|---|
| `codec_test.rs` | yes | The packet format, against literal RFC 4861 / 4443 / 8200 / 8106 / 4291 bytes |
| `e2e_test.rs` | yes | The whole event → handler/LLM → action → message path, over the UDP test transport |
| — | — | **Nothing here proves anything about the raw ICMPv6 socket.** See `src/server/ndp/CLAUDE.md`. |

**LLM budget: 1 call.** One test uses a mock Ollama; every other test either uses a static handler
(no call by construction) or points the client at `127.0.0.1:1` so that the outcome *proves* no
call succeeded.

```bash
./cargo-isolated.sh test --no-default-features --features ndp \
    --test server ndp -- --test-threads=100
```

49 tests, ~1.1s.

## `codec_test.rs` — literal bytes, not round-trips

Every expected byte string is written out literally and derived from the published layout, **not
from the implementation**. Round-tripping the encoder through the decoder would prove only that
one function inverts the other, which the root `CLAUDE.md` names as circular evidence; it appears
here exactly once, as the last test in the file, explicitly labelled as a consistency check and
not as the argument.

Sources are named in the file header: RFC 4861 §4.1–4.6 and §11.2, RFC 4443 §2.1/§2.3, RFC 8200
§8.1, RFC 8106 §5.1, RFC 4291 §2.7.1.

### The checksums, and why they are trustworthy here

Each expected checksum was produced by an **independent** one's-complement implementation written
directly from RFC 8200 §8.1 — the pseudo-header laid out by hand, next header 58, upper-layer
length as a 32-bit field — and embedded as a literal. That is the same class of evidence
`CLAUDE.md` accepts for BGP (netgauze) and Kafka (kafka-protocol): an independent reading of the
spec rather than an independent implementation, since no third-party crate here speaks NDP.

Because "independent implementation, same author" is a weaker claim than it sounds,
`the_checksum_is_reproducible_by_hand` **works the smallest case out arithmetically in its doc
comment** — five addends and a fold, ending at `0x7bb8` — so a reader can confirm the whole scheme
without running or trusting anything. The two terms that appear in that arithmetic are exactly the
two classic mistakes: `0x003a` is why the next header must be 58, and `0xff04` (the destination
address) is why the checksum cannot be computed from the ICMPv6 bytes alone.

### Coverage worth keeping if these are ever rewritten

- **The 8-octet length unit, for every option type.** `option_length_is_in_units_of_eight_octets`
  asserts the octet count *and* the length field separately, so an implementation that wrote the
  octet count fails rather than passing by luck. RDNSS is the variable case (1 + 2n units) and is
  checked at one, two and three servers.
- **The pseudo-header, three ways**: reproducible by hand; the same octets between different
  address pairs giving different checksums; and a checksum computed over the message alone being a
  different number from the right one.
- **Neighbour Advertisement flags are the top three bits of a 32-bit field**, not three octets and
  not the low bits. Each of R, S and O is asserted alone, so a swapped pair cannot hide behind the
  combined `0x60000000` case.
- **Redirect's two addresses, in order.** Target (the better first hop) comes before destination.
  Swapping them produces a message that decodes cleanly and reroutes the wrong thing, so the order
  is pinned against literal octets rather than by round-trip.
- **The Prefix Information flag octet** — `0xc0` for L|A, `0x80` on-link only, `0x00` for neither —
  and the four reserved octets before the prefix, which are the easy ones to leave out.
- **The MTU option's two reserved octets**, for the same reason: omitting them shifts the MTU.
- **A Router Advertisement that withdraws itself.** `router_lifetime: 0` with M|O set and an
  infinite RDNSS lifetime — a combination that looks like an empty message and is not.
- **Solicited-node multicast takes the low 24 bits** (RFC 4291 §2.7.1), checked against four
  targets including one where the third-from-last octet is non-zero.
- **What decoding must refuse**: a non-zero ICMPv6 code, an option with length zero (RFC 4861 §4.6
  requires this — a zero length is an infinite loop in a naive walker), truncated messages,
  options claiming more octets than remain, and every ICMPv6 type that is not 133–137 (a raw
  socket delivers echo replies and MLD too).
- **What decoding must *not* refuse**: an unrecognised option is walked over, and the test puts a
  recognised option *after* it so a walk that resumed at the wrong offset fails. A known type with
  an impossible length degrades to `Other` rather than losing the whole message.
- **The validation refusals**, each because the alternative is a silent no-op on the wire: an
  autonomous prefix that is not a `/64`, a preferred lifetime past its valid lifetime, an MTU
  below 1280, a 16-bit router lifetime overflow, an unparseable address, a boolean given as a
  string.
- **No octets anywhere in event data.** `event_data_carries_no_octets_anywhere` walks every key and
  value of all five message types: no `*_hex`/`*_raw`/`*_bytes`/`data`/`payload` key, no long
  hex-looking string. This is the rule the whole protocol design hangs on and it is worth a test
  rather than a comment.

## `e2e_test.rs` — in-process, over the UDP transport

Builds a real `SpawnContext` and calls `Server::spawn` directly, because the child-process harness
cannot start NDP unprivileged (the `RawSockets` gate fires before `spawn`). The server is asked
for its declared `transport: "udp"`.

**That transport carries `source(16) || destination(16) || ICMPv6 message`, and the shape is the
point.** Those two addresses are exactly what the checksum depends on, so every message the server
emits is verified against the addresses it claims to have travelled between — `decode_reply` calls
`verify_checksum` before it looks at a single field. A test transport carrying only the ICMPv6
bytes would leave the most error-prone part of the protocol unexercised end to end.

`state.set_ollama_model(Some(...))` is set before spawning: without it `ensure_model_selected`
tries to auto-select against `localhost:11434` and the test would depend on the developer's
machine.

### The tests, and why each exists

| Test | The point |
|---|---|
| `a_router_advertisement_the_model_authors_reaches_the_wire` | The protocol's whole purpose. A mock model chooses a prefix, an RDNSS server and an MTU, and every one lands in the decoded advertisement — `mitm6` with reasoning. Also asserts the **source address comes from the server**, since that is what a host installs as its default router, and that the server filled in its own link-layer address. |
| `a_static_handler_answers_a_neighbor_solicitation_with_no_llm_call` | The deterministic path, with the LLM endpoint unreachable — a message arriving proves no call was *needed*, not merely that none was counted. Asserts the R/S/O flags and the target link-layer address, which is the field that decides where a peer sends traffic. |
| `every_message_type_raises_its_own_event` | All five events really fire. An event declared and never raised is a defect this repo has shipped in bulk, and `event_emit_sites_test` cannot tell which decoded message maps to which event — this drives one of each and requires the matching `decision=` line, which names the event id. |
| `a_message_with_a_wrong_checksum_is_dropped` | One corrupted checksum octet, then **the same solicitation uncorrupted as a control**. Without the control, "no reply" is indistinguishable from a server that was not listening. |
| `an_llm_failure_sends_nothing` | The reason the protocol is in the deliberately-silent class. Asserts the `decision=fail_closed_*` tag **and** that no datagram followed. |
| `no_backend_error_text_can_reach_a_peer` | The other half of the `WireFailure` rule, asserted as the stronger property: nothing is sent, so there is nothing for an error to hide in. |
| `no_policy_means_no_message_and_no_llm_call` | The default is an observer, and it costs no LLM round-trip per packet. A raw ICMPv6 socket on a busy link sees a great many. |
| `a_deliberate_refusal_is_logged_as_a_decision` | `no_response` must be distinguishable from silence **in the log**, since on the wire it is not. |
| `unusable_startup_parameters_are_refused` | Six combinations, including `udp_peer` with `transport: "raw"` (where it would do nothing) and `transport: "raw"` with no interface (where the server could start and never transmit). |
| `an_undeclared_parameter_names_the_declared_ones` | The error lists what is available, so a model can correct itself. |
| `the_raw_transport_refuses_rather_than_pretending` | The ARP/DataLink/ICMP/IS-IS defect — a server in `Running` having received nothing. Fixed four separate times elsewhere. |
| `the_metadata_declares_what_the_transport_really_needs` | `RawSockets`, `connectionless()`, `Experimental`, and a `notes` that says which half is proven. All four are load-bearing claims and all four are easy to quietly change. |

### Timing

No fixed sleeps. `wait_for_status` and `wait_for_all` poll the status stream against a deadline,
and every socket read is a `tokio::time::timeout`. The "nothing was sent" assertions use a
**non-blocking** `try_recv_from` and are made *after* the decision has been observed in the log, so
they are not racing a message still in flight. Under `--test-threads=100` the whole file finishes
in about a second.

## What still has no coverage

Everything past the codec on the real transport:

- No message this code produced has reached a real IPv6 stack.
- `Socket::new(IPV6, RAW, ICMPV6)`, the hop-limit and multicast-interface options, `send_to` and
  the receive loop have never been executed. Their error paths are asserted only through the "no
  such device" case, which fails before the privileged step.
- The kernel's own checksum behaviour on a raw ICMPv6 socket (RFC 3542 §3.1 says it overwrites
  ours) is documented, not observed.
- The `link_local_address` parameter is not bound as a real source address on the raw path, so what
  a peer would actually see there is unverified.

A green run here means "the bytes are right and the failure discipline is honest". It does not
mean NDP works. `src/server/ndp/CLAUDE.md` records the `feth`-pair experiment — with `rdisc6`,
`ndp -a` and a real stack's SLAAC as the peer — that would change that, and it has not been run.
