# VXLAN server tests

No LLM calls; a python policy makes NetGet host 10.99.0.2 on VNI 42.

- `kernel_peer_test.rs` (Linux; root or passwordless sudo, plus iproute2 and ethtool): the
  kernel's vxlan driver in a network namespace (`tests/helpers/netns.rs`), 10.99.0.1/24.
  - `ping` exits 0 only on valid echo replies.
  - `ip neigh` holds the MAC from the model's ARP reply.
  - A python UDP socket in the namespace reads `pong:hello`.
  - 10.99.0.3 has no host, so ping fails.
- `raw_vtep_test.rs` (unprivileged): the test is the remote VTEP on 127.0.0.9, for VXLAN
  **and Geneve**. It runs ARP, ping and UDP, and tshark's vxlan and geneve dissectors (the
  pcap oracle) read every frame. Refusals: the I flag clear, another VNI, a truncated inner
  frame, and a bad IPv4 header checksum are all dropped before the model. An IP with no host
  is silence.

**The namespace's vx0 has tx checksum offload turned off** (`ethtool -K vx0 tx off`). Across
a veth nothing completes a CHECKSUM_PARTIAL inner UDP checksum, so without it NetGet would
(rightly) drop every UDP datagram as having a bad checksum. A physical NIC fills the checksum
in. ICMP is unaffected, because the kernel always checksums it in software.
