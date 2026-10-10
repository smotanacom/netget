# libp2p server tests

No LLM calls: a python policy is the model. The peer is **go-libp2p** v0.41.1 (`peer/`,
go.sum-pinned, built by `install_peers.py`, named by `NETGET_LIBP2P_GO_PEER`). It runs TCP,
Noise and yamux only, so every byte is go-libp2p's negotiation, encryption and multiplexing.
The tests fail rather than skip without it.

The server runs with a fixed `private_key_seed`, so the test knows its peer id.

## `go_libp2p_dials_netget`

go-libp2p dials, verifies NetGet's peer id, and reports what its identify service parsed: agent
`netget/…`, protocol version, protocols and listen addrs. Then:

1. It pings.
2. It talks two messages on `/netget/chat/1.0.0` (echoed by the policy).
3. A protocol NetGet does not offer is refused.
4. A message announcing 2 MiB resets its stream.

When the peer connected, the policy opened a stream of its own. go's echo handler printed
what arrived on it, and NetGet saw the echo come back.

## The other tests

- `a_wrong_peer_id_is_refused_by_the_dialler`: go dials NetGet under another valid peer id,
  and go-libp2p refuses NetGet's proof.
- `a_failed_handler_resets_the_stream`: with an unreachable model, go's read ends in a stream
  reset rather than a wait.
- `multistream_refusals_over_raw_tcp`: hand-written multistream bytes.
  - `/tls/1.0.0` gets `na`.
  - Past `MAX_PROPOSALS` the connection closes.
  - A message announcing more than `MAX_MULTISTREAM_MESSAGE` closes it unread.
- `base58_and_varint_round_trip`
