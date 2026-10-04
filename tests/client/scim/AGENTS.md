# SCIM client tests

`peer_test.rs` runs scim2-server 0.4.0 (independent, unchanged) and drives NetGet's client
through discovery, two creates, a filtered sorted list, a get, a PATCH (read back with a GET),
a replace, a 409 uniqueness conflict, a delete and the 404 after it. Needs `NETGET_SCIM_BIN`
from `tests/server/scim/install_peers.py`; fails without it.
