# RadSec server tests

The PKI is `tests/helpers/radsec_pki.rs` (rcgen): a CA, a server certificate for
`localhost`/`127.0.0.1`, a client certificate, and a stranger — a client certificate from a
second CA. `POLICY` (in `wire_test.rs`) accepts alice/wonderland with Reply-Message
"welcome alice" and rejects everyone else with "go away". No LLM calls.

- `wire_test.rs` (rustls + NetGet's RADIUS codec): no client certificate and the stranger's are
  both refused; two requests pipelined on one connection are each answered and pass
  `verify_reply` (Response Authenticator and Message-Authenticator); a request with a forged
  Message-Authenticator gets no reply while the next ones do (checked by removing the
  verification: the test then fails); with no handler the fail-closed Access-Reject arrives,
  still signed; Lengths of 4097 and 19 close the connection.
- `real_client_test.rs`: FreeRADIUS's `radclient` (UDP) through **radsecproxy** and through
  **FreeRADIUS as a proxy** with NetGet as its TLS home server (`check_cert_cn = "localhost"`).
  Each proxy verifies NetGet's signatures with `radsec` before re-signing for radclient, so an
  Access-Accept with "welcome alice" reaching radclient is the proxy agreeing with NetGet.

Peers: `apt-get install freeradius freeradius-utils radsecproxy`, and link
`/usr/sbin/freeradius` to `radiusd` as the RADIUS suites already require. Both tests fail
rather than skip when a binary is absent. CI: the `radsec-pairs` job.

FreeRADIUS reads `check_cert_cn = no` as the literal name "no" and refuses the handshake.
