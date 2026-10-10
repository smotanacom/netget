# Minecraft client tests

- `session_test.rs`: against NetGet's own server (status, legacy status, a refused login),
  and against a scripted fixture for what that server never sends — Set Compression, a login
  plugin request and a cookie request the client must answer, a deflated Login Success, an
  Encryption Request, an announced packet over the client's bound, a compressed packet whose
  declared size is a lie (verified by removing the size check: the lie is then reported as an
  accepted login), and a translate/extra disconnect reason.
- `real_server_test.rs`: node-minecraft-protocol 1.68.0's `createServer`, unchanged, through
  `js/peer.cjs` — offline mode for status with a player sample, the legacy ping, a login it
  accepts through compression and one it kicks in the login state; online mode for the
  Encryption Request. Fails rather than skips without the peer.

`install_peers.py ROOT` installs mcstatus (hash-pinned) and node-minecraft-protocol
(`npm ci` from `js/package-lock.json`) and prints NETGET_MINECRAFT_PYTHON,
NETGET_MINECRAFT_NODE_MODULES and NETGET_MINECRAFT_PEER. Both suites use them. No LLM calls.
