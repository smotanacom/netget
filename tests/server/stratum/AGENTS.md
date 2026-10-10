# Stratum pool tests

`install_peers.py <dir>` builds the peers for both roles, each pinned by SHA-256 (cpuminer
2.5.1 from SourceForge, ckpool 3cedff5e1977 from Bitbucket, Bitcoin Core 28.1) and prints
`NETGET_CPUMINER`, `NETGET_CKPOOL`, `NETGET_BITCOIND`, `NETGET_BITCOIN_CLI`. The tests fail
without them.

- `cpuminer_mines_against_netget`: pooler's `minerd -a sha256d` mines at difficulty 0.0005
  until two shares are accepted. cpuminer hashes with its own SHA-256 code and submits only
  what meets its target; NetGet credits only what its own rebuild of the header meets. So
  "yay!!!" twice is agreement on the coinbase, merkle root, header layout and byte orders.
  Mutation-checked: writing ntime big-endian into the header makes cpuminer's shares refused.
- `rust_refuses_bad_shares_and_the_model_credits_good_ones`: raw JSON-RPC — before subscribe
  (25), an unknown worker the model rejects (24), shares for it (24), an unknown job (21), a
  short extranonce2 and a late ntime (20), a share below the difficulty (23, with its hash),
  then a share found with NetGet's own hashing that the model credits, the duplicate of it
  (22), an unknown method (20), the password never shown, and a line over `MAX_LINE`.
- `a_failed_handler_authorizes_nobody`: no model → `false`, a category only, no work.

No LLM calls.
