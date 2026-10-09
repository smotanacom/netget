# CalDAV client tests

`peer_test.rs` runs Radicale 3.8.1 (independent, unchanged; plain htpasswd user alice) and
drives NetGet's client through discovery from the well-known URL, collection creation, PUTs,
an If-None-Match conflict (412), list, a time-range query, GET, DELETE, and a stale and a current If-Match. Needs `NETGET_DAV_BIN` from `tests/server/caldav/install_peers.py`;
fails without it.
