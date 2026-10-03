# Forward emitter tests

Command tests exercise atomic validation, no bytes for rejection, injection during manual
connect, disconnect and mismatched ACK cleanup. Both-role tests drive current-memory ACK
follow-ups until the declared depth bound. Official Fluentd1.19.4 peer_receiver.rb runs its
unmodified Engine/in_forward transport, EventTime/PackedForward/decompression/parser and ACK
logic on an OS-assigned TCP port. A custom unbuffered Output observes routed typed events;
it does not replace framing/parser or write domain storage. The listening fd is observed
without ownership to obtain its actual port. The engine process has a 30s lifespan and
kill-on-drop cleanup in tests.

The test requires four mode observations including tag, Unicode record and exact nanoseconds,
plus matching ACK events for all four requests. NETGET_FORWARD_RUBY/GEM_HOME/GEM_PATH and the
pinned Python peers are mandatory, with absent tools/import failures failing rather than
skipping. Bootstrap details in the server test notes. No authentication/TLS, UDP heartbeat,
JSON convenience framing, binary records, persistence, pcap or fuzz evidence claimed.

Pending-ACK cap tests verify rejection before wire output; a parked connected handler
keeps commands/ACK parsing available until the bounded event queue overflows and closes.
Timeout tests advance the clock past the 10s pending-ACK deadline and require a standard
forward_ack_timeout observation with no replay.
Additional command tests require no bytes for packed/compressed inner-depth rejection,
close before output for 33 protocol actions in one handler, and cleanup on repeated ACK,
malformed MessagePack and unsupported secure-forward HELO. All 7 client tests passed in the
24-test standalone Fluent Forward suite on 2 October 2026.
