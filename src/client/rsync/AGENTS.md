# rsync client

Uses the daemon's protocol-29 codec (`src/server/rsync/wire.rs`; read that AGENTS.md first).
Each action is one TCP connection, as rsync itself makes one per transfer:

- **`rsync_list_modules`**: the module line is empty. Lines with a TAB are modules, the rest
  is MOTD. The answer arrives as `rsync_modules`.
- **`rsync_list{path, recursive}`**: arguments `--server --sender -ld|-lr . <path>`. No file
  requests are made, then the end-of-run exchange. The answer arrives as `rsync_listing`.
- **`rsync_fetch{path, recursive}`**: the same, then one whole-file request (no basis) for
  each regular file while `max_fetch_bytes` lasts; the rest are listed as `skipped`. Each
  file is checked against MD4(seed ‖ data). Contents arrive in `rsync_fetched`, as utf8, or
  as hex when not text.
- `@RSYNCD: AUTHREQD` is answered with `username` and base64 (no padding) of
  MD4(0000 ‖ password ‖ challenge). Without credentials the operation reports that the module
  requires authentication.

A stock daemon sends its file list in readdir order; it is sorted into rsync's order before
indexes are assigned.

Requests are written concurrently with reading the replies (`tokio::join!` of a writer and a
reader), because rsync's sender stops reading while it is blocked writing file data.

Failures (a refusal, a missing path, a checksum mismatch, a timeout of 120 s per operation)
arrive as `rsync_error`. Injected actions return the event's data as the `Executed` detail.

Chains are bounded at `MAX_FOLLOWUP_DEPTH` (8). Nothing is written to disk.
