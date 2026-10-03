# HTTP/3 server tests

`e2e_test.rs` is registered in tests/server/mod.rs. The independent aioquic 1.3.0
client performs concurrent authenticated GET/POST requests, checks status, body,
headers and response trailers. The NetGet pair proves request and response
trailers and shared client handler logging. Connection/idle/ALPN limits, pending-handshake deadline/slot recovery, stream
credit and body bounds plus partial-body deadline and removal regressions exercise the vendored h3-quinn cancellation path and UDP port release.
Outgoing field tests accept exactly 32 KiB and reject one byte more after counting
the status pseudo-field; incoming fields test compact QPACK expansion beyond the
decoded limit. Static/script handlers provide decisions; no real model or external site is used.
The Quinn/h3 fixture is a lifecycle/bounds probe, not independent evidence.

Bootstrap/env pins are in tests/client/http3/CLAUDE.md. Missing independent peer
is a hard failure; no test is ignored or returns success when a peer is absent.
Run with the guarded programme wrapper:

```
NETGET_AIOQUIC_PYTHON=/Users/matus/dev/netget/.protocol-expansion-20261001/peers/aioquic-env/bin/python python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features http3 --test server -- http3 --test-threads=4
```

State remains Experimental: no pcap oracle, fuzz target or second independent
implementation is claimed. HTTP/3 is encrypted; Wireshark needs TLS secrets to
inspect application HEADERS/DATA, otherwise the capture demonstrates QUIC only.
