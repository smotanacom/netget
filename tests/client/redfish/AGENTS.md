# Redfish client tests

`peer_test.rs` runs DMTF's Redfish-Mockup-Server 1.3.0 (independent, unchanged) on its bundled
public-rackmount1 mockup and drives NetGet's client through session login (204 with
X-Auth-Token), the systems collection, a system, a PATCH it reads back, a reset whose target it
takes from the system's Actions (the mockup logs the POST), and a 404. Needs the
`NETGET_REDFISH_*` variables from `tests/server/redfish/install_peers.py`; fails without them.
