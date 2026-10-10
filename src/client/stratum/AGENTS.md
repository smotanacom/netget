# Stratum V1 miner (client)

Connects (`host:port`, a `stratum+tcp://` prefix is accepted), sends `mining.subscribe` and
`mining.authorize [user, password]` (`user` default `netget`, `password` default `x`; the
password's name keeps it redacted wherever startup parameters are printed), and tracks
`mining.set_difficulty`, `mining.notify` (the last `MAX_JOBS`, 8; `clean_jobs` clears them) and
`mining.set_extranonce`. `client.get_version` is answered by Rust; other server requests get
error 20.

## Events

- `stratum_authorized {worker, ok, error}` for every authorize answered.
- `stratum_job {job_id, clean_jobs, prev_hash, ntime, nbits, merkle_branches, difficulty}`.
- `stratum_share_result {job_id, nonce, extranonce2, hash, share_difficulty, accepted, error}`:
  `hash` and `share_difficulty` are Rust's own computation for the share submitted.
- `stratum_mining_done {hashes, best_difficulty, difficulty, reason}` when a mine submitted
  nothing.
- `stratum_message {method, params}` for anything else.

## Actions

- `stratum_mine {max_hashes?, submit_best?}` — single-threaded on a blocking thread, at most
  `MAX_MINE_HASHES` (50M, default 1M): a fresh extranonce2, nonces from 0, the first share
  meeting the difficulty submitted (or the best one with `submit_best`).
- `stratum_submit {nonce, extranonce2, job_id?, ntime?}` — numbers; Rust formats the hex.
- `stratum_suggest_difficulty {difficulty}`, `stratum_authorize {user, password?}`,
  `disconnect`.

Shares are submitted as the worker authorized on connect. A chain stops after
`MAX_FOLLOWUP_DEPTH` (8); `MAX_PENDING` (64) requests may await the pool.
