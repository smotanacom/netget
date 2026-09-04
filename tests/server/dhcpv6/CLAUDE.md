# DHCPv6 E2E Tests

`tests/server/dhcpv6/e2e_test.rs`. Three tests, **9 mocked LLM calls** (plus the retries the
failure test provokes on purpose).

## Strategy

The client half is written from RFC 8415 **in this file** and deliberately does not use
`dhcproto`, which is the codec the server encodes with. Encoding and decoding with the same
library on both sides asserts only that the library round-trips with itself; a real DHCPv6 client
is an independent decoder like this one. The root `CLAUDE.md` names this failure mode — `rss` sat
at Experimental for months because its test parsed the server's output with the same crate that
produced it.

There is no usable real DHCPv6 client to point at these servers: `dhclient -6`, `dhcpcd` and
`odhcp6c` bind UDP/546, need root, and drive a kernel interface rather than an ephemeral loopback
port; macOS ships no DHCPv6 client binary at all (`ipconfig` asks configd to run DHCPv6 on a real
interface). None of those binaries exists on this machine. So the peer is an independent reading
of the spec, not an independent implementation — say that plainly rather than implying a client
was run, and that is why the protocol is rated `Experimental`.

Everything runs on `::1` (DHCPv6 is IPv6-only) on an ephemeral high port. Port 547 is privileged;
`server_startup.rs` only enforces `PrivilegedPort` when the requested port is actually below
1024, so `"port": 0` needs no privileges.

## The codec in the test file

- `build_message(msg_type, xid, options)` — `msg-type | transaction-id | options` (RFC 8415 §8).
  The transaction id is **three** octets. A `[u8; 3]` is used throughout rather than a `u32`
  precisely so it cannot silently become four.
- `option()`, `identity_association()`, `ia_addr_option()`, `oro_option()` build the option TLVs.
- `Dhcpv6Message::decode` walks the option stream and rejects a truncated header or an option
  whose length runs off the end.
- `identity_association(code)` decodes IA_NA / IA_PD *and* the IA Address / IA Prefix options
  encapsulated inside them.
- `domain_search()` decodes option 24 as the sequence of RFC 1035 wire-format names it is,
  **including compression pointers**. `dhcproto` encodes the list through a `trust-dns`
  `BinEncoder` with compression enabled, so a second name sharing a suffix with the first comes
  back as a pointer. A decoder that cannot follow one would fail on a perfectly legal option.

## What is asserted

`assert_echoes_request(xid)` covers what RFC 8415 §16 requires of every server message: the
client's transaction id (a mismatch presents as a **timeout**, not an error, because the client
discards the message silently), the Client Identifier echoed back, and a Server Identifier
present.

| Test | LLM calls | Asserts |
|---|---|---|
| `test_dhcpv6_solicit_advertise_request_reply` | 4 | SOLICIT→ADVERTISE (type 2), IA_NA under the client's IAID with T1/T2 and the IA Address lifetimes, IA_PD with the delegated prefix and its length, option 23, option 24, option 7 (Preference), **no** option 14. REQUEST→REPLY (type 7) confirming the same address, and that the ADVERTISE and REPLY carry the **same** Server Identifier. RELEASE→REPLY with a Success status code (option 13) and no fabricated IA_NA |
| `test_dhcpv6_rapid_commit_and_information_request` | 3 | A SOLICIT with option 14 is answered with a REPLY, not an ADVERTISE, and that REPLY carries option 14 with zero-length data. An INFORMATION-REQUEST **with no Client Identifier** (RFC 8415 §18.2.6 makes it optional) is answered with configuration only: no Client Id invented, no IA_NA, both resolvers in order, both search domains |
| `test_dhcpv6_llm_failure_sends_nothing` | 2+ | An LLM failure produces **no datagram at all**, and the `decision=fail_closed_` token appears in the log. A CONFIRM produces no datagram and is logged as dropped |

## The silence test is the important one

DHCPv6 is in the deliberately-silent class: a reply writes an address, lifetimes and resolvers
into the client's stack, and the protocol has no way to say "the backend is down" — a Status Code
is a positive statement about a *lease*. So the failure path has to be asserted as an absence.

The trap with an absence is that it is also what you get when the datagram never reached the
server. So the mock rule that provokes the failure carries `expect_at_least(1)`: the model
**was** consulted, its answer was unusable, and nothing went on the wire. Without that count the
test would pass against a server that was never listening.

The CONFIRM half checks the other silent path — dropped *before* the model, because CONFIRM asks
about a binding and NetGet keeps none (RFC 8415 §18.3.3 requires silence from a server that
cannot perform the test). It is asserted through the log line, since a message that raises no
event has no event name to write a mock rule against.

## Mock notes

**Both echo paths are exercised on purpose.** The reply actions take the transaction id from the
per-datagram request context, so a static mock is correct — but the `transaction_id` override
exists for out-of-band replies and would otherwise be untested. So the SOLICIT rules use
`respond_with_actions_from_event` and pass `e["transaction_id"]` through explicitly, while the
REQUEST, RELEASE and INFORMATION-REQUEST rules omit it and exercise the automatic echo. Every
reply is asserted against the id the client actually sent, either way.

`DEFAULT_SERVER_DUID` is asserted byte for byte, not merely checked for presence. It is DUID-EN
with IANA's reserved documentation enterprise number 32473 and the identifier `netget`, and it
has to be **constant**: the client addresses its REQUEST to the Server Identifier from the
ADVERTISE and rejects a REPLY carrying a different one, while NetGet holds no state between those
two model calls. A randomly generated default would break every four-message exchange, and this
assertion is what would catch that.

Each test uses one server for every message type rather than one server per scenario.

## Not covered

A real DHCPv6 client; multicast delivery to FF02::1:2; relay (RELAY-FORW / RELAY-REPL); CONFIRM
and DECLINE as answerable messages; temporary addresses (IA_TA); per-IA status codes;
authentication; lease state of any kind — there is no lease database, and the model may hand the
same address to two clients.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features dhcpv6 --test server dhcpv6:: \
    -- --test-threads=100
```

Note `--test server dhcpv6::`, not `--test server::dhcpv6::e2e_test`: `--test` names the test
*binary*, and the path is a filter argument.

## References

- [RFC 8415](https://datatracker.ietf.org/doc/html/rfc8415),
  [RFC 3646](https://datatracker.ietf.org/doc/html/rfc3646),
  [RFC 6355](https://datatracker.ietf.org/doc/html/rfc6355)
- `src/server/dhcpv6/CLAUDE.md` — the server side, and why silence is the correct failure mode
