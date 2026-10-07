# ethernet_ip independent peer checks

The peer must be installed; tests fail if unavailable, never skip.
See tests/helpers/ICS_PEERS.md for installation and commands.
Both roles use a separate stack, with negative and owner-stop socket checks.
