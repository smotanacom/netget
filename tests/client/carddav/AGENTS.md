# CardDAV client tests

`peer_test.rs` runs Radicale 3.8.1 (independent, unchanged; plain htpasswd user alice) and
drives NetGet's client through discovery from the well-known URL, collection creation, PUTs,
list, an addressbook-query, GET and DELETE. Needs `NETGET_DAV_BIN` from `tests/server/caldav/install_peers.py`;
fails without it.
