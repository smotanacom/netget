# OpenVPN — control-channel server

## What this is

A server that speaks the OpenVPN UDP **control channel**: the wire format, the reliability layer underneath it, a real
TLS session whose records are carried inside `P_CONTROL_V1` packets, and the key-method-2 exchange that follows the
handshake. A genuine `openvpn` 2.7 client completes its TLS handshake against this server, sends its options string and
its `--auth-user-pass` credentials, and is answered — or not, as the model decides.

```text
client ──> P_CONTROL_HARD_RESET_CLIENT_V2
server ──> P_CONTROL_HARD_RESET_SERVER_V2        (only if the model calls accept_peer)
client ──> P_CONTROL_V1 * n   TLS ClientHello, fragmented
server ──> P_CONTROL_V1 * n   ServerHello, certificate, CertificateVerify, Finished
client ──> P_CONTROL_V1       key method 2: key material, options, username, password, IV_*
server ──> P_CONTROL_V1       key method 2 answer (only if the model calls accept_key_exchange)
client ──> P_CONTROL_V1       PUSH_REQUEST                                <-- stops here, forever
```

**Status**: `DevelopmentState::Experimental` · **Privileges**: `PrivilegeRequirement::None`

## What this is not

**It is not a VPN and never carries traffic.** `PUSH_REQUEST` is never answered, no data-channel keys are derived from
the exchanged key material, there is no TUN device, and every `P_DATA_*` packet is dropped. A real client reaches
`Peer Connection Initiated`, asks for its configuration, retransmits that request until it gives up, and never logs
`Initialization Sequence Completed`. Use `src/server/wireguard/` if you want a tunnel.

What it *is* good for is exactly what a real control channel gives you without one: because the TLS session is genuine,
the server learns the client's OpenVPN version and platform (its `IV_*` peer info), the options it expects, and **the
username and password it was going to authenticate with**.

| Component | Status |
|---|---|
| Packet parse/serialize (control, ACK, data) | ✅ Spec-correct, validated against a real client |
| Reliability layer (packet ids, ACK arrays, ordering, retransmission) | ✅ Implemented; a real client's multi-packet handshake completes |
| Session reset exchange | ✅ A real `openvpn` client accepts our reply |
| TLS control channel | ✅ Real rustls session over `P_CONTROL_V1`; OpenVPN 2.7.6 negotiates TLS 1.3 and logs `VERIFY OK` |
| Key method 2 (read) | ✅ Options, username, password and `IV_*` peer info are parsed |
| Key method 2 (answer) | ✅ A real client accepts it and logs `Peer Connection Initiated` |
| LLM accept/reject policy | ✅ Enforced at both stages — only an explicit accept sends anything |
| `PUSH_REQUEST` / `PUSH_REPLY` | ❌ Logged and ignored; this is where a real client stalls |
| Data channel key derivation | ❌ None. The key material is read and discarded |
| Data channel / TUN device | ❌ None |
| Peer authentication | ❌ None — the model's decision is the only gate; no client certificate is requested |
| `--tls-auth` / `--tls-crypt` / `-v2` | ❌ Detected and refused rather than mis-parsed |

## Why the rating is still Experimental

`Beta` in this repo means "human-reviewed, works against real clients". A real client now gets much further than it
did — through TLS and through the key exchange, driven by the protocol's own `openvpn` binary in a test that is not
`#[ignore]`d — but it still cannot *use* this as a VPN. The exchange it completes ends in a timeout.

This is deliberately the lesson from `wireguard`'s demotion recorded in the root `CLAUDE.md`: rate the protocol by what
a real peer can actually do with it, not by how much of it is implemented. When `PUSH_REPLY` and a data channel exist
and a client reports `Initialization Sequence Completed`, revisit the rating — and not before.

## Architecture

Five modules, split so the parts that are hard to get right can be tested without a socket:

| File | What it owns |
|---|---|
| `packet.rs` | The wire format: `ControlFrame`, `DataFrame`, opcodes. No state. |
| `reliable.rs` | `ReliableSender` / `ReliableReceiver`. Packet ids, ACK arrays, in-order delivery, retransmission with backoff, fragmentation. Holds no socket and no session ids. |
| `tls_channel.rs` | The rustls `ServerConfig`, the per-run self-signed certificate, and its fingerprint. |
| `keymethod.rs` | Parsing and building key-method-2 messages. Pure functions. |
| `session.rs` | `ControlSession`: one peer's reliability state + `rustls::ServerConnection` + key-exchange state. `SessionManager` owns the table. |
| `mod.rs` | The UDP socket, the three loops, and the two LLM decisions. |

`mod.rs` binds one UDP socket. The receive loop, the idle sweep and the **retransmission timer** run inside a single
task joined by `tokio::select!`, because `register_server_task` stores exactly one handle per server — registering
three would silently drop two and leak them past `stop_server`.

### The reliability layer

OpenVPN's control channel is a reliable layer over UDP, and TLS assumes that reliability. Without it nothing beyond the
reset works: a handshake spread over several datagrams desynchronises permanently on the first loss or reorder.

* **Sender.** Each outgoing control packet gets the next id, is held until acknowledged, and is retransmitted with
  exponential backoff (1s → 8s, 8 attempts, then the session is declared dead). A retransmission is **byte-identical**:
  the ACK array is fixed when the packet is queued, because a peer that sees two different frames carrying one packet id
  has to guess which it already processed. At most `SEND_WINDOW` (4) packets are in flight — OpenVPN's own
  `RELIABLE_CAPACITY` is 12 and anything past a peer's buffer is dropped silently, which looks like a dead server.
* **Receiver.** Payloads are delivered to TLS strictly in packet-id order; anything early is buffered. A duplicate is
  **acknowledged again but not delivered twice** — the peer only retransmits because it missed the first ACK. A packet
  outside the window is neither acknowledged nor buffered, so the peer keeps it and sends it again later.
* **ACK arrays** carry at most `MAX_ACK_ARRAY` (4) ids. OpenVPN's `reliable_ack_parse` *rejects* a frame with more than
  `RELIABLE_ACK_SIZE` (8), so this is a correctness bound, not a tuning knob.
* **Acknowledgements are sent standalone**, as `P_ACK_V1`, and always before anything else in the same flush. They are
  not piggybacked on outgoing control packets. Piggybacking would save datagrams and complicate the "a retransmission is
  byte-identical" rule; the extra ACK datagrams are free on any real link.

Everything in `reliable.rs` is transport-free and unit-tested directly in `tests/server/openvpn/codec_test.rs`, because
none of these properties can be provoked over a loopback socket that never loses anything — and every one of them is
silently fatal to a TLS handshake if wrong.

### The TLS control channel

`rustls::ServerConnection` is used **directly**, not through `tokio-rustls`: there is no `AsyncRead`/`AsyncWrite` to
wrap, since the records arrive inside UDP packets. Payloads go in with `read_tls` + `process_new_packets`, records come
out with `write_tls` and are fragmented by `reliable::fragment` into `P_CONTROL_V1` packets of at most
`MAX_CONTROL_PAYLOAD` (1100) bytes. OpenVPN 2.7 reports `tls_mtu = 1250`, so 1100 leaves room for the control header.

The provider is named explicitly (`rustls::crypto::ring`, which the `openvpn` feature enables) rather than taken from
the process default: `ServerConfig::builder()` panics when zero or several providers are installed, and this binary can
link more than one. `src/bin/netget.rs` already lists `openvpn` in its `CryptoProvider::install_default` gate — that was
checked, and `tests/rustls_provider_gate_test.rs` keeps it honest — but naming the provider here means the server does
not depend on that having happened.

**Certificate**: a fresh self-signed P-256 certificate per server run, generated with `rcgen`. Nothing is written to
disk and there is no shipped key. A client trusts it OpenVPN 2.6+'s documented way for self-signed setups —
`--peer-fingerprint` — and the server logs the SHA-256 in exactly that spelling at startup:

```text
OpenVPN control channel peer fingerprint SHA256=AB:CD:...:EF
```

No client certificate is requested. This server authenticates nobody at the TLS layer; what the peer sends afterwards is
reported to the model, which decides.

### Key method 2

Both sides write one plaintext message into the TLS session as soon as the handshake finishes:

```text
u32       0                  -- literal
u8        key method (2)
[48]      pre-master secret  -- CLIENT ONLY; a server omits it
[32]      random1
[32]      random2
string    options string
string    username
string    password
string    peer info          -- IV_* variables
```

Two encoding details that a wrong implementation gets past the type checker and not past a real client:

1. A `string` is a `u16` length followed by that many bytes, and **the length counts a trailing NUL** which is part of
   the payload.
2. `write_empty_string` emits a length of **zero and no bytes at all**. That is not the same as a string containing one
   NUL, and it is what a server with no `--auth-user-pass-verify` sends for username, password and peer info.

The answer mirrors the client's options string with `tls-client` flipped to `tls-server`, so the client's OCC comparison
warns about nothing. The key material we send is fresh random bytes and **nothing is derived from it** — the data
channel that would consume it does not exist, and `crypto.rs` stays unwired for that reason.

`parse_client_key_method_2` distinguishes *incomplete* (`Ok(None)`) from *invalid* (`Err`). The control channel is a
byte stream across several `P_CONTROL_V1` packets, so a prefix means "wait", and treating it as an error would kill
every session whose key exchange spans two packets.

## LLM integration

Two events, one per policy decision. Individual control packets are acknowledged without consulting the model: clients
retransmit them, so an event per packet would spend model calls on duplicates.

| Event | Actions | What accepting does |
|---|---|---|
| `openvpn_peer_reset` | `accept_peer` / `reject_peer` | Sends `P_CONTROL_HARD_RESET_SERVER_V2` and starts a control session |
| `openvpn_client_key_exchange` | `accept_key_exchange` / `reject_key_exchange` | Sends the server's key-method-2 message |

`openvpn_client_key_exchange` carries the username, password, options string and parsed `IV_*` peer info. It does **not**
carry `pre_master`, `random1` or `random2`: those are secrets, and no decision can be made from 112 random bytes.

`execute_action` returns `ActionResult::Custom` under two distinct names — `openvpn_peer_decision` and
`openvpn_key_exchange_decision` — so a decision about one stage can never be read as a decision about the other.

No async actions: the executor builds a stateless `OpenvpnProtocol` with no handle to the running server, so anything
listed there could only return `NoAction`.

### Why a backend failure sends nothing, at both stages

**Reset.** Before TLS, OpenVPN has exactly one server-to-client message, `P_CONTROL_HARD_RESET_SERVER_V2`, and sending
it **is** admitting the peer. There is no NAK and no error packet. A real OpenVPN server drops what it will not admit —
that is what an HMAC failure under `--tls-auth` does. Answering on backend failure would be the fail-open bug, not a fix
for it.

**Key exchange.** `AUTH_FAILED` exists, but a client only looks for control-channel messages **after** it has read the
server's key-method-2 answer; sent before that, the client parses it *as* that answer and reports a protocol error
rather than a rejection. So there is still no refusal message the peer would understand, and sending our key material in
order to say "no" would move the fail-open bug rather than remove it.

The distinction the peer cannot be given is given to the operator instead. Every outcome carries a stable `decision=`
token, matching `src/server/radius/`:

| Token | Meaning |
|---|---|
| `decision=model_accept` | the model accepted; the reply was sent |
| `decision=model_reject` | the model refused; nothing sent, by the model's choice |
| `decision=fail_closed_no_action` | the model answered with neither action; nothing sent |
| `decision=fail_closed_llm_error` | the LLM call itself errored; nothing sent |
| `decision=fail_closed_no_control_channel` | the TLS session could not be created; nothing sent |
| `decision=fail_closed_no_session` | the session was gone before the answer could be written |
| `decision=fail_closed_write_error` | the answer could not be queued on the TLS session |

`grep 'decision=fail_closed_'` finds every peer the model did not actually answer for. The LLM-error lines also carry
`class=overloaded` / `class=unavailable` from `WireFailure::classify` — the distinction a protocol with two error codes
would have put on the wire — and the full error text, which stays in the log and never touches the socket.

### Credentials in the log

The username is logged at INFO; the password only at DEBUG, and both go to the model in the event. Capturing them is the
point of running this as a honeypot, but they are still credentials and they never reach the wire.

## Locking

`SessionManager::with` hands out `&mut ControlSession` inside a **synchronous** closure and nothing else. The closure
cannot await, so the lock is never held across an `await` that does I/O or an LLM call. Every session method returns the
datagrams to transmit rather than transmitting them; `mod.rs::flush_peer` copies them out, drops the guard, and only
then touches the socket.

`Peer` (in `peer.rs`) stays cloneable transport bookkeeping. `rustls::ServerConnection` cannot be cloned and must never
be snapshotted — two copies of a TLS session are two divergent record streams — which is why the non-cloneable half
lives in `session.rs` instead.

## Peer table

`MAX_PEERS` (100) entries; a sweep every 30s drops peers idle for more than 120s, removes their control session and
closes their connection in `AppState`, so a scan cannot pin every slot indefinitely. A session whose packets go
unacknowledged through all 8 retransmissions is dropped by the retransmission loop.

## Storage

None. Peer and session state is in-memory transport state — no database, no filesystem, no persistence. The key material
the client sends is parsed, offered to the model as *metadata about* the exchange (never the secrets themselves), and
dropped with the session.

## Library choices

- **rustls 0.23** (`ring` provider) for the control-channel TLS session, used synchronously.
- **rcgen 0.14** for the per-run self-signed certificate.
- **sha2** for the `--peer-fingerprint` digest.
- Everything else is a custom implementation: no viable Rust OpenVPN *server* library exists (`openvpn-parser` is
  read-only and unmaintained; `libopenvpn3` FFI is client-only).

### Declared dependencies that are not used

The `openvpn` feature in `Cargo.toml` still declares more than the code needs. Cargo.toml is shared, so this is recorded
rather than edited:

- `tokio-rustls` — not referenced. The control channel needs `rustls` synchronously, not an async stream wrapper.
- `hmac` — not referenced; `crypto.rs` uses `hkdf` + `sha2` only.
- `tun` — not referenced (still used by `src/server/wireguard/`, so the dependency itself is live).
- `aes-gcm`, `chacha20poly1305`, `hkdf` — used by `crypto.rs`, which is compiled but not wired into the server, because
  no data-channel keys are derived.

## Robustness

Everything the parsers see arrives from a UDP socket. Every field is length-checked before it is read and no parse path
uses `unwrap()` on input-derived data; a panic in the receive loop would be silent while the server kept reporting
`Running`. `tests/server/openvpn/codec_test.rs` fuzzes both frame parsers with 20,000 pseudorandom byte strings.

A TLS session that fails takes down only that peer: the alert rustls produced is still transmitted, the session stops
being fed, and the idle sweep collects it.

## Future work

Making this a real VPN still needs, in order: `PUSH_REQUEST`/`PUSH_REPLY`, data-channel key derivation from the
exchanged key sources (TLS-PRF over `pre_master`/`random1`/`random2` from both sides), the `P_DATA_V2` AEAD path, and a
TUN device with the privilege declaration that implies. `crypto.rs` is the piece the third step would plug into — **do
not call `derive_data_keys` with constants**, which is what an earlier version of this protocol did, giving every peer
on every installation the same key.

## References

- [OpenVPN protocol overview](https://openvpn.net/community-resources/openvpn-protocol/)
- [OpenVPN source](https://github.com/OpenVPN/openvpn) — `ssl_pkt.c` for the control-channel layout, `reliable.c` for
  the reliability layer, `ssl.c` for `key_method_2_write` / `key_method_2_read`
