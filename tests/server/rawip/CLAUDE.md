# Raw IP protocol-N — test strategy

**Run**:

```bash
./cargo-isolated.sh test --no-default-features --features rawip \
    --test server::rawip::e2e_test -- --test-threads=100
```

20 tests, all passing, none `#[ignore]`d. **LLM call budget: 2** (one mocked call in
`the_full_path_runs_and_a_hex_payload_arrives_as_bytes`, one in
`no_response_puts_nothing_on_the_wire`; the failure and malformed-packet tests assert **zero**).

## The split, and why it is the whole strategy

A raw socket cannot be opened without root, so no test can bind one. Pretending otherwise is how
a protocol ends up rated on evidence that does not exist. The suite therefore separates what
*can* be proven from what cannot, and says so:

1. **The IP header decoder, against literal packet bytes.** `decode_ip_packet` is a pure function
   over a byte slice — no I/O, no state, no privilege — so it is asserted field by field against
   hand-written RFC 791 / RFC 8200 packets. This is the part of the protocol that is genuinely
   proven.
2. **The full decode → event → LLM → action path**, over the unprivileged UDP test transport
   (`transport: "udp"`, which carries whole IP packets inside datagrams). Same decoder, same
   event, same executor, same emit path.
3. **The raw socket itself: not tested, anywhere.** Nothing binds one, sends on one, or receives
   on one. `src/server/rawip/CLAUDE.md` lists exactly what that leaves unproven.

## Why the transport tests bypass `ServerForm::create`

They call `RawIpServer::spawn_with_llm_actions` directly. That is not a shortcut around the
framework: the privilege gate in `server_startup` reads the protocol's **static** `metadata()`,
which declares `RawSockets`, and `metadata()` cannot see startup parameters. So an unprivileged
`ServerForm::create` is refused whatever `transport` says. The gate is a startup-path property;
the server module underneath it is what these tests drive.

The server is still registered in `AppState` (via `ServerInstance::new`) with a real non-empty
instruction, so `call_llm` takes exactly the path it takes in production.

## Test inventory

### Decoder (7)

| Test | What it pins |
|---|---|
| `ipv4_header_decodes_field_by_field` | every IPv4 field at its RFC 791 §3.1 offset: IHL, DSCP (top six bits of byte 1), ECN (bottom two), total length, identification, DF, TTL, protocol, addresses, and the payload sliced at the header boundary |
| `ipv4_options_are_reported_and_excluded_from_the_payload` | IHL 6 → 24-byte header, `options_present`, `options_length: 4`, and the option bytes **not** in the payload. An off-by-one here hands the model four bytes of IP option and calls them protocol data |
| `a_fragmented_packet_reports_its_offset_and_more_fragments_flag` | MF read from the flags bits, offset from the low 13 — not the two mixed together |
| `ipv6_header_decodes_field_by_field` | traffic class straddling bytes 0-1, the 20-bit flow label across bytes 1-3, payload length, next header, hop limit, both 16-byte addresses |
| `malformed_and_truncated_packets_are_refused_not_panicked` | eight inputs, each asserted to produce a **specific** `IpDecodeError`: empty, 2 bytes, 19 bytes, IHL 4, IHL 6 with 20 bytes captured, version 5, version 0, 39 bytes of IPv6 |
| `a_packet_cut_short_is_reported_rather_than_over_read` | a `total_length` larger than what arrived sets `payload_incomplete` and reads only what is there; one smaller than the header falls back rather than producing a negative-length slice |
| `iana_names_are_reported_where_known_and_absent_where_not` | GRE/ESP/AH/SCTP/TCP/UDP resolve, an unassigned number returns `None` rather than a guess |

The malformed set is the one to extend when touching the decoder. It is the only thing between a
stranger's bytes and the receive loop, and a panic there kills the whole server silently inside a
`tokio::spawn`.

### The genericity claim, asserted (1)

`decoding_does_not_depend_on_the_protocol_number` decodes the same packet with twelve different
protocol numbers (0, 1, 41, 47, 50, 51, 89, 112, 132, 200, 253, 255) and requires every field —
**including the payload slice** — to be identical apart from `protocol` itself.

**This is the test that should fail if someone adds a `match protocol_number` to this module.**
It is the executable form of the rule in `src/server/rawip/CLAUDE.md`: if GRE ever gets special
framing here, the protocol has stopped being generic and belongs in its own module.

### Startup parameters (4)

`protocol_number_is_required_and_range_checked` (no default, `-1`/`256`/`100_000` refused, 0 and
255 accepted, defaults are IPv4 + raw), `tcp_and_udp_are_refused_and_point_at_the_real_protocols`
(6 and 17 refused, and the message must name `'tcp'` / `'udp'` — a refusal that does not say what
to do instead is half a refusal), `ip_version_and_transport_are_parsed_and_validated`, and
`an_undeclared_startup_parameter_is_refused_by_name`.

### Executor (4)

| Test | What it pins |
|---|---|
| `the_executor_actually_decodes_the_encoding_it_documents` | `"48656c6c6f"` as `hex` → five bytes; the **same string** as `utf8` → its own ten bytes; utf8 is the default |
| `the_executor_refuses_what_it_cannot_honour` | `base64` refused rather than silently treated as text; non-hex declared hex refused; a bad `destination` refused; an unknown action refused |
| `every_advertised_action_runs_its_own_declared_example` | each action's own `example` is accepted by its own `execute_action` — the local form of the `executable_examples_test` ratchet, and the shape a model copies |
| `the_event_offers_the_model_the_protocols_actions` | the event carries both action names, so `call_llm` actually advertises them |

The hex pair is the important one. `send_tcp_data` documented hex in three places and called
`as_bytes()`, so a model following the documentation put literal ASCII on the wire. Asserting
both encodings of the *same* string is what makes the assertion mean something.

### End to end, over the UDP test transport (4)

**`the_full_path_runs_and_a_hex_payload_arrives_as_bytes`** — the mock matches
`on_event("rawip_packet_received")` **and** `and_event_data_contains("protocol", "47")`, so the
rule cannot match unless the header really decoded; it answers with a hex payload. The test then
asserts the peer received the five bytes `Hello` — **not** the ten ASCII characters
`48656c6c6f` — and reads the recorded call's `event_data` back to assert the decoded header the
model was shown: `ip_version`, `source`, `destination`, `ttl`, `protocol`, `protocol_name`,
`identification`, `dscp`, `flags.dont_fragment`, `payload_length`, `payload_encoding`, `payload`,
`listening_protocol_number`.

**`no_response_puts_nothing_on_the_wire`** — the model's explicit silence emits nothing, and the
call still happens (`expect_calls(1)`). This is what separates a decision from a failure.

**`an_llm_failure_puts_nothing_on_the_wire`** — the backend is `http://127.0.0.1:1`, where
nothing listens, so every call fails immediately. **Nothing may reach the peer.** A generic IP
protocol has no error frame — netget deliberately does not implement the protocol above IP — so
any bytes emitted would be a guess at a format nobody defined. In particular a `WireFailure`
category string appearing here would be a leak into a protocol that has no format to put it in,
and the assertion message says so.

**`a_malformed_packet_is_dropped_without_reaching_the_model`** — a datagram with version nibble 7
produces no reply **and zero LLM calls** (`expect_calls(0)`, plus an explicit `call_count()`
check). The event's contract is decoded header fields; handing the model an undecodable blob
under those field names would have it reason about numbers that were never there.

## Mock discipline

Every mocked test finishes with `mock.wait_for_expectations(30).await` followed by
`mock.verify_calls().await?` — waiting on the expectations waits on the exchange, because the
exchange finishes with the last LLM call it provokes. Fixed sleeps are enough alone and not when
a hundred tests run together.

Each mocked test uses **one** rule. Two rules on the same event with no way to tell them apart is
the most common mistake in this repo: the first answers everything and the second reports zero
calls.

Everything binds 127.0.0.1 only. Nothing contacts an external endpoint.

## What a future contributor should add

* **The raw-socket test, under `sudo`, against a real GRE peer.** That is the only thing standing
  between this protocol and Beta, and no amount of additional mocked coverage substitutes for it.
* IPv6 over the UDP transport. The IPv6 decoder is covered against literal bytes but no
  end-to-end test runs `ip_version: "ipv6"`; on a real IPv6 raw socket the kernel strips the
  header anyway (see `src/server/rawip/CLAUDE.md`), so this would exercise the transport, not
  close that gap.
* A payload larger than `EVENT_PAYLOAD_LIMIT`, asserting `payload_truncated` and that
  `payload_length` still reports the full size.
