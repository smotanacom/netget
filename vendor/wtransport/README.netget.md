# NetGet's wtransport 0.7.2 patches

This directory is the crates.io `wtransport` 0.7.2 source (MIT OR Apache-2.0; the original
manifest is `Cargo.toml.orig`). `vendor/wtransport-proto` is its protocol crate at the same
version, patched alongside it. Cargo builds both through `[patch.crates-io]`, and
`tests/vendored_wtransport_patch_test.rs` fails if Cargo.lock stops resolving either one to
these copies.

Upstream's driver could not be bounded or stopped from outside, and its peers could
grow it without limit. The changes, all in `src/driver/mod.rs` and `src/endpoint.rs`:

- **Dropping the last session handle ends the connection.** `Driver` now owns its worker's
  `JoinHandle`; `Drop` closes the QUIC connection (H3_NO_ERROR) and aborts the worker.
  Upstream detached the worker with `tokio::spawn`, so a server that stopped kept every
  session's reader running and its socket answering.
- **Stream and control readers belong to the worker.** Each incoming stream's header parse,
  the local and remote control streams, both QPACK streams and the CONNECT stream run in one
  `JoinSet` that the worker owns, instead of detached tasks. The set holds at most 16 at once
  (further streams wait in QUIC flow control), and each header parse has 10 seconds.
- **Settings are checked.** A peer must enable H3 datagrams and WebTransport (either the
  draft-02 `ENABLE_WEBTRANSPORT` or a non-zero `WEBTRANSPORT_MAX_SESSIONS`), its QUIC transport
  must allow datagrams, and boolean settings must be 0 or 1; otherwise the connection fails
  with H3_SETTINGS_ERROR. Upstream accepted anything (`TODO(biagio): validate settings`). A
  client opens a session only after the server set `ENABLE_CONNECT_PROTOCOL`, as RFC 9220
  requires before extended CONNECT.
- **A dropped settings channel is an error, not a panic** (`expect("Channel cannot be
  dropped")`).
- Four `is_empty` helpers the old control-stream loop used are removed.

`vendor/wtransport-proto` (`src/qpack.rs`, `src/settings.rs`, `src/error.rs`):

- QPACK field sections are bounded at 64 fields and 16 KiB (RFC 9114 field size, 32 bytes of
  overhead per field). Upstream decoded any number of any size into a `HashMap`.
- A field section that names a dynamic table entry, or has a non-zero Required Insert Count
  or Base, is refused (the decoder advertises no dynamic table). Upstream read and ignored both.
- Duplicate field names are refused instead of silently overwriting, since the header map
  holds one value per name; invalid names (anything but lowercase token characters), control
  characters in values and pseudo-headers after regular fields are refused.
- A SETTINGS frame that repeats an identifier is H3_SETTINGS_ERROR (RFC 9114 §7.2.4).
- `ErrorCode::Internal` (H3_INTERNAL_ERROR) exists for a reader task that panicked.

Public APIs, versions and wire encoding are otherwise unchanged. When upgrading, diff the new
upstream against `/src` of the crates.io 0.7.2 package, keep each change only if upstream still
lacks it, and rerun `tests/vendored_wtransport_patch_test.rs` and the `webtransport` server and
client suites.
