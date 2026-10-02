# DoQ server tests

`e2e_test.rs` is registered through `tests/server/mod.rs`. Tests bind loopback and
use static/script handlers or an intentionally unreachable model endpoint; no
real model or external DNS resolver is used.

Independent interoperability: Knot `kdig +quic` authenticates the generated PEM
certificate with `+tls-ca` and `+tls-hostname=localhost` and checks A, AAAA and
NXDOMAIN. Missing kdig is a hard failure (install Knot DNS with QUIC support).
This is an independent DNS encoder/decoder and transport implementation.

Other tests use a Quinn transport peer with Hickory messages; these prove framing,
parallel streams, zero IDs, question echo, FIN, bad length/truncation/extra-message
rejection (including extra bytes inside the declared DNS length), oversize bounds, client cancellation, idle/connection limits, ALPN,
SERVFAIL and explicit NOTIMP, server stop and UDP port release. Decision tests assert
operator status logs alongside wire outcomes for positive/negative answers, deliberate
silence, invalid action and unavailable backend. This peer is not
counted as independent DNS evidence. `support.rs` is shared with the client tests.

Run using the programme's guarded build wrapper when working in the expansion:

```
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --offline --no-default-features --features doq --test server -- doq:: --test-threads=4
```

For normal repository work use `./cargo-isolated.sh` in place of the wrapper.
State remains Experimental: no pcap oracle, fuzz target, or second independent
server-side peer has been established. No stable/full-RFC claim is made.

Independent client used: `/opt/homebrew/bin/kdig`, Knot DNS 3.6.0. `kdig -h`
explicitly advertises `+[no]quic`, along with TLS CA/hostname/SNI options. Merely
finding the binary is not considered proof of QUIC support: the real query test
must pass.

Verified on 2026-10-01: the combined guarded `--features doq --test server
--test client -- doq:: --test-threads=4` run passed 9 server and 8 client tests,
with zero ignored tests. Socket tests require loopback socket permission in a
restricted sandbox. Existing generic registry/action/startup-default/pairing
ratchets are delegated to the expansion coordinator for its combined feature run.
