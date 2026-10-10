# 9P client (9P2000)

Uses the server's `wire.rs`. Connects, negotiates 9P2000 (msize 65536 offered), attaches as
`uname` (default netget) to `aname`, and keeps the root fid. Each action walks from the root
to a fresh fid — in steps of 16 elements for longer paths — does its work and clunks it.

## Actions

`ninep_ls`, `ninep_cat`, `ninep_stat`, `ninep_write {data, encoding?, append?, create?}`,
`ninep_mkdir`, `ninep_remove`, `ninep_rename {name}`. Every result arrives as `ninep_result`
with `ok`, and a server refusal (Rerror) is a result with its text in `error`, not a
failure: the session goes on. A transport or framing failure ends the session.

`append` stats the file and writes at its length (9P has no append mode for opens); a
rename is a wstat changing only the name, so it cannot move between directories.

## Bounds

A message over the negotiated msize is refused before it is read; a read is capped at
1 MiB and a listing at 1024 entries; a reply must carry the request's tag and type + 1, and a
read must not return more than asked. Each request has a 30 s deadline.
