# NBD server — Experimental

Network Block Device, fixed newstyle only. `wire.rs` holds the protocol constants and the
`Export` model; `mod.rs` the handshake and transmission.

The handler describes an export once per name per connection (`nbd_export_request` →
`nbd_export`: size up to 1 TiB, extents of text, hex or a fill byte, error regions with an errno,
block sizes, description) or refuses it (`nbd_reject` unknown → NBD_REP_ERR_UNKNOWN, policy →
NBD_REP_ERR_POLICY), and answers NBD_OPT_LIST (`nbd_list` → `nbd_list_exports`). Everything an
extent does not cover reads as zeroes. Rust serves every read from the description, so reads
cost no handler calls.

Rust owns:
- The handshake: NBDMAGIC/IHAVEOPT with FIXED_NEWSTYLE and NO_ZEROES; a client without fixed
  newstyle may only send EXPORT_NAME. Options EXPORT_NAME (refusal closes, as the protocol has
  no error reply there), ABORT, LIST, STARTTLS and anything unknown (ERR_UNSUP), INFO and GO
  (NBD_INFO_EXPORT always; NAME, DESCRIPTION and BLOCK_SIZE when asked), STRUCTURED_REPLY,
  LIST/SET_META_CONTEXT for `base:allocation`. 64 options and 60 s for the whole handshake;
  an option declaring more than 64 KiB closes the connection before anything is read.
- Transmission, read-only: READ (structured: OFFSET_DATA for extents, OFFSET_HOLE for zeroes,
  one data chunk with DF; simple replies otherwise), BLOCK_STATUS for base:allocation (data vs
  hole|zero, 1024 descriptors), FLUSH and CACHE succeed, WRITE (payload drained), TRIM and
  WRITE_ZEROES fail with EPERM, reads past the end or over the maximum block size fail EINVAL,
  reads touching an error region fail with its errno (ERROR_OFFSET names the first bad byte),
  DISC closes. Once a request's first byte arrives the rest must come within 60 s.

No handler answer refuses the export with ERR_POLICY. No TLS, no extended headers, no writes.
