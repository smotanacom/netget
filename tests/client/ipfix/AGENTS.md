# IPFIX exporter checks

`e2e_test.rs` validates live command injection during a parked handler, complete
batch rejection before emission, independent periodic template refresh,
domain/data/options sequence semantics, template-ID reuse rejection, event and
action queue bounds, followup depth, common script memory, owned disconnect and
removal, unsolicited reply closure, and catalog/domain/template limits.

`peer_test.rs` requires Python ipfix 0.9.7's public MessageBuffer decoder to
bind an actual socket, then decode two native exports and assert IP addresses,
ports, reduced/large counters, seconds/milliseconds, UTF-8 with NULs, options
scope and sequence 0 then 3. Its readiness line announces the bound port.
On Linux/macOS the second required test runs the actual official GoFlow2 2.2.7
service with the raw producer. An independent fixture-only version-10 header
proves its UDP listener is ready, then each production export is sent once.
The service output must contain the templates/options and every field's exact
expected wire value. Decoding the external producer's base64 output is confined
to the test; no raw/base64 field reaches NetGet's model-facing API.

Use `tests/server/ipfix/install_peers.py` and its documented environment and
platform contract. Both services use loopback, temporary files, process cleanup
and bounded readiness/read deadlines. Missing peers fail. GoFlow2 tests are
required on Linux/macOS; no official service support/evidence is asserted on
Windows. A local UDP send is never treated as end-to-end acknowledgment. The
external peers are independent of NetGet's codec; native pair tests alone are
not the interoperability oracle. Protocol maturity remains Experimental.
