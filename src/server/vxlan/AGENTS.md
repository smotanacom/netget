# VXLAN / Geneve server (tunnel endpoint)

A VTEP whose overlay hosts the model plays. VXLAN (RFC 7348, port 4789) or Geneve
(RFC 8926, port 6081, options skipped), chosen by `encapsulation`. All framing is hand-rolled
in `frame.rs`, shared with the client:

- Ethernet and ARP.
- IPv4: the header checksum is checked, and fragments are reported but not reassembled.
- ICMP echo and errors.
- UDP: checksums are checked on input and written on output.

## Who answers

The model gets three events, each with `vni`, `vtep` and `src_mac`:

- `vxlan_arp_request` → `vxlan_arp_reply{mac}` (default `overlay_mac`). The MAC is
  remembered as that IP's, and used as the source of its later frames.
- `vxlan_icmp_echo_request` → `vxlan_icmp_echo_reply` (identifier, sequence and data
  mirrored).
- `vxlan_udp_datagram` → `vxlan_udp_reply{data, encoding}`, from the port it was sent to,
  back to the sender.

The frame is built from the request in `VxlanProtocol::for_frame`; the registry instance only
validates. Everything else that decodes (IPv6, TCP, other ICMP, ARP replies) is logged at
DEBUG and dropped. Malformed datagrams and frames are dropped with a WARN. With `vni` set,
other VNIs are dropped.

Replies go to the sending VTEP's IP **at the server's port**, not to the datagram's source
port. VTEPs listen on the port they send to; Linux sends from a hash-chosen source port.

**Deliberately silent.** An IP with no host never answers. A backend failure is the same
silence, with `decision=fail_closed_llm_error` in the log.

## Not implemented

- TCP, IPv6, IP fragmentation, VLAN tags.
- MAC learning, and flooding to other VTEPs: one answering endpoint, not a switch.
- Geneve options (skipped on input, none sent).
- Interop with a kernel Geneve peer is unverified here: the test kernel has no geneve module.
  tshark's dissector checks the framing.
