# NBD client — Experimental

Uses `src/server/nbd/wire.rs`. Negotiates fixed newstyle (NO_ZEROES when offered), optionally
NBD_OPT_LIST (`list_exports`), STRUCTURED_REPLY, SET_META_CONTEXT base:allocation, then GO for
`export` asking for name, description and block sizes; a refusal fails the connect with the
server's reason. `nbd_connected` reports size, read-only, block sizes, description, whether
structured replies and block status are available, and the list. One request at a time:
`nbd_read` (up to 1 MiB; reassembles data and hole chunks; reports the first 4096 bytes as text
or hex, the SHA-256, all-zero, or the error with its offset), `nbd_block_status`, `nbd_flush`,
`disconnect` (NBD_CMD_DISC). No writes, TLS or extended headers.
