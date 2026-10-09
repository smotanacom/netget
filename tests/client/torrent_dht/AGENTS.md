# tests/client/torrent_dht

BitTorrent DHT **client**. Two files with very different shapes; neither involves a
third-party DHT node, which is why `src/client/torrent_dht` is
`DevelopmentState::Experimental`.

## `e2e_test.rs` — three `#[test]`, **0 LLM calls, no sockets**

Codec-level. Assertions are on the `dht_query` payload `mod.rs` bencodes into a BEP 5 query,
so a wrong `query_type` fails here rather than on a live node.

`get_async_actions()` advertises `dht_ping`, `dht_find_node`, `dht_get_peers` and
`dht_announce_peer`. `execute_action` once had an arm for none of them — it accepted only
`dht_query`, a name declared nowhere — so a model calling the tool it was shown got
`Unknown DHT client action`, and one copying the action's `example` (which said
`{"type": "dht_query", "query_type": "ping"}`, contradicting the action's own name) worked by
accident. The general-shape guard is
`tests/event_action_declarations_test.rs::every_advertised_client_action_is_accepted_by_its_own_executor`.

- `each_advertised_query_action_sets_its_own_bep5_query_type` — and carries `node_id` through.
- `an_explicit_query_type_is_not_overwritten` — the bare `dht_query` shape still works, so
  callers that learned the old form are unaffected.
- `an_unknown_dht_action_is_still_refused` — widening the executor must not make it fail open.

## `command_channel_test.rs` — one `#[tokio::test]`, **0 LLM calls, real sockets**

`injected_dht_query_reaches_the_node`. In-process through `AppState`, driving a live client.
The "DHT node" is a plain `UdpSocket` owned by the test; it never answers, so no
`dht_response` fires and no further LLM call is attempted. The client's LLM points at an
unreachable URL, so its connected-event call fails and the loop must tolerate that.

- `send_to_client` with `dht_ping` really puts a bencoded query on the wire, and the reported
  `bytes_sent` equals what the node received.
- **The node id must arrive as 20 *decoded* bytes, not 40 characters.** The test decodes the
  datagram and checks `a.id` is 20 bytes hex-equal to what the caller supplied, and that `t`
  is the two bytes `00 aa`. This was a real defect: the parameter description says 40 hex
  characters, the client bencoded the string without decoding, and NetGet's own DHT server —
  which *does* hex-decode — could not read its own client.
- A non-hex `node_id` is `Rejected` with an error naming the field, rather than silently put
  on the wire as text.
- An unknown verb is `Rejected`; the action is recorded in the access log as
  `injected_action`.
- UDP has no wire close, so an injected `disconnect` means stop receiving, release the socket,
  drop the handle — asserted as `Disconnected` plus `!has_client_handle`.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features torrent-dht \
    --test client -- client::torrent_dht --test-threads=100
```
