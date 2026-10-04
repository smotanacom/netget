# RDAP client tests

`peer_test.rs` runs ICANN rdap-srv 1.0.0 (independent server, unchanged) on objects created by its
own `rdap-srv-data`: example.com, ns1.example.com, AS64496, 192.0.2.0/24, help and a referral for
moved.example. The client's script handler chains domain → nameserver → ip (resolved to the covering
network) → autnum → domain search → help → 404 → 307 and every answer is asserted. Needs the
`NETGET_ICANN_RDAP_SRV*` variables from `tests/server/rdap/install_peers.sh`; fails without them.
