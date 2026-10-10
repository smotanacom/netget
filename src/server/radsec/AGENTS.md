# RadSec server (RADIUS over TLS, RFC 6614)

TLS and framing only. Every packet goes through `RadiusServer::answer`, the RADIUS server's
own decision path: the same events (`radius_access_request`, `radius_accounting_request`,
`radius_status_server`), the same reply actions, the same signing and the same fail-closed
Access-Reject. The model sees RADIUS; the access log says RADIUS too. Port 2083 (IANA
`radsec`).

## TLS

- `certificate_file` + `private_key_file` (PEM), or neither for a fresh self-signed
  certificate. With `ca_file`, a client certificate chaining to it is **required** at the
  handshake (rustls `WebPkiClientVerifier`); a client without one, or with one from another CA,
  is refused before any RADIUS is read.
- `shared_secret` defaults to `radsec`, as RFC 6614 §2.3 fixes it.
- 10 s for the handshake; `idle_timeout_secs` (default 300) between packets.

## Framing and concurrency

A packet's own Length field frames it (`packet::read_frames`). A Length outside 20..=4096
closes the connection: a stream cannot be resynchronised. Up to 32 requests per connection are
answered at once and may be answered out of order, which RFC 6614 allows; replies already
being computed when the peer goes quiet are still written before the connection closes.

A Message-Authenticator that does not verify drops the packet, as an Accounting-Request with a
bad authenticator does. RADIUS gained Message-Authenticator signing and verification in the
same change, because RadSec peers (radsecproxy, FreeRADIUS proxying) and NetGet's own client
check it.

## Tests

`tests/server/radsec/`: raw TLS (`wire_test.rs`) and radclient through radsecproxy and through
FreeRADIUS as a proxy (`real_client_test.rs`). See its AGENTS.md.
