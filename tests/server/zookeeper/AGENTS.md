# ZooKeeper Server E2E Testing

## Strategy

Two layers, on purpose.

1. **Byte level** (`test_zookeeper_connect_handshake`) — a raw `TcpStream` sends a
   `ConnectRequest` and the test asserts the `ConnectResponse` bytes. Every real client blocks
   on that frame, and a byte-level assertion is the only thing that pins its layout.
2. **Real client** (everything else) — `zookeeper_async::ZooKeeper`, already a dependency of
   the `zookeeper` feature. A passing test means an actual ZooKeeper client completed a
   session and decoded our replies with its own decoder, rather than our encoder agreeing with
   our decoder.

The previous version of this file hand-built request bytes over a raw socket and **never sent a
`ConnectRequest`**, which is exactly why the missing handshake survived. Do not go back to
that: any new test that skips the handshake is testing a code path no client can reach.

## Tests

| Test | Proves | LLM calls |
|---|---|---|
| `test_zookeeper_connect_handshake` | `ConnectResponse` layout, timeout clamping, session resume, and that a pre-handshake request closes the connection | 1 |
| `test_zookeeper_get_data` | a real client completes a session and decodes `zookeeper_data` + its `Stat` | 2 |
| `test_zookeeper_get_children` | a real client decodes `zookeeper_children`, in order | 2 |
| `test_zookeeper_error_response` | `error_code: -101` reaches the client as `ZkError::NoNode` | 2 |

| `peer_inject_test::injected_zookeeper_action_reaches_raw_socket_and_close_sends_eof` | `send_to_peer` of `zookeeper_data` is framed on the socket, a missing `xid` defaults to -1, byte/packet counters match, `close_connection` yields EOF and releases the handle | 0 |

**LLM budget: 7 calls.**

`test_zookeeper_connect_handshake` gets four independent assertions out of a single LLM call by
opening four sockets against one server — the handshake never involves the model, so extra
connections are free.

## What the handshake test asserts

- body length is exactly 37 bytes (`protocolVersion` 4, `timeOut` 4, `sessionId` 8, password
  buffer 4+16, `readOnly` 1);
- `protocolVersion == 0`; `sessionId != 0`; password is 16 bytes and is **not** the zeros the
  client sent;
- a 30 s request is granted unchanged, a 500 ms request is clamped up to 4000 ms;
- a presented session id is echoed back rather than replaced;
- a `getData` frame sent before any `ConnectRequest` gets the connection closed (`read` returns
  0), not a reply.

## xid correlation

The client's xid counter starts at 1 and increments per request, so mocks use
`respond_with_actions_from_event` and echo `event["xid"]` rather than hardcoding a value —
same reason the UDP protocols must echo their transaction ids.

## Session timeout

Tests ask for 30 s. The client pings at `negotiated / 3 * 2` = 20 s, comfortably beyond the
runtime of any test, so no unexpected `ping` traffic appears. Pings are answered in Rust and
raise no event anyway, but a shorter timeout would still add wire noise.

Each test ends with `zk.close()`, which exercises the `closeSession` (opcode -11) path.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features zookeeper \
    --test server -- --test-threads=100 zookeeper
```

Runtime is about 1 second for the whole suite. All traffic is loopback.

## Not covered

- `create` / `setData` / `exists` round-trips (the actions exist and encode, but the request
  bodies are not decoded, so there is nothing meaningful to assert about the input).
- Watches — not implemented.
- Session expiry — no session state is kept.
