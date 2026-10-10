# Stratum V1 pool (server)

Newline-delimited JSON-RPC over TCP, well-known port 3333 (Stratum has no IANA port; 3333 is
the convention ckpool and the original pools set). `wire.rs` is shared with the client role:
framing, the error shape, and the share arithmetic.

## What Rust owns

- `mining.subscribe` → `[[["mining.set_difficulty", id], ["mining.notify", id]], extranonce1,
  4]`: a 4-byte extranonce1 per connection (a counter from a random start), extranonce2 of
  `EXTRANONCE2_SIZE` (4). Anything before subscribe is error 25.
- `mining.configure` → `{}` (no extensions, so no version rolling); `mining.extranonce.subscribe`
  and `mining.suggest_difficulty` → `false` (the latter is logged and ignored). Unknown
  methods → error 20. Notifications from miners are ignored.
- **Jobs** are built in Rust (`wire::pool_job`): a coinbase whose scriptSig carries the BIP 34
  height (per connection, from 1), the message (`DEFAULT_COINBASE_MESSAGE` "NetGet", at most
  `MAX_COINBASE_MESSAGE` 64 bytes) and an 8-byte push for the extranonces; one zero-value
  `OP_RETURN "netget"` output; no other transactions (no merkle branches); version
  0x20000000, nbits `POOL_NBITS` (0x207fffff), ntime = now. The pool is connected to no node,
  so nothing is ever submitted as a block. A connection keeps `MAX_JOBS` (16); `clean_jobs`
  voids the rest.
- **Shares** (`mining.submit [worker, job_id, extranonce2, ntime, nonce]`), refused by Rust
  with the slush error codes before anyone is asked: unauthorized worker 24, unknown/stale job
  21, extranonce2 not 4 bytes or not hex 20, ntime outside `[job.ntime, job.ntime +
  NTIME_ROLL]` 20, a sixth (version) parameter 20, a repeat 22 (the last `MAX_SEEN_SHARES`),
  and a hash below the difficulty 23 — whose message names the hash. Otherwise the header is
  rebuilt (coinbase → merkle root → 80 bytes), double-SHA-256 hashed and measured
  (`DIFF1 / hash`), and only a share meeting the connection's difficulty reaches the model.

## What the model decides

- `stratum_authorize {worker, password_given, user_agent, remote_addr}` — the password itself
  is never shown. `stratum_accept` authorizes; Rust then sends the difficulty and work (the
  latest job, or a fresh one). `stratum_reject {message}` → `false` with `[24, message]`.
- `stratum_share {worker, job_id, hash, share_difficulty, difficulty, remote_addr}` —
  `stratum_accept` credits it (`true`); `stratum_reject {message}` → `[20, message]`.
- Either may be accompanied by `stratum_set_difficulty {difficulty}`, `stratum_new_job
  {message?, prev_hash?, clean_jobs?}` and `stratum_show_message {message}`.

## Failure

A failed model call, an invalid reply, silence, or both accept and reject, answer `false` with
`[20, <category>]` — never the error — and authorize nobody / credit nothing. Decisions are
logged `decision=model_answer|model_reject|model_silent|fail_closed_*`.

## Bounds

Lines `MAX_LINE` (16 KiB, the connection ends past it); idle `idle_timeout_secs` (default 900);
`MAX_WORKERS` (32) per connection; `DEFAULT_MAX_CONNECTIONS`; difficulty in (0, 1e15].
