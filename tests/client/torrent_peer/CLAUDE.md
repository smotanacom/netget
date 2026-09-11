# tests/client/torrent_peer

BitTorrent Peer Wire **client**. Two files with very different shapes; neither involves a
third-party BitTorrent client, which is why `src/client/torrent_peer` is
`DevelopmentState::Experimental`.

## `e2e_test.rs` — five `#[test]`, **0 LLM calls, no sockets**

Codec-level. Every assertion is on the bytes BEP 3 specifies, fed through the protocol's own
`execute_action`, so the test fails if the framing changes rather than if the implementation is
merely refactored.

`get_async_actions()` advertises `peer_interested`, `peer_not_interested`, `peer_request_piece`
and `peer_send_piece`. `execute_action` once had an arm for none of them — it accepted only
`peer_message`, a name declared nowhere — so a model calling the tool it was shown got
`Unknown Peer client action`, and one copying the action's `example` (which said
`{"type": "peer_message", ...}`, contradicting the action's own name) worked by accident. The
general-shape guard is
`tests/event_action_declarations_test.rs::every_advertised_client_action_is_accepted_by_its_own_executor`;
this file pins that the *bytes* are right, not just that the name is accepted.

- `interested_and_not_interested_are_bare_message_ids` — ids 2 and 3, empty payload.
- `request_piece_frames_index_begin_length_as_big_endian_u32` — id 6.
- `send_piece_frames_index_begin_then_the_raw_block` — id 7.
- `a_request_without_its_parameters_is_refused_by_name`, not silently truncated.
- `the_raw_peer_message_shape_still_works`, for the message ids that have no named action
  (have, bitfield, cancel).

## `command_channel_test.rs` — one `#[tokio::test]`, **0 LLM calls, real sockets**

`injected_peer_handshake_reaches_our_own_server`. In-process through `AppState`: a NetGet
torrent-peer **client** connected to a NetGet torrent-peer **server**, with a wire verb
injected from outside the client's loop via `send_to_client`. The server uses a static handler
and the client's LLM points at an unreachable URL, so no model is consulted.

The handshake is asserted as exactly 68 bytes (`ClientSendOutcome::Sent { bytes_sent: 68 }`) —
BEP 3's fixed pstrlen + pstr + reserved + info_hash + peer_id. The server's access log is then
checked for the echoed info_hash and the client's for `injected_action`. An unknown verb is
`Rejected`; `disconnect` half-closes, the server EOFs, and the handle goes away.

Note both ends are NetGet, so this proves the command channel and our own framing agree with
themselves. It is not interop evidence.

Non-obvious: the client's read loop uses `read_exact`, which is **not cancellation-safe**, so
the command channel is drained by its own task rather than a `tokio::select!` arm on the read.
Copy that shape for any client whose reads are length-prefixed.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features torrent-peer \
    --test client -- client::torrent_peer --test-threads=100
```
