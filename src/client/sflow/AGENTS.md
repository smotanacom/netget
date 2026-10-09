# sFlow v5 typed exporter — Experimental

`export_sflow_samples` accepts one complete typed batch: declared agent address,
sub-agent, optional unsigned32 uptime and flow/counter samples. Each sample
supplies its own sequence number and source class/index. Flow samples also
supply sampling rate/pool/drops and input/output format/value. `expanded` defaults
to false. All counter fields are required with their explicit integer widths.
The supported record schemas and wire bounds live in `server/sflow/codec.rs`.
No opaque packet bytes, flow store or automatic statistical sampler is exposed.

`synthesized_header` creates a header-only IPv4/IPv6 TCP/UDP telemetry summary
from typed fields, with frame length and stripped-byte metadata. There is no
payload or transport checksum and it is not a captured/live data packet. Packet
length is the original reported IP length, which can exceed the header bytes.
Other flow actions emit typed IPv4/IPv6 summaries, Ethernet summaries or switch
metadata; counter actions emit interface/Ethernet/VLAN counters.

Each logical UDP client holds at most 32 declared agent/sub-agent sequence
entries, starting at zero and counting successfully locally sent datagrams
modulo 2^32. A candidate batch validates fully before transmission; state commits
only after a successful complete UDP send. Disconnecting destroys this state.
Sample sequence/pool/drop values remain caller-owned sFlow-instance values.
Uptime is supplied explicitly or derived from elapsed logical-client time modulo
2^32; there is no claim that it measures an actual monitored device's uptime.

The owned client task has one in-flight send, 32 queued events, 32 queued handler
actions and followup depth 8. Name lookup and send each have a 10-second deadline.
Command injection and disconnect remain available while a common handler is
parked. Manual removal cancels sends, socket, command handle and intercepts;
automatic disconnect drops parked handler/event/action futures before status
becomes Disconnected. Any UDP reply is unexpected and closes the session.

`sflow_connected` describes local readiness. `sflow_exported` reports typed
agent/sub-agent, sequence, uptime, sample/record/byte counts and always
`local_transport_only=true`. These events use the common client dispatcher,
shared memory, scripts and access log. A local transport receipt is not collector
reception, storage, durability or an acknowledgment; there is no automatic retry.

Tests require the pinned external Cistern public decoder and actual GoFlow2
collector, with literal byte assertions alongside typed values. Cistern's known
source-ID encoder defect and excluded VLAN emitter are documented in the server
AGENTS.md; its decoder is independently used with native normative VLAN output.
GoFlow2's VLAN record is opaque, so those assertions inspect exact 28 bytes.
No third-party source is vendored or linked, and no Cargo dependency is added.
The same Experimental limitations apply: v5 subset only, no capture, automatic
sampler/poller, SNMP configuration, storage, authentication, reliability,
fuzz/pcap or full-compliance claim.
