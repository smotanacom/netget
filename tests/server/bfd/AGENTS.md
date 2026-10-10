# BFD server tests

No LLM calls; python script handlers decide.

- `real_client_test.rs`: **BIRD 2** (`apt-get install bird2`), unprivileged, with a static
  multihop session to NetGet on 127.0.0.2:4784, read back with `birdc show bfd sessions`.
  Fails rather than skips without it. Both tests hold a static mutex on port 4784.
  - Up; then the model's `bfd_set_timers` (rx 400 ms) makes BIRD's interval 0.400, which is
    the Poll Sequence completing.
  - AdminDown and back through `send_to_peer`. An invalid action is Rejected.
  - Stopping the server lets BIRD detect the silence.
  - Keyed SHA1 with the right key comes Up. With the wrong key BIRD stays Down for 4 s and
    the model is never asked.
- `raw_peer_test.rs`: a peer written in the test, on 127.0.0.5 and 127.0.0.6, listening on the
  server's own port number.
  - A TTL-64 packet is dropped (single-hop GTSM). Then the handshake runs Down → Init → Up.
  - The Poll Sequence lowering the interval once Up, and a Poll answered with Final (never
    both bits).
  - Silence brings the session Down with `control_detection_time_expired`, and it then
    transmits nothing (passive role).
  - tshark's BFD dissector (the pcap oracle) reads every packet both sides sent.
  - Bounds: an oversized datagram is dropped; `max_sessions` 1 refuses a second peer; a
    declined peer is asked about once.
- `packet_test.rs`: every authentication type signs and verifies, and a wrong key or a
  tampered packet is refused. The decoder's §6.8.6 refusals. The state machine, and the
  meticulous sequence window (replay and too-far-ahead).

Mutation-checked: discarding the model's answer to state changes fails the BIRD session test
at "Up at 0.400".

BIRD's BFD sockets set SO_PRIORITY 7, which needs CAP_NET_ADMIN. Run unprivileged without
it, every BFD socket fails and sessions never leave Down. CI therefore runs
`setcap cap_net_admin+ep` on the bird binary; locally, run as root or do the same.
