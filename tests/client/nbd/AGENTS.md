# NBD client tests

`peer_test.rs` runs nbdkit 1.48.1 (independent, unchanged) with its data plugin ("hello nbdkit"
at 0, "tail" at 64 KiB, 1 MiB) behind its error filter, armed by creating `{dir}/inject`.
NetGet's client lists the exports (the name-agnostic plugin's default ""), reads the plugin's
bytes and a zero region, gets allocation from block status, flushes, then reads after the filter
is armed and reports EIO. Needs the `NETGET_NBD_*` variables from
`tests/server/nbd/install_peers.py`.
