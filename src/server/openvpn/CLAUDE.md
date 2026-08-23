# OpenVPN — control-plane responder

## What this is

A server that speaks the unauthenticated front half of the OpenVPN UDP protocol. It decodes the real wire format,
answers a client's session reset with a `P_CONTROL_HARD_RESET_SERVER_V2` that a genuine `openvpn` client accepts,
acknowledges the control packets the client sends next, and asks the LLM whether to answer each peer at all.

Useful as a honeypot and protocol observatory: who is probing UDP/1194, with what options, and what their TLS
ClientHello looks like.

**Status**: `DevelopmentState::Experimental` · **Privileges**: `PrivilegeRequirement::None`

## What this is not

**It is not a VPN and never carries traffic.** There is no TLS control channel, therefore no key exchange, therefore no
data-channel keys, no TUN device and no tunnel. A real client gets as far as sending its ClientHello, receives ACKs for
it, then times out waiting for a ServerHello this server cannot produce. Use `src/server/wireguard/` for a real VPN.

| Component                              | Status                                                     |
|----------------------------------------|------------------------------------------------------------|
| Packet parse/serialize (control, ACK, data) | ✅ Spec-correct, validated against a real client       |
| Session reset exchange                 | ✅ A real `openvpn` client accepts our reply                |
| Control-packet ACKs                    | ✅ A real client accepts them and stops retransmitting      |
| LLM accept/reject policy               | ✅ Enforced — only `accept_peer` causes any bytes to be sent |
| TLS control channel                    | ❌ Does not exist                                           |
| Peer authentication                    | ❌ None — the model's decision is the only gate             |
| Key exchange / data channel / TUN      | ❌ Removed, see below                                       |
| `--tls-auth` / `--tls-crypt` / `-v2`   | ❌ Detected and refused rather than mis-parsed              |

## What was wrong before, and what changed

This protocol was rated Stable, then demoted to `Incomplete` on inspection because it had never been run against a real
client. Driving OpenVPN 2.7.4 against it showed three separate problems.

### 1. The reset reply had its fields in the wrong order

The old `PacketHeader::serialize` wrote the message packet id **before** the ACK array and the peer session id. The real
layout is:

```text
u8       opcode << 3 | key_id
u64      sender's session id
u8       ACK count n
u32 * n  acknowledged packet ids
u64      peer session id      -- present only when n > 0
u32      message packet id    -- absent for P_ACK_V1
```

So no `openvpn` client could ever parse the reply. Three further codec bugs sat behind it:

- The 8-byte session id was read only for the `*_V2` opcodes. It is present on **every** control and ACK packet,
  so `P_CONTROL_V1` and `P_ACK_V1` were mis-parsed.
- The peer session id was sniffed for with `if buf.len() >= 8`, which eats eight bytes of TLS payload when it is absent
  and misses it when the remaining payload is short.
- `P_ACK_V1` was given a message packet id, which it does not have, and `P_DATA_V2` was given an 8-byte session id
  where the protocol has a 24-bit peer id.

The reply now emitted is byte-identical to a frame OpenVPN 2.7.4 accepted; see `tests/server/openvpn/`.

### 2. The data channel was decorative and unreachable

`initialize_data_channel()` derived every peer's AES-256-GCM/ChaCha20-Poly1305 key with HKDF over three string literals
committed to this repository, so every peer on every installation shared one key that anyone could reproduce from the
source. It was also unreachable: without a key exchange no peer can legitimately be keyed, and no real client ever got
past the broken reset reply anyway.

The TUN device and that call site are gone. Data packets are now parsed, counted and dropped with an explicit log line
saying no key exchange has taken place. `crypto.rs` is kept, unwired, as the piece a future key exchange would plug
into — its module docs say so, and say not to call `derive_data_keys` with constants.

### 3. Root was claimed for something that no longer exists

The TUN device was the only reason for `PrivilegeRequirement::Root`, and `server_startup` gates on it, so the protocol
could not start — or be tested — on an ordinary machine. With no TUN device, nothing here needs elevation and the
declaration is now `None` (port 1194 is unprivileged).

## Library choices

None. Custom implementation — no viable Rust OpenVPN *server* library exists (`openvpn-parser` is read-only and
unmaintained; `libopenvpn3` FFI is client-only).

### Declared dependencies that are not used

The `openvpn` feature in `Cargo.toml` declares more than the code needs. Cargo.toml is shared, so this is recorded
rather than edited:

- `tokio-rustls` — never referenced by this protocol, before or after this change.
- `hmac` — never referenced; `crypto.rs` uses `hkdf` + `sha2` only.
- `tun` — no longer referenced (still used by `src/server/wireguard/`, so the dependency itself is live).
- `rustls` / `rcgen` — the old `create_tls_config()` built a `ServerConfig` that was stored in a field and never used.
  That function is gone, so neither is referenced by this protocol any more.
- `aes-gcm`, `chacha20poly1305`, `hkdf`, `sha2` — used by `crypto.rs`, which is compiled but not wired into the server.

## Architecture

`mod.rs` binds one UDP socket. The receive loop and an idle sweep run inside a single task joined by `tokio::select!`,
because `register_server_task` stores exactly one handle per server — registering two would silently drop the first and
leak it past `stop_server`.

Peer decisions are dispatched to a spawned task so an LLM call does not stall the receive loop.

### Connection flow

```text
client ──> P_CONTROL_HARD_RESET_CLIENT_V2
                    │  peer recorded as Deciding
                    │  openvpn_peer_reset raised
                    ├─ accept_peer  ──> P_CONTROL_HARD_RESET_SERVER_V2
                    ├─ reject_peer  ──> nothing is sent
                    └─ no decision  ──> nothing is sent (fails closed)
client ──> P_CONTROL_V1 (TLS ClientHello)
                    └─ ACKed; payload logged and dropped
client ──> P_DATA_V1/V2
                    └─ dropped: no keys exist and none can
```

### Retransmission

A client retransmits its reset every couple of seconds until it hears back, which is faster than an LLM decision. The
peer is inserted as `Deciding` **before** the model is consulted, so one handshake costs at most one model call:
retransmits arriving mid-decision are dropped, retransmits after acceptance get the cached reply bytes verbatim, and
retransmits from a rejected peer stay unanswered. A reset from a known address bearing a *different* session id is
treated as a restart and starts over.

### Peer table

`MAX_PEERS` (100) entries; a sweep every 30s drops peers idle for more than 120s and closes their connection in
`AppState`, so a scan cannot pin every slot indefinitely.

## LLM integration

One event, `openvpn_peer_reset`, raised once per handshake. Control packets after the reset are ACKed without consulting
the model: clients retransmit them, so an event per packet would spend model calls on duplicates while changing nothing
the server is able to do.

Two sync actions, both enforced:

- **accept_peer** — send `P_CONTROL_HARD_RESET_SERVER_V2` and start tracking the peer. The only thing that causes any
  byte to be sent.
- **reject_peer** — send nothing and drop the peer. OpenVPN has no reject packet at reset time, so silence is the
  refusal.

`execute_action` returns `ActionResult::Custom { name: PEER_DECISION_RESULT, data: { accept, reason } }`, which the
server loop reads. **Fails closed**: a rejection, an empty answer, and an LLM error all leave the peer unanswered, and
all three are logged distinctly so an outage can never be mistaken for approval.

### Why a backend failure sends nothing

Before the TLS control channel exists, OpenVPN has exactly one server-to-client message,
`P_CONTROL_HARD_RESET_SERVER_V2`, and sending it **is** admitting the peer. There is no NAK and no error packet;
`AUTH_FAILED` is a push message that only exists once TLS is up, which this server never reaches. A real OpenVPN server
drops what it will not admit — that is what an HMAC failure under `--tls-auth` does. So the usual fix of answering the
peer with a category instead of silence does not apply here: the only available reply means "you are in", and emitting
it on an LLM outage would be the fail-open bug rather than a cure for it.

The distinction the peer cannot be given is given to the operator instead. Every outcome carries a stable `decision=`
token, matching `src/server/radius/`:

| Token | Meaning |
|---|---|
| `decision=model_accept` | the model called `accept_peer`; the reset reply was sent |
| `decision=model_reject` | the model called `reject_peer`; nothing sent, by the model's choice |
| `decision=fail_closed_no_action` | the model answered with neither action; nothing sent |
| `decision=fail_closed_llm_error` | the LLM call itself errored; nothing sent |

`grep 'decision=fail_closed_'` finds every peer the model did not actually answer for. The LLM-error line also carries
`class=overloaded` / `class=unavailable` from `WireFailure::classify` — the distinction a protocol with two error codes
would have put on the wire — and the full error text, which stays in the log and the status stream and never touches
the socket.

No async actions: the executor builds a stateless `OpenvpnProtocol` with no handle to the running server, so anything
listed there could only return `NoAction`.

## Storage

None. Peer state is in-memory transport state — no database, no filesystem, no persistence.

## Robustness

Everything the parsers see arrives from a UDP socket. Every field is length-checked before it is read and no parse path
uses `unwrap()` on input-derived data; a panic in the receive loop would be silent while the server kept reporting
`Running`. `tests/server/openvpn/codec_test.rs` fuzzes both parsers with 20,000 pseudorandom byte strings.

## Future work (large — not a patch)

Making this a real VPN needs the entire control channel: a genuine TLS session over the OpenVPN reliability layer
(retransmission windows, ACK windows, fragmentation), key-method-2 material exchange inside it, per-session key
derivation, `PUSH_REQUEST`/`PUSH_REPLY`, then the data channel and a TUN device. That is a project in its own right and
is deliberately out of scope. Anything less than all of it produces a server that still cannot carry traffic — which is
what this protocol was before.

## References

- [OpenVPN protocol overview](https://openvpn.net/community-resources/openvpn-protocol/)
- [OpenVPN source](https://github.com/OpenVPN/openvpn) — `ssl_pkt.c` for the control-channel layout
