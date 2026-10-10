# 9P server (9P2000, Plan 9's file protocol)

Hand-written over Tokio TCP (`wire.rs`, shared with the client). The library question was
settled by what NetGet must own: `rs9p` is 9P2000.L only and owns the filesystem behind a
trait, while here every answer comes from the handler, so the codec is small enough to own.

## What Rust owns

Message framing and every bound; Tversion (msize capped at 65536; any "9P2000*" version is
answered with plain 9P2000, anything else with "unknown"); Tattach without authentication
(Tauth is refused); the fid table; walks — full, partial (Rwalk with fewer qids, newfid not
created) and ".."; open modes (no writing a directory, OTRUNC asked as a length-0 wstat,
ORCLOSE honoured at clunk); directory reads paged in whole stat entries with sequential
offsets; file reads served at the client's offset from the content the handler gave at
offset 0; qids (FNV-1a of the path, version from mtime); and Tflush (requests are answered
in order, so the flushed one has already been).

## What the handler decides

One event per decision, each with the path and the attaching `uname`:
`ninep_stat` (→ `ninep_entry` / `ninep_not_found`), `ninep_list` (→ `ninep_listing`),
`ninep_read` (→ `ninep_content`, utf8 or hex), and `ninep_write`, `ninep_create`,
`ninep_remove`, `ninep_wstat` (→ `ninep_ok`). Any of them may answer `ninep_error` with the
text the client shows. A wstat that changes nothing (the sync idiom) is answered without
asking. Nothing is stored: a write is the handler's to remember (memory or the SQLite
facility), and the next read from offset 0 asks again.

## Failure modes and bounds

A handler failure, silence or a wrong-kind answer is an Rerror with `WireFailure`'s generic
text, never a fabricated file (`answers_on_failure`), each with its `decision=` tag. A
message over the negotiated msize is refused from its size field before it is read; 256 fids
per connection; 16 elements per walk (more is malformed and closes the connection); names
at most 255 bytes without '/'; 1 MiB per file; 1024 entries per directory; each connection
may be silent `idle_timeout_secs` (default 300) between messages.
