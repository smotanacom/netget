# Minecraft server tests

- `wire_test.rs`: raw packets against a scripted server — status with the handshake address
  reaching the handler, ping/pong, the three legacy forms plus mcstatus's `FE 01 FA`, and a
  login refused with the player's name; then the bounds (an announced packet one byte over
  2048, a 256-character address, a 17-character name, a silent client under a 1 s
  `idle_timeout_secs`, a second status request) and the fail-closed paths (a refusal closes
  both kinds unanswered; with no handler the model is unreachable, status closes and login is
  disconnected with the generic text).
- `real_client_test.rs`: mcstatus 14.2.0 (modern status, ping and its legacy client) and
  node-minecraft-protocol 1.68.0 (its ping, and a real `createClient` login that must end on
  NetGet's Disconnect in the login state). Both fail rather than skip without
  `tests/client/minecraft/install_peers.py`'s exports.

No LLM calls: every handler is a script or static.
