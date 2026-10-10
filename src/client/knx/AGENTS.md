# KNX/IP client

One KNXnet/IP tunnel to a gateway (`wire.rs` from the server): CONNECT in NAT mode, a
CONNECTIONSTATE heartbeat every 60 s, DISCONNECT on the way out. `knx_group_write
{group_address, value, dpt?}` and `knx_group_read` go out as L_Data.req; each waits for the
gateway's ack, is retransmitted once after a second, and a second silence ends the tunnel, as
the specification requires; at most 32 queue behind it. Every L_Data.ind is acked and raised as
`knx_telegram {kind: write|read|response, source, destination, dpt, value, interpretations}`,
decoded by `group_types` as on the server. A handler chain from the client's own actions stops
after 8 follow-ups; telegrams other devices put on the bus are new events.
