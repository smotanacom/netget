# KNX/IP server

A KNXnet/IP **tunnelling** gateway over UDP (port 3671), hand-written (`wire.rs`, shared with
the client): search and description, connect (tunnel connection, link layer; NAT mode when the
HPAI is 0.0.0.0:0), connection state, disconnect, tunnelling requests with acks and sequence
numbers (a repeat of the last request is acked again and not processed, anything else is
dropped), cEMI L_Data.req in, L_Data.con back to the sender and L_Data.ind to every other
tunnel — Rust routes group telegrams between tunnels as a bus would. Tunnels get individual
addresses on the gateway's line (default gateway 1.1.250).

## Values

A group value is never shown to the handler as bytes. `group_types` maps group addresses to
DPTs (1, 5, 5.001, 6, 7, 8, 9, 12, 13, 14, 16, 17, 20), decoding and encoding by them; an
unconfigured address's value is offered under every DPT its size allows (`interpretations`).

## What the handler decides

The handler is every device on the bus. `knx_group_telegram {kind: write|response, source,
destination, dpt, value, interpretations}` → `knx_ignore` or `knx_group_write` (feedback);
`knx_group_read` → `knx_group_response {value, dpt?}` from the gateway's address, or silence.
Responses and writes go to every tunnel.

## Failure modes and bounds

Deliberately silent: a failed or silent handler sends nothing — a read with no answer is what a
real bus gives, and a fabricated value would be a device that does not exist (`decision=` tags
in the log). Frames over 512 bytes and malformed ones are ignored; `max_tunnels` (default 8,
at most 64) refuses with E_NO_MORE_CONNECTIONS; a tunnel silent for 120 s is dropped with a
DISCONNECT_REQUEST; 64 telegrams may wait for the handler. Telegrams NetGet sends are not
retransmitted. Not implemented: routing (multicast), device management, KNX IP Secure,
tunnelling v2 over TCP.
