# MessagePack-RPC client tests

- `session_test.rs`: against NetGet's own server — two calls in flight matched by msgid, an
  error, binary round-tripped, a notification each way, a bad action refused locally.
- `real_server_test.rs`: Neovim with `--listen` (eval, a command and the variable it set, an
  E117 error, a buffer handle as extension 0, and a notification after `nvim_subscribe`) and
  ugorji/go's MsgpackSpecRpc net/rpc server (`Arith.Add`, `Echo`, `Fail`).

`install_peers.py ROOT` builds `peer/` (go.sum-pinned) and checks Neovim is installed,
printing NETGET_MSGPACK_PEER and NETGET_MSGPACK_NVIM for both suites.
