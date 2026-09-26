# WebRTC Server E2E Tests

## What these tests prove

The previous suite here asserted that the NetGet process started. It could not have failed
for any WebRTC reason, and it did not call `verify_mocks()`, so its four mocked event rules
were never checked — for events that could not fire in the first place.

This suite establishes a **real peer connection**. The peer is webrtc-rs itself
(`RTCPeerConnection` + `RTCDataChannel`), not a mock: it creates a data channel, produces a
fully-gathered SDP offer, exchanges SDP with NetGet over the server's built-in WebSocket
signalling endpoint, completes ICE, DTLS and SCTP, and then a message is asserted to have
**arrived** on the other side. That last assertion is the whole point of the file.

**LLM budget: 9 mocked calls across 4 tests.** No Ollama required. Every test finishes with
`server.verify_mocks().await?`.

Runtime: ~1s for the file (measured, mock mode).

## Tests

### 1. `test_webrtc_data_channel_message_round_trip` — the load-bearing one
**LLM calls: 4** (startup, offer decision, peer connected, message received)

1. Peer builds an `RTCPeerConnection`, creates the data channel `netget`, and gathers.
2. Peer opens a WebSocket to the server's port and sends `{"type":"offer", ...}`.
3. Mock LLM answers the `webrtc_offer_received` event with `accept_offer`.
4. Server returns an `answer` frame; the test asserts it is a real answer carrying
   `m=application`, and applies it to the peer.
5. Data channel opens on both sides. The peer sends `"ping from peer"` from its `on_open`.
6. Mock LLM answers `webrtc_peer_connected` with `send_message "welcome e2e-peer"` and
   `webrtc_message_received` with `send_message "pong from netget"`.
7. **The assertion:** the peer's `on_message` receives exactly `["pong from netget",
   "welcome e2e-peer"]`. Receiving the pong also proves the server received the peer's
   "ping from peer", since nothing else could have produced it.

If the transport does not come up, no message arrives and the test fails on a 30s timeout
with a message saying so. There is no path through this test that passes without bytes
crossing the data channel.

### 2. `test_webrtc_offer_rejected_by_model`
**LLM calls: 2.** The model answers `reject_offer` with a reason; the peer must receive
`{"type":"rejected", "peer_id":"stranger", "reason": "...guest list..."}` and no answer.

### 3. `test_webrtc_offer_without_decision_is_refused` — fail-closed
**LLM calls: 2.** The model returns a well-formed reply containing `append_to_log` and no
admission decision. The offer must still be **refused**, with a reason naming the missing
decision. This is the OAuth2-shaped defect guarded against: model silence must never read as
approval.

### 4. `test_webrtc_malformed_signalling_is_rejected_without_panic`
**LLM calls: 1** (startup only — none of these frames reaches the model, which is itself part
of the point). Sends, over one socket: non-JSON text, an `answer` frame from the peer, an
offer whose SDP does not parse, and an offer with a whitespace-only `peer_id`. Each must
produce an `error` frame; the socket must stay usable throughout; the server must not log a
panic.

### 5. `inbound_limit_test.rs` — the declared `max_inbound_bytes`

In-process on `tests/helpers/inbound_limit.rs`, with its hand-written RFC 6455 client (`ws`):
an `offer` of exactly `SIGNALLING_MAX_MESSAGE_BYTES` (a minimal SDP plus a padding field serde
ignores) reaches the model; a frame header declaring one byte more, with no payload, is
answered with a 1009 close and the connection ends, with zero model calls; a fresh
connection's offer is still put to the model. Verified by removal twice: with the
`WebSocketConfig` limits set to `None` the server waited for the payload (no close), and
without the 1009 send the close carried no code. Data-channel messages are bounded inside
webrtc-rs and are not driven here.

## Mock pattern

Startup is matched with `.on_instruction_containing("webrtc")` rather than `.on_any()`, so
the startup rule cannot swallow an event call and mask a missing event rule.

```rust
mock.on_instruction_containing("webrtc")
    .respond_with_actions(json!([{ "type": "open_server", "port": 0, "base_stack": "webrtc", ... }]))
    .expect_calls(1)
    .and()
    .on_event("webrtc_offer_received")
    .and_event_data_contains("peer_id", "e2e-peer")
    .respond_with_actions(json!([{ "type": "accept_offer" }]))
    .expect_calls(1)
    .and()
```

`"port": 0` is deliberate: the harness learns the real port from the server's
`... listening on 127.0.0.1:PORT` status line. Do not reformat that line without checking
`tests/helpers/netget.rs::parse_server_startup` and its "listening on" fallback — the port
is scraped from it.

## Environment prerequisites

- **A non-loopback network interface must exist.** webrtc-ice omits loopback from host
  candidates and 0.11 has no option to include it, so a host with only `lo` cannot complete
  ICE. This is a crate property, not a bug in the server.
- **No external endpoints are contacted.** The server is started with no `ice_servers`, so
  there is no STUN/TURN traffic; ICE connectivity checks go only to the peer's own local
  addresses, and signalling is `127.0.0.1`.
- mDNS is at webrtc-rs's default (`QueryOnly`). Failure to open the mDNS socket is
  non-fatal in the crate, so a sandbox that blocks multicast does not break the tests.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features webrtc \
    --test server -- --test-threads=100 server::webrtc::e2e_test

# see the SDP and the data-channel traffic
... -- --nocapture --test-threads=100 server::webrtc::e2e_test
```

Filtering on plain `webrtc` also pulls in `server::webrtc_signaling`, which is a different
protocol with its own (currently failing, unrelated) suite. Use `server::webrtc::` to scope
to this one.

## Connection bounds (`connection_bounds_test.rs`)

In-process and model-free (0 LLM calls). Four tests: the cap (256 upgrades, the 257th gets
503, closing one frees one slot); a raw upgraded peer that never writes is sent the
`netget-keepalive` Ping and then Close 1001 at `idle_timeout_secs` (2s here); a tokio-tungstenite
client that sends nothing but its automatic Pongs is kept for five bounds and pinged at least
twice; an offer whose decision is parked for a human by a `manual` rule keeps its signalling
connection and is sent nothing — no Ping, no Close — for four bounds, and once the intercept is
answered (`reject_offer`) the connection gets a fresh bound rather than being closed at once.
The offer is a minimal
SDP (`v=0`, `o=`, `s=`, `t=`) that parses, which is what gets it past `parse_offer` to the
decision.

Verified by removal: disabling the watchdog arm fails the keepalive and live-client tests;
removing the probe from `accept_bounded::watch_idle_with_probe` fails both again (Close 1001
with no Ping first). Replacing the per-frame `busy()` guard with a bare `touch()` fails the
parked test: the decision runs inline, so the watchdog is not polled during the park, but
without the guard's release the loop comes back to a clock that ran out during it and closes
the connection the instant the answer is sent.

## What is not covered

- Media tracks (out of scope for the protocol; see `src/server/webrtc/CLAUDE.md`).
- Trickle ICE, renegotiation, ICE restart (unsupported).
- Multiple simultaneous peers. The server supports them (one WebSocket each, independent
  state machines) but no test asserts it; adding one costs 3 more LLM calls.
- `max_peers` enforcement.
- Real STUN/TURN traversal — untestable without an external endpoint, which tests must not
  contact.
- Browser interoperability, which is the main thing a human should check by hand before this
  protocol is promoted past `Experimental`.
