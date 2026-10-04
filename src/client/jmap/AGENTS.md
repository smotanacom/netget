# JMAP client — Experimental

Fetches the session (`session_path`, `/.well-known/jmap` by default) over HTTPS or, with
`tls: false`, HTTP, following redirects only on the same origin (Stalwart redirects to
`/jmap/session`), and refuses a session whose `apiUrl` is on another origin. Authentication is
Basic (`username`/`password`) or Bearer (`api_token`). `ca_cert_path` replaces the system roots
with that certificate, verified by rustls (the platform verifier will not take a self-signed
server certificate as its own anchor).

`jmap_connected` reports the username, accounts, primary accounts, capabilities and state.
`jmap_request` sends up to 16 calls (`request.rs` checks them); `using` is derived from the
method names and a missing accountId becomes the primary account for the method's capability.
`jmap_response` carries the method responses, the session state and whether it moved, or a
request-level problem with its HTTP status. Handler-driven requests chain to depth 8.
