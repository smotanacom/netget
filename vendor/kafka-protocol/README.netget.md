# kafka-protocol 0.14.1, patched so a peer-supplied count or length cannot abort the process

This is kafka-protocol **0.14.1** exactly as published on crates.io (the `.crate` whose
sha256, `0ce8ca397fccce043d8e1c12ab0122d2facf9249c57ef030ab90f62449413294`, is the checksum
Cargo.lock recorded for it; upstream commit `838fec2967d76bffb8bae96fe2a4acfe47bfa73e`),
with the changes listed below in `src/protocol/types.rs`, `src/records.rs` and
`src/compression/snappy.rs`. The root `Cargo.toml` points `[patch.crates-io] kafka-protocol`
here. `Cargo.toml`, both licences, `README.md`, `src/`, the crate's own `tests/` (its
manifest declares that target) and `.cargo_vcs_info.json` are kept; this file is the only
addition.

## Why

The decoder sized allocations from numbers the peer wrote, before reading the bytes those
numbers describe. NetGet's Kafka broker implements no SASL, so a request body reaches the
decoder from any unauthenticated TCP peer, and the Kafka *client* decodes broker responses
(including every Fetch response's record batches) through the same code, so a malicious
broker had the same lever. An allocation the allocator refuses calls `handle_alloc_error`,
which **aborts** the process — not a panic, so `tokio::spawn` cannot contain it and
`panic_log` never sees it. On Linux with the default `vm.overcommit_memory = 0` every host
with less memory than the request dies deterministically.

- **Arrays.** `Array<E>` and `CompactArray<E>` called `Vec::with_capacity(n)` with the wire
  count. An 18-byte Metadata v1 request whose topics array declares `0x7fffffff` entries
  asked for `2^31 × 72` bytes — about 144 GiB — in one allocation.
- **Record batches.** Every Produce request carries RecordBatches and the broker decodes
  each one. `decode_new_records` called `records.reserve(record_count)` with the batch's
  `record_count`, an `i32` checked only for `< 0`: a **61-byte** batch with a correct CRC
  declaring `0x7fffffff` records reserves hundreds of GiB. `Record::decode_new` did the same
  with each record's header count (`IndexMap::with_capacity(num_headers)`).
- **Snappy.** A snappy-compressed batch's raw block begins with a varint uncompressed
  length, up to `u32::MAX`, and the decompressor zero-filled a buffer of that size
  (`tmp.resize(actual_len, 0)`) before decoding a byte: 5 bytes of header buy 4 GiB of
  touched memory.
- **Byte and string decoders.** `String`, `CompactString`, `Bytes` and `CompactBytes`, when
  decoding into an owned `std::string::String` / `Vec<u8>`, did `vec![0; n]` from the
  declared length (up to 4 GiB for the compact forms) and only then found the buffer short.
  No generated message decodes into those types — every message field is `StrBytes` or
  `bytes::Bytes`, read through `try_get_bytes`, which checks first — and the `Decoder` trait
  is `pub(crate)`, so these four are not reachable from outside the crate in 0.14.1. They
  are patched anyway, so the rule holds without exception and an upgrade that starts using
  them does not reopen the hole.

0.18.0, the newest release when the array patch was written, carries the identical code at
every one of these sites.

## The changes

Each is marked `// netget:` (the two array sites `// NetGet patch`) in the source.

| File | Site | Change |
|---|---|---|
| `src/protocol/types.rs` | `Decoder<Option<Vec<T>>> for Array<E>` | `Vec::with_capacity((n as usize).min(buf.remaining()))` |
| `src/protocol/types.rs` | `Decoder<Option<Vec<T>>> for CompactArray<E>` | `Vec::with_capacity(((n - 1) as usize).min(buf.remaining()))` |
| `src/records.rs` | `RecordBatchDecoder::decode_new_records` | `records.reserve(batch_decode_info.record_count.min(buf.remaining()))` |
| `src/records.rs` | `Record::decode_new` (headers) | `IndexMap::with_capacity(num_headers.min(buf.len()))` |
| `src/protocol/types.rs` | `Decoder<Option<StdString>> for String` | `NotEnoughBytesError` if `n > buf.remaining()`, before `vec![0; n]` |
| `src/protocol/types.rs` | `Decoder<Option<StdString>> for CompactString` | same, for `n - 1` |
| `src/protocol/types.rs` | `Decoder<Option<Vec<u8>>> for Bytes` | same, for `n` |
| `src/protocol/types.rs` | `Decoder<Option<Vec<u8>>> for CompactBytes` | same, for `n - 1` |
| `src/compression/snappy.rs` | `Snappy::decompress` | error if the declared length exceeds 32× the compressed input, before `tmp.resize` |

The reasoning is the same at every count site: each element (array entry, record, header)
costs at least one byte on the wire, so the bytes remaining are a sound upper bound on the
pre-allocation. A genuine message keeps its exact pre-sizing; a hostile count pre-allocates
at most the message's own size before the decode fails on the missing element. Length sites
refuse outright, with the same `NotEnoughBytesError` the copy that follows would have
returned. The snappy ceiling is from the format: no snappy element produces more than 64
output bytes from 3 input bytes (a copy with a 2-byte offset), so a valid block never
declares more than ~21.3× its own size, and 32× leaves margin while bounding the fill by the
batch itself.

## Not covered

- **Decompression output.** gzip, lz4 and zstd decompress by streaming into a growing
  buffer, so their memory is bounded by what the data actually inflates to rather than by a
  declared number — a compression bomb inside the broker's 100 MiB request cap still
  inflates in full. That is a different class (no declared length to check) and is not
  patched here.
- **Legacy (magic 0/1) nested compression.** `Record::decode_legacy` recurses once per
  nested compressed wrapper message with no depth counter.

## Tests

`tests/vendored_kafka_protocol_patch_test.rs` fails if Cargo.lock stops resolving
kafka-protocol to this directory, if any patched line goes missing, or if decoding the
hostile array requests stops being a plain error. Its
`every_decode_allocation_in_the_crate_is_bounded_by_the_bytes_present` walks every `.rs`
file under `src/` and fails on any `with_capacity`, `reserve`, `vec![0; …]` or `resize` it
cannot classify as bounded, so an upgrade that adds a site has to be looked at.
`tests/server/kafka/declared_array_length_test.rs` sends the 18-byte Metadata request and a
Produce request carrying the 61-byte batch to a running broker, and decodes the hostile
record-count, header-count, key-length, header-key-length and snappy batches through
`RecordBatchDecoder` beside a well-formed control batch that proves the hand-built framing
and CRC are right.

## Upgrading

Download the new `.crate`, verify its checksum against Cargo.lock, unpack it over this
directory (keeping this file), re-apply every change in the table above, update the
version, checksum and commit here and in the ratchet test, and run both tests. If upstream
has taken equivalent bounds, drop the patch entry from the root `Cargo.toml` and delete this
directory.
