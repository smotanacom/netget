# VXLAN / Geneve client (a host on the overlay)

One host (`overlay_ip`, `overlay_mac`) behind a tunnel to one remote VTEP (`remote_addr`),
on one `vni`. It uses the server's framing (`src/server/vxlan/frame.rs`).

## Sockets

It binds `local_address:<tunnel port>`, because the remote VTEP sends there. It sends
encapsulated frames from that socket to `remote_addr`. `local_address` defaults to the source
of the route to the remote.

## What runs in Rust

Like any host's stack: ARP and echo requests for `overlay_ip` are answered without the model,
and the ARP cache learns from replies and from requests aimed at us.

## What the model does

- `vxlan_resolve{ip}`, `vxlan_ping{ip, data}` and `vxlan_send_udp{ip, port, source_port, data,
  encoding}`.
- These wait on ARP when the MAC is unknown: one request per IP, at most `MAX_PENDING` (32)
  waiting, for `ARP_TIMEOUT` (2 s). After that the action is reported as
  `vxlan_unreachable`, and an injected caller gets `Rejected`.
- Events: `vxlan_ready`, `vxlan_arp_resolved`, `vxlan_icmp_echo_reply` (with `rtt_ms` for
  our own pings), `vxlan_udp_datagram`, and `vxlan_icmp_error` (e.g. `port_unreachable`
  with the original destination port).
- Chains are bounded at `MAX_FOLLOWUP_DEPTH` (8).

Only frames from the remote VTEP's IP on our VNI are considered.
