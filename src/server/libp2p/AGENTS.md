# libp2p server (a host)

libp2p over TCP, well-known port 4001 (IPFS's swarm port). Everything is hand-rolled on
crates already in the tree: ring (ChaCha20-Poly1305), x25519-dalek, ed25519-dalek, sha2 and
hkdf. The client (`src/client/libp2p`) uses the same modules in the dialler role.

## Layers

- `wire.rs`:
  - uvarints, and the protobuf subset libp2p's small messages use;
  - base58btc, and Ed25519 peer ids (an identity multihash of the 36-byte `PublicKey`
    protobuf, so every id starts `12D3KooW`);
  - binary and text multiaddrs: the `/ip4|ip6/…/tcp/…[/p2p/…]` subset, with unknown
    components shown by code, stopping there;
  - multistream-select 1.0;
  - the identify message;
  - `shown`/`data_bytes`: messages as text, or hex with an explicit `encoding`, never sniffed.
- `noise.rs`: `Noise_XX_25519_ChaChaPoly_SHA256` with an empty prologue.
  - Each handshake and transport message is framed by a u16 big-endian length.
  - The handshake payload carries the identity key and its signature over
    `"noise-libp2p-static-key:" ‖ static key`. It is verified before anything else is
    trusted. A dialler that named a peer id checks it, and refuses before sending its own
    message 3.
  - Only Ed25519 identities are accepted; RSA, secp256k1 and ECDSA are refused by name.
- `yamux.rs`: a reader task routes frames to streams; a writer task owns the Noise writer and
  every stream's send window.
  - Data the peer has no window for waits in the writer.
  - Data past the window *we* gave is a protocol error that ends the connection.
  - A stream's receive window is granted back at half of `INITIAL_WINDOW`.
  - At most `MAX_STREAMS` streams are open at once; excess SYNs are reset.
  - Pings are answered. GoAway ends the session.
  - Our half of a stream stays open until closed explicitly, so a reply can follow the peer's
    half-close.
- `host.rs`: upgrading a connection (`/noise`, then `/yamux/1.0.0`), with a 10 s negotiation
  deadline.
  - The peer's streams are served in Rust: identify (our key, listen and observed addrs,
    protocols, `netget/<version>`), identify-push (read and dropped), and ping (32-byte echo).
  - The application protocols' streams become `Note::Message`s: uvarint-length-prefixed
    messages, at most `MAX_MESSAGE` (1 MiB). A longer announcement resets the stream before
    it is read.
  - Stream writers are kept for actions, at most `MAX_REMEMBERED_STREAMS`.

## The model (`mod.rs`)

Each connection has one loop over its notes and the dashboard's injected actions, so the
model answers one event at a time, in arrival order. After the upgrade, NetGet runs the peer's
identify and raises:

- `libp2p_peer_connected {peer_id, remote_addr, agent_version, protocols, listen_addrs}`:
  `libp2p_open_stream`, `libp2p_disconnect`, or nothing.
- `libp2p_message {peer_id, stream_id, protocol, data, encoding}`: `libp2p_send` (defaults to
  the same stream), `libp2p_open_stream`, `libp2p_close_stream`, `libp2p_disconnect`.

When the peer closes a stream, NetGet closes its own half after the queue has drained, so a
reply to the last message still goes out. A model failure resets the stream being answered
(`decision=fail_closed_llm_error`). Silence leaves it open (`model_silent`). The peer has a
handle, so `send_to_peer` and the dashboard's buttons work.

Startup parameters:

- `protocols`: default `host::DEFAULT_PROTOCOLS`, `/netget/chat/1.0.0`.
- `private_key_seed`: any text; its SHA-256 is the identity key, for a stable peer id. The
  name puts it under the redactor.

## Not implemented

QUIC, WebTransport, WebRTC, the TLS security protocol, mplex, early muxer negotiation (Noise
extensions), signed peer records, DHT, gossipsub, relay, AutoNAT, hole punching.
