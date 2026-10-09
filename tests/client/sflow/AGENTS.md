# sFlow exporter checks

`e2e_test.rs` uses real UDP sockets and common forms/command handles. It checks
whole-batch atomic rejection (including a late invalid counter), literal native
output, command injection/disconnect while a manual handler waits, per-agent
datagram sequences, transport-only export events, bounded sequence state,
event/action queues, followup depth, owned intercept/socket release, unexpected
reply rejection and native pair/common-script memory. Sample sequences are
caller supplied, and the tests make no inferred per-source sampler claim.

`peer_test.rs` requires the pinned unmodified Cistern public decoder and live
official GoFlow2 2.2.7 service from the server bootstrap. The decoder checks a
native IPv6 raw header against literal bytes, source IDs, switch data and a
normative 28-byte VLAN counter with a 64-bit value above 2^53. The actual service
checks compact/expanded flow and counter formats, interface encoding, IPv4
summaries, datagram sequences and exact VLAN bytes. Both exported datagram
sequences must occur exactly once; GoFlow2's concurrent workers may report them
in either order. Native socket sequence checks retain arrival-order coverage.
GoFlow2's opaque VLAN
representation and the peer's excluded defective VLAN encoder are explicit;
neither native self-roundtrip nor a peer self-roundtrip supplies the oracle.

Run both roles with the environment documented in `tests/server/sflow/AGENTS.md`.
Missing peers fail, with no ignore/skip. Native lifecycle and semantic tests are
portable when socket support exists; official daemon bootstrap evidence is
limited to Linux amd64/macOS arm64. The implementation remains Experimental,
without capture, fuzz, SNMP, automatic sampling/polling, persistence,
authentication, ACK, reliable-delivery or full-compliance evidence.
