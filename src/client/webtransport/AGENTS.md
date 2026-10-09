# WebTransport client — Experimental

Opens one session to `https://<remote_addr><path>` with the vendored `wtransport` and runs the
server's `session::Session` loop on it (see `src/server/webtransport/AGENTS.md`): streams and
datagrams from the server raise `webtransport_stream` and `webtransport_datagram`, and the same
session actions open streams, send datagrams and close. The session opening raises
`webtransport_connected`.

Trust is one of: `certificate_sha256` (the browser rule — ECDSA P-256, valid at most 14 days),
`ca_cert_path`, or the system roots. Giving both of the first two is an error. Extra request
headers come from `headers` (for example `origin`). A refused session reports only that it was
refused: wtransport does not surface the status. Injected actions (`send_to_client`) are the
four session actions; `webtransport_reply` is only an answer to an event.
