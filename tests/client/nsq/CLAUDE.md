# NSQ client validation

Use the programme build wrapper:

```sh
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features nsq --test client --test server nsq:: -- --test-threads=4
```

`real_server_test.rs` launches an independent nsqd with ephemeral loopback TCP/HTTP
ports and a private temporary data directory, through RealServer. The guard reaps the
process group on success, failure and panic. Missing nsqd fails with install guidance:
`brew install nsq`; on Linux use the official nsq v1.3.0 release binary (CI already
installs the release for the broker's to_nsq/nsq_tail peers). No environment variables
are required if nsqd/to_nsq/nsq_tail are on PATH or standard helper fallback paths.

Verified local peer: nsqd v1.3.0 built with go1.27.1, `/opt/homebrew/bin/nsqd`.
Homebrew's source archive is pinned to SHA256
`c6289e295aaa40c8d9651de76e66bc9f23e7f5c40b1cc051ea5901965093e1f0` at
https://github.com/nsqio/nsq/archive/refs/tags/v1.3.0.tar.gz.
CI's existing official Linux release is
https://github.com/nsqio/nsq/releases/download/v1.3.0/nsq-1.3.0.linux-amd64.go1.21.5.tar.gz
and includes nsqd alongside the existing tools. The suite
proves SUB starts at RDY0, RDY1 admits one in-flight delivery, FIN admits the next without
another RDY, RDY0 pauses requeue, REQ redelivers the same ID with incremented attempts,
TOUCH succeeds or produces recoverable errors, PUB/MPUB/DPUB body framing, and CLS.
Requeue ordering is deliberately not assumed. Static/script and mocked model actions
are asserted by subscribing through the independent daemon; the mocked model's memory
update is checked on the followup prompt. A manual connected event stays healthy across
three daemon heartbeat intervals and remains controllable through injected disconnect.

`session_test.rs`: NetGet broker pair; injection during fragmented frames; final response
survives EOF; explicit unsupported negotiation; disconnect/stop during stalled IDENTIFY;
all three tasks tracked; followup bound, blocked-write cancellation/deadline and stalled-handler
queue bound. The RDY reduction fixture preserves legal deliveries already in transit when
RDY0 is sent. `wire_test.rs`: names/IDs/numbers/body/MPUB bounds, frame size/type bounds before allocation, exact largest frame and partial-frame
read deadlines. Source tests stay outside src. No ignore or skip gates.

NSQ has no supported tshark/Wireshark dissector in this environment. Capture transport
is TCP, BPF `tcp port <port>`, display filter `tcp.port == <port>`, no decode-as clause.
This is transport visibility, not a protocol packet oracle. Maturity remains Experimental.
