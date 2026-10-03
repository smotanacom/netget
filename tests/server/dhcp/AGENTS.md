# DHCP Protocol E2E Tests

`tests/server/dhcp/test.rs`. Two tests, **6 LLM calls total**.

## Strategy

The client half is written from RFC 2131 / RFC 2132 **in the test file** and deliberately does
not use `dhcproto`, which is what the server encodes with. Using the same codec on both sides
proves only that it round-trips with itself; a real DHCP client is a decoder like the one here.

There is no usable real DHCP client to point at these servers — `dhclient` and `ipconfig` bind
UDP/68, need root, and cannot be aimed at an ephemeral loopback port — so an independent
decoder is the strongest available peer. Say that plainly rather than implying a client was
run.

`build_request()` constructs the BOOTREQUEST; `DhcpMessage::decode()` parses the reply header
and walks the option list, rejecting a truncated option, a missing magic cookie, an `hlen`
above 16, and options that run off the end.

## What is asserted

`assert_echoes_request()` covers what RFC 2131 section 4.1 requires of every reply: `op` =
BOOTREPLY, `htype`/`hlen`, the **xid echoed from the request** (a client silently discards a
reply whose transaction id differs, so a mismatch presents as a timeout, not an error), the
client hardware address, and the broadcast flag.

Per message type, against RFC 2131 table 3:

| Test | Asserts |
|---|---|
| `test_dhcp_discover_offer_and_request_ack` | OFFER: option 53 = 2, `yiaddr`, options 1 (mask), 3 (router), 6 (both DNS servers, in order), 51 (lease), 54 (server id). ACK: option 53 = 5, `yiaddr`, 51, 54. Then a datagram with `hlen=255` gets **no reply and no LLM call**, and a following DISCOVER is still answered |
| `test_dhcp_nak_rejects_request` | NAK: option 53 = 6, `yiaddr` zero, **no** option 51, option 56 carrying the model's message, option 54 |

## LLM call budget

- `test_dhcp_discover_offer_and_request_ack`: 1 startup + 2 DISCOVER + 1 REQUEST = 4
- `test_dhcp_nak_rejects_request`: 1 startup + 1 REQUEST = 2

One server per test handles every message type, rather than one server per scenario.

## The malformed-datagram rule

The first test carries a mock rule matching `message_type: "unknown"` with
`.expect_calls(0)`. It is an assertion, not a handler: an undecodable datagram must be dropped
by the server, never turned into a `dhcp_request` event. Before this suite it *was* forwarded,
spending an LLM round trip on an event whose every field read "unknown" and out of which no
reply could be built (`base_reply` has no xid to echo). `hlen=255` is the specific shape that
panics inside `dhcproto::Message::chaddr()`, so the same datagram also checks that the listener
survives it.

## Mock notes

Message types are matched in dhcproto's `Debug` spelling — `"Discover"`, `"Request"` — not
upper case. A handler comparing against `"DISCOVER"` never matches.

Unlike DNS, DHCP does **not** need `respond_with_actions_from_event()`: the reply actions take
the transaction id from the per-datagram request context rather than from the model, so a
static mock is correct here. The test still asserts the echoed xid, which is what that rule
exists to protect.

## Not covered

A full DORA exchange driven by a real client; relayed delivery (the reply goes to the UDP
source address, not to `giaddr:67`); relay agent options (82); DHCPv6; lease state of any kind
(there is no lease database — the model picks every address).

## References

- [RFC 2131](https://datatracker.ietf.org/doc/html/rfc2131),
  [RFC 2132](https://datatracker.ietf.org/doc/html/rfc2132)
- [dhcproto](https://docs.rs/dhcproto/latest/dhcproto/) — server side only
