# CardDAV tests

Peers come from `tests/server/caldav/install_peers.py` (`NETGET_DAV_BIN`); the store script is
in `tests/helpers/dav.rs`.

- `peer_test.rs` — vdirsyncer discovers and downloads two cards, then uploads an edit and a new
  vCard 4.0 card and propagates a local delete.
- `http_test.rs` — vCard validation; addressbook-query with ends-with, starts-with,
  is-not-defined, allof, anyof, negate-condition and limit; multiget with address-data; invalid
  data refused; extended MKCOL with and without the addressbook resourcetype; MKCALENDAR
  refused; the NetGet pair.

`tests/client/carddav/peer_test.rs` — NetGet's client against Radicale.
