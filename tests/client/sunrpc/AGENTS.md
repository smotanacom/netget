# SunRPC client tests

No LLM calls: a python chain is the model. The peer is the system **rpcbind** (libtirpc) on
127.0.0.1:111, read back with **rpcinfo**. The test fails rather than skips when rpcbind is not
running. rpcbind accepts SET/UNSET from an unprivileged loopback caller, so the test needs no
root.

## The chain

1. Dump (v4): rpcbind lists itself.
2. PMAP v2 SET of a program in the user range, numbered per process so concurrent runs do not
   collide.
3. GETPORT, answered 4242.
4. RPCBIND v4 GETADDR, answered `….16.146`.

## Readbacks and injected actions

- `rpcinfo -p` shows the registration.
- An injected v4 SET on udp shows in `rpcinfo -s` as versions `2,1` on `udp,tcp`.
- Injected unsets (v2 with tcp; v4 with an empty netid) remove both, and `rpcinfo -p` shows
  that.
- GETPORT then answers 0.
- An invalid version is rejected locally.

Mutation-checked: discarding the model's actions stalls the chain at the dump.
