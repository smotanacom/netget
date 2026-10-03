# NetGet's tonic 0.12.3 receive-limit patch

This directory is the crates.io `tonic` 0.12.3 source. The version, MIT license,
original manifest, and `.cargo_vcs_info.json` upstream commit are preserved.

Upstream `codec/decode.rs` checks a frame's compressed length against
`max_decoding_message_size`, but `codec/compression.rs` inflates gzip/zstd with an
unrestricted `io::copy`. A small compressed frame can therefore allocate and decode
past the declared receive limit on either a client or server.

The local patch passes that same configured limit to decompression, caps the initial
reserve, and reads at most limit + 1 decompressed bytes. Going over the limit returns
`RESOURCE_EXHAUSTED`. The existing uncompressed length check uses that status too.
No RetryInfo accompanies these permanent size failures. Invalid compressed data keeps
upstream's `INTERNAL` behavior. Public APIs, versions and wire encoding remain unchanged.
BytesMut retains its normal bounded capacity growth; the limit governs message length.

`tests/vendored_tonic_patch_test.rs` uses an independent test service and client over a
real loopback socket: exactly 4 MiB succeeds, 4 MiB + 1 fails, in request and response
directions with plain and gzip messages. Over-limit requests never reach the service.
The test also checks pinned version, license and provenance. Run with the `grpc` feature.

When upgrading tonic, compare these two codec files against the new upstream source,
retain this patch only if the upstream decompressor is still unbounded, and rerun the
wire regressions before removing or changing the patch.
