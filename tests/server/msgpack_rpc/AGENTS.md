# MessagePack-RPC server tests

`RPC_SCRIPT` (in `wire_test.rs`): add sums, echo returns its params, notify_me answers and
notifies back, anything else is an error; notifications are ignored.

- `wire_test.rs`: three pipelined requests answered in order, binary and extensions through
  the JSON round trip, a notification seen by the handler, a notification sent back, a message
  split across writes; then a str32 announcing 4 GiB (never allocated; closed past 1 MiB), a
  valid request nesting past 32 levels closed without any reply, non-envelopes, an idle
  connection, and no handler (an error response, never a result).
- `real_client_test.rs`: Neovim (`nvim --headless -l` with rpcrequest/rpcnotify over
  sockconnect) and ugorji/go's MsgpackSpecRpc codec.

Peers from `tests/client/msgpack_rpc/install_peers.py`. No LLM calls.
