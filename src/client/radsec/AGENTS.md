# RadSec client (RFC 6614)

The RADIUS client over one TLS connection: `RadiusClient::run` with a `Link::Stream`, so its
actions, events, signing and reply verification are exactly the RADIUS client's
(`radius_access_request` → `radius_access_accept` / `_reject` / `_challenge`,
`radius_accounting_request` → `radius_accounting_response`, `radius_error`). See
`src/client/radius/AGENTS.md`.

- `ca_file` is **required**: the server is always verified, against `server_name` (default:
  the host of `remote_addr`). There is no option to skip verification.
- `certificate_file` + `private_key_file`: the client certificate RadSec servers normally
  require.
- `shared_secret` defaults to `radsec`; `timeout_ms` (default 10 000) per request. Nothing is
  retransmitted over the stream.
- One connection carries authentication and accounting. If it closes, the client ends.

## Tests

`tests/client/radsec/`: NetGet's own RadSec server, FreeRADIUS's TLS listener, and radsecproxy
in front of FreeRADIUS. See its AGENTS.md.
