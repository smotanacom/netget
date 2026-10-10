# libp2p client tests

No LLM calls: a python chain is the model. The peer is go-libp2p listening (`libp2p-peer
listen`, see `tests/server/libp2p`). It prints what it sees, opens a chat stream of its own to
NetGet once identify is done, and echoes what NetGet sends.

## The chain

1. On connect, NetGet opens `/netget/chat/1.0.0` with "hello from netget".
2. When the stream is open, it pings.
3. Any message that is not an echo, it echoes.

## What is asserted

From go-libp2p's side:

- it identified NetGet (agent, protocols);
- it received NetGet's hello;
- NetGet answered the stream go opened.

From NetGet's side:

- the go agent, and a peer id equal to the multiaddr's;
- the echo of its hello;
- a successful ping;
- go's message, on an even (listener-opened) stream.

Injected actions:

- identify, answered with go's protocols;
- an unsupported protocol, answered `ok: false`;
- an unknown stream, rejected locally.

`a_wrong_peer_id_fails_the_connect` dials go under another peer id and expects the connect to
fail naming the mismatch.

Mutation-checked: discarding the model's actions fails the chain.
