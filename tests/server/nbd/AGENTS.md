# NBD tests

Peers: `python3 tests/server/nbd/install_peers.py ROOT` builds libnbd 1.24.3 (nbdinfo, nbdcopy)
and nbdkit 1.48.1 (server, data plugin, error filter) from hash-pinned release tarballs and
prints `NETGET_NBD_NBDINFO`, `NETGET_NBD_NBDCOPY`, `NETGET_NBD_NBDKIT`,
`NETGET_NBD_DATA_PLUGIN` and `NETGET_NBD_ERROR_FILTER`. `tests/helpers/nbd.rs` holds the export
policy (disk0, flaky with an EIO region under data, secret refused, anything else unknown).

- `peer_test.rs` — nbdinfo lists exports, describes disk0 (size, read-only, base:allocation,
  block sizes, description) and maps its allocation run by run; nbdcopy copies disk0 byte for
  byte and fails on flaky with an I/O error (the region holds data, so a sparse copy cannot skip
  it); secret is refused and an unknown export is reported as such.
- `wire_test.rs` — the export model; NetGet's client against NetGet's server; a raw client
  without structured replies (simple replies, EPERM, EINVAL, ERR_UNKNOWN, ERR_UNSUP) and an
  oversized option.

`tests/client/nbd/peer_test.rs` — NetGet's client against nbdkit.
