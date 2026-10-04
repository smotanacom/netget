# RDAP tests

Peers: `tests/server/rdap/install_peers.sh ROOT` (cargo + Go) prints `NETGET_OPENRDAP`,
`NETGET_ICANN_RDAP`, `NETGET_ICANN_RDAP_SRV`, `NETGET_ICANN_RDAP_SRV_DATA`.

```bash
./cargo-isolated.sh test --no-default-features --features tcp,rdap \
    --test server --test client -- rdap:: --test-threads=100
```

- `peer_test.rs` — OpenRDAP 0.10.2 (Go) under a `/rdap` base path: domain, nameserver, IP
  (an address the covering network answers), autnum, entity, domain search, help, and a 404
  that fails the lookup; ICANN rdap 1.0.0 (Rust, separate lockfile) reads every class and the
  search and sees the 403 error object. A malformed autnum is 400 with an RDAP error body and
  no handler call. Both peers unchanged; tests fail without them.
- `query_test.rs` — normalization and refusals, envelope/class/redirect rules, bounds, and
  the NetGet pair (lookup, search, 404, 302 referral, 403 body, client-side refusal).

`tests/client/rdap/peer_test.rs`: ICANN rdap-srv 1.0.0 serving objects made by its own
rdap-srv-data — every lookup class, search, help, 404 and 307 referral.
