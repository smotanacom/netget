# RESTCONF client tests

`peer_test.rs` runs FreeCONF's RESTCONF server with its car example (independent, unchanged).
NetGet's client discovers it through host-meta (FreeCONF serves neither the API root nor
yang-library-version), finds the car module, reads the module, a tire and a leaf, PATCHes the
speed and reads it back, deletes a tire, gets 404 for it, and invokes car:addOil once accepted
and once refused with FreeCONF's RFC 8040 error. Needs `NETGET_RESTCONF_FREECONF`.
