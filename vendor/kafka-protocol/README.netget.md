# kafka-protocol 0.14.1, patched so a peer-supplied array count cannot abort the process

This is kafka-protocol **0.14.1** exactly as published on crates.io (the `.crate` whose
sha256, `0ce8ca397fccce043d8e1c12ab0122d2facf9249c57ef030ab90f62449413294`, is the checksum
Cargo.lock recorded for it; upstream commit `838fec2967d76bffb8bae96fe2a4acfe47bfa73e`),
with one change in `src/protocol/types.rs`. The root `Cargo.toml` points
`[patch.crates-io] kafka-protocol` here. `Cargo.toml`, both licences, `README.md`, `src/`,
the crate's own `tests/` (its manifest declares that target) and `.cargo_vcs_info.json`
are kept; this file is the only addition.

## Why

`Array<E>` and `CompactArray<E>` decode a length from the wire and call
`Vec::with_capacity(n)` with it before reading a single element, with no check against
the bytes remaining in the buffer. NetGet's Kafka broker implements no SASL, so the
request body reaches these decoders from any unauthenticated TCP peer: an 18-byte
Metadata v1 request whose topics array declares `0x7fffffff` entries asks for
`2^31 × 72` bytes — about 144 GiB — in one allocation. `Vec::with_capacity` on allocator
failure calls `handle_alloc_error`, which **aborts** the process (not a panic, so
`tokio::spawn` cannot contain it and `panic_log` never sees it). On Linux with the default
`vm.overcommit_memory = 0` every host with less than that much memory dies
deterministically. Fetch, Produce and OffsetCommit have nested arrays with similar element
sizes, and the Kafka *client* decodes broker responses through the same code, so a
malicious broker had the same lever. 0.18.0, the newest release when this was written,
carries the identical code.

## The change

Every element costs at least one byte on the wire, so the bytes remaining in the buffer are
a sound upper bound on the pre-allocation; a genuine request keeps its exact pre-sizing and
a hostile count pre-allocates at most the request's own size before the decode fails on
the missing element.

```diff
--- a/src/protocol/types.rs
+++ b/src/protocol/types.rs
@@ impl<T, E: Decoder<T>> Decoder<Option<Vec<T>>> for Array<E> {
             n if n >= 0 => {
-                let mut result = Vec::with_capacity(n as usize);
+                let mut result = Vec::with_capacity((n as usize).min(buf.remaining()));
                 for _ in 0..n {
@@ impl<T, E: Decoder<T>> Decoder<Option<Vec<T>>> for CompactArray<E> {
             n => {
-                let mut result = Vec::with_capacity((n - 1) as usize);
+                let mut result = Vec::with_capacity(((n - 1) as usize).min(buf.remaining()));
                 for _ in 1..n {
```

`tests/vendored_kafka_protocol_patch_test.rs` fails if Cargo.lock stops resolving
kafka-protocol to this directory, if the two patched lines go missing, or if decoding the
hostile request stops being a plain error. `tests/server/kafka/declared_array_length_test.rs`
sends the 18 bytes to a running broker.

## Upgrading

Download the new `.crate`, verify its checksum against Cargo.lock, unpack it over this
directory (keeping this file), re-apply the diff above, update the version, checksum and
commit here and in the ratchet test, and run both tests. If upstream has taken an
equivalent bound, drop the patch entry from the root `Cargo.toml` and delete this directory.
