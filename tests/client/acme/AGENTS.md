# ACME client tests

`peer_test.rs` runs pebble-challtestsrv (every name resolves to 127.0.0.1) and Pebble 2.10.1
(independent, unchanged; validation without random sleeps, no injected bad nonces) and drives
NetGet's client: register, order two names, http-01 validated by Pebble's VA against the client's
responder, dns-01 after the test publishes the TXT record through challtestsrv's management API,
finalize with a key file (mode 0600), revoke (Pebble refuses a second revocation as
`alreadyRevoked`), a name on Pebble's blocklist (`rejectedIdentifier`) and deactivation. Needs the
`NETGET_ACME_*` variables from `tests/server/acme/install_peers.py`; fails without them.
