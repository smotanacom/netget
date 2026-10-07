# NETCONF selected SSH surface

Implementation scope is SSH netconf subsystem, mandatory client host-key verification,
native hello/capabilities, NETCONF1.0 delimiter and1.1 chunk framing, get/get-config/
edit-config, typed native errors and explicitly selected datastores. Server decisions
use shared handlers/instruction/memory; no local configuration database or YANG engine.
Do not infer persistence, authorization or commit from a local action or hello capability.

`wire.rs` checks length headers, chunks and aggregate message bytes before copying.
`xml.rs` uses a flat ordered Start/End/Text node stream, preserving qualified names,
namespace URIs, bindings, attributes and mixed text without recursive output nesting.
DTD/entities outside predefined/numeric XML references are refused; comments and
processing instructions are omitted, and CDATA becomes text. XML1.0/UTF8 only.
Whole XML message1MiB, depth32, nodes8192, raw/decoded contiguous text64KiB,
names256bytes, URI/attribute4096bytes, attributes32 per element, active namespace
bindings16, retained content8MiB; protocol wrappers count towards wire caps.

`owned_stream.rs` owns the sole TCP stream. Its I/O polls a CancellationToken future,
registering the native driver waker; dropping the registered owner cancels and wakes
russh's internal driver. Every pending model callback must also select cancellation.
Await the RunningSession returned by server::run_stream, not just setup. Never clone
the TCP stream or rely on dropping its internally spawned JoinHandle.

Peer evidence is maintained separately from maturity claims. Required unchanged
ncclient0.7.1 and netconf2.1.0/sshutil1.5.0 use pinned Paramiko3.5.1 in an owned Python
environment; modern Paramiko5 removes their SSH helper's unconditional DSS import.
No peer monkeypatches, global daemon/configuration changes or ambient SSH agent/keys.
Primary references: [RFC6241](https://www.rfc-editor.org/rfc/rfc6241.html),
[RFC6242](https://www.rfc-editor.org/rfc/rfc6242.html).
