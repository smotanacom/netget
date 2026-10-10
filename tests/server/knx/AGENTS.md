# KNX/IP server tests

`BUS_SCRIPT` (in `wire_test.rs`): 1/2/4 reads 21.5 °C (DPT 9.001 via `group_types`), 1/2/5 a
DPT 16 text; a write to 1/2/3 is fed back on 1/2/10; other reads go unanswered.

- `wire_test.rs`, raw frames with NetGet's codec: two tunnels with distinct channels and
  addresses; a write acked, confirmed with the tunnel's address, heard by the other tunnel,
  and its feedback reaching both; reads answered by DPT; a repeated sequence number acked
  again without reprocessing; heartbeat and disconnect; the tunnel cap, a refused connection
  type, oversized and malformed frames; an unanswered read and an unreachable model (nothing
  after the confirmation).
- `real_client_test.rs`: xknx 3.20 (`xknx_peer.py`) switching, reading both values, reading an
  unanswered address and hearing the feedback; knxd connecting to NetGet as its tunnelling
  uplink (`-b ipt:`), driven by knxtool, whose listener hears the feedback and the response.

Peers from `install_peers.py`. No LLM calls.
