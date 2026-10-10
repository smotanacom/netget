# WS-Discovery server tests

No LLM calls; a python script handler plays two ONVIF cameras. Camera A answers probes with
its address. Camera B answers without one and gives its address only on Resolve. A probe for
any other type is answered with silence.

`real_client_test.rs` uses **python WSDiscovery** (`pip install WSDiscovery`; interpreter from
`NETGET_WSD_PYTHON`, default `python3`). It fails rather than skips when the library is missing.

- **Multicast.** NetGet is bound to `0.0.0.0:3702` and joined to the group, and the library's
  `searchServices` probes the group. B's address in the result can only have come from the
  multicast Resolve python sent after B's address-less match.
- **Directed.** The library's own probe serializer and parser are used over a plain socket,
  aimed at NetGet's ephemeral port. `WSDiscovery.searchServices(address=…)` cannot be used:
  python-ws-discovery 2.1 sends a directed probe from a socket it never reads, so every reply
  is lost whatever the target does (checked against a stub responder). This test also checks
  that a probe for a type nobody has finds nothing.
- **2009/01.** A probe in the OASIS namespaces is answered in them. After a datagram nested
  past `MAX_DEPTH`, the next probe is still answered.
- **`parser_bounds`.** The codec's depth and list-length refusals, without a server.

The multicast test holds a static mutex. Port 3702 is shared (SO_REUSEADDR), but one multicast
test at a time keeps the answers countable.
