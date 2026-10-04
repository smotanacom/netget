# ACME tests

Peers: `python3 tests/server/acme/install_peers.py ROOT` builds lego 4.35.2, Pebble 2.10.1 and
pebble-challtestsrv from `tools/go.mod` / `tools/go.sum`, copies Pebble's test certificates from
the verified module, installs certbot 5.8.0 and acme 5.8.0 (hash-pinned wheels) and prints the
`NETGET_ACME_*` variables. `tests/helpers/acme.rs` holds the CA policy script, a test TLS root
and server certificate, and the Pebble launcher.

- `peer_test.rs` — lego over HTTPS: two names by http-01 (Rust fetches from lego's solver),
  `list`, revoke with reason 4, an order the policy rejects; certbot over HTTP with an RSA
  account: standalone http-01, a wildcard and its base by dns-01 through a manual hook,
  revocation, and an account the policy refuses.
- `wire_test.rs` — raw JWS: 415, 413, nonce reuse, URL mismatch, an unsupported alg, a forged
  payload, the same key's account, `onlyReturnExisting`, contacts, identifier types and names,
  `notAfter`, `orderNotReady`, another account's order, wildcard challenges, an http-01 Rust
  cannot fetch (invalid, handler not asked), dns-01 and a CSR for other names (`badCSR`); a CA
  with no handler answer creating nothing; the NetGet pair over HTTPS.

`tests/client/acme/peer_test.rs` — NetGet's client against Pebble.
