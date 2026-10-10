# libp2p client

The server's stack (`src/server/libp2p`) in the dialler role. `remote_addr` is `host:port` or
a multiaddr `/ip4|ip6|dns/…/tcp/…[/p2p/<id>]`; `peer_id` may name the id separately, and both
must agree. A named id is checked in the Noise handshake, and a mismatch fails the connect
naming both ids.

After the upgrade NetGet runs the remote's identify and raises `libp2p_connected` (agent,
protocols, listen and observed addrs). The remote's own identify and ping are answered by
Rust, as on the server.

## Tasks

All are registered with `register_client_task`:

- the yamux reader;
- the yamux writer;
- the inbound-stream acceptor;
- the notes→events converter;
- the dispatcher, which asks the handler;
- the session, which performs actions from the handler and from `send_to_client`.

The connection ending ends the session.

## Actions

- `libp2p_open_stream{protocol, data?}`: answered by `libp2p_response{ok, stream_id}`, or
  `ok: false` when the remote answers "na".
- `libp2p_send{stream_id, data, encoding}`
- `libp2p_close_stream{stream_id}`
- `libp2p_ping`: answered with `rtt_ms`.
- `libp2p_identify`
- `disconnect`

Messages on any stream, opened by either side, arrive as `libp2p_message`. When the remote
half-closes a stream, the writer is kept so the handler can still answer on it.

A handler chain stops after `MAX_FOLLOWUP_DEPTH` (8).
