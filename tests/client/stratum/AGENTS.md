# Stratum miner tests

`real_server_test.rs` runs Bitcoin Core 28.1 in regtest (one block mined to a wallet address)
and ckpool in solo mode on it, from `tests/server/stratum/install_peers.py`.

ckpool will not go below difficulty 1 (4 billion hashes a share), so the chain mines each job
for 20 000 hashes and submits the best share anyway. ckpool refuses it `[23, "Above target"]`
and logs the hash *it* computed in `logs/ckt.log`; the test asserts that line names exactly the
hash NetGet reported, for the mined share and for a hand submission. That is ckpool's code and
NetGet's agreeing on the coinbase, merkle root and header. A worker that is not a regtest
address is refused by ckpool (checked with bitcoind) on a second connection, and a submit for
an unknown job is refused before the wire.

Mutation-checked: dropping the model's actions in the dispatcher fails the test. No LLM calls.
