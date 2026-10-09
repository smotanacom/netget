# 9P server tests

`TREE_SCRIPT` (in `wire_test.rs`) answers every event from one Python script: a small tree,
`/many` with 300 files for paging, `/bin.dat` as hex content, and changes accepted only
under `/scratch`.

- `wire_test.rs`: raw messages — version capping and dialect/unknown answers, Tauth refused,
  full/partial/failed/".." walks, reads at offsets, binary content, directory paging in whole
  entries with a bad-offset refusal, stat fields, create and write (the written text seen by
  the handler), refusals carrying the handler's text, a refused remove still clunking, the
  no-op wstat answered without asking, a directory refused for writing; then the bounds
  (before-version, over-msize, a 17-element walk, the 256-fid cap, idle close) and the
  fail-closed paths (oversized content and no handler both answer the generic Rerror).
- `real_client_test.rs`: 9fans.net/go's plan9/client and knusbaum/go9p's client, through
  `tests/client/ninep/peer`, each running a fixed scenario whose whole JSON output is
  asserted. go9p's `Create` returns a file with iounit 0 whose writes loop forever on any
  server, so its scenario reopens the file before writing.

No LLM calls: every handler is a script.
