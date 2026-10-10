# VXLAN client tests

No LLM calls. `kernel_peer_test.rs` (Linux; root or passwordless sudo, plus iproute2 and
ethtool) runs the kernel's vxlan driver in a network namespace: 10.99.0.1/24 on VNI 42, with a
python UDP service on 9999. NetGet is 10.99.0.2.

1. On `vxlan_ready` the model pings 10.99.0.1. The kernel ARPs for 10.99.0.2 before it
   answers, and NetGet answers that ARP in Rust.
2. On the echo reply the model sends `echo <seq> <data>` to the service and `nobody` to the
   closed port 9998.
3. The service prints what it got and answers `ack:…`, which arrives as
   `vxlan_udp_datagram`. The kernel's port unreachable arrives as `vxlan_icmp_error`.
4. The namespace's own `ping 10.99.0.2` is answered.
5. An injected resolve of an absent IP is Rejected after the ARP wait, with
   `vxlan_unreachable`.

Mutation-checked: discarding the model's actions fails at the first echo reply. vx0's tx
checksum offload is off for the reason given in `tests/server/vxlan/AGENTS.md`.
