//! A bound on BSON nesting, applied before `bson` ever sees the bytes.
//!
//! `bson` 3.0 has no depth limit. `Document::from_reader` builds a `RawDocumentBuf` and converts
//! it with `TryFrom<&RawDocument> for Document` (`src/raw/document.rs:580`), which turns each
//! element into a `Bson` through `TryFrom<RawBsonRef> for Bson` (`src/raw/bson_ref.rs:412`) and
//! `TryFrom<RawBson> for Bson` (`src/raw/bson.rs:447`) — and an embedded document, an array or a
//! code-with-scope's scope goes straight back into the first of those. One Rust call chain per
//! level, nothing counting. Each level also copies the rest of the bytes (`RawBson::from` owns
//! the sub-document), so the work is quadratic in the depth as well.
//!
//! An embedded document costs the peer **seven bytes per level** (type, a one-byte key and its
//! NUL, a length, a terminator). Measured against 3.0.0, `Document::from_reader` on a thread of
//! a given stack size:
//!
//! | build | stack | survives | overflows at | bytes |
//! |---|---|---|---|---|
//! | debug | 2 MiB (a tokio worker) | 283 levels | **284** | 2 277 |
//! | release | 2 MiB | 1 219 levels | **1 220** | 9 765 |
//! | release | 8 MiB (a main thread) | 4 860 levels | **4 861** | 38 893 |
//!
//! Ten kilobytes in one unauthenticated `OP_MSG` — the MongoDB server's body cap is 48 MB — and
//! the process is gone. **This is not a panic**: a stack overflow is a `SIGSEGV` against the
//! guard page, and `tokio::spawn` cannot contain it.
//!
//! So the bytes are walked *iteratively* first. [`scan_bson_document`] keeps the end offset of
//! each open document in a fixed array of [`MAX_BSON_DEPTH`] slots, allocates nothing, never
//! recurses, and bounds-checks every length against the document that contains it. Only a
//! document it reports [`BsonScan::Complete`] may be handed to `bson`.
//!
//! **It is at least as strict as `bson` about structure, never stricter about anything `bson`
//! would accept.** Every element is sized exactly as `RawIter::get_next_kvp` sizes it, so the
//! scan finds each nested document at the same offset the decoder will; an element that runs
//! into its parent's terminator, which `bson` yields and only then rejects, is `Malformed` here
//! at once. What it does not check — UTF-8 in keys and strings, boolean bytes, the old binary
//! subtype's inner length — cannot open a level, and `bson` still rejects it.
//!
//! **The declared top-level length.** `bson`'s `reader_to_vec` (`src/raw.rs:322`) reserves
//! `Vec::with_capacity(length)` from the document's own first four bytes — up to 2 GiB from a
//! 21-byte message — before discovering the bytes are not there. A document whose declared
//! length exceeds the bytes present is `Malformed` here, so nothing downstream reserves on the
//! peer's number.

/// Deepest nesting a BSON document may use before it is refused. The top-level document is
/// level 1; each embedded document, array or code-with-scope scope inside it adds one.
///
/// MongoDB itself allows 100 levels in a stored document, and a command wraps a document in
/// two more (the command document and its `documents` array). That is not the number here,
/// because the table above says what 100 costs: at ~7.4 KB of stack per level in a debug
/// build, 102 levels spends ~750 KB of a 2 MiB worker on decoding alone, and the MongoDB server
/// then walks the same document recursively three more times (relaxed extended JSON for the
/// model's event, `Debug` in a `trace!`, and `Drop`). 64 keeps the whole of that well inside
/// one worker in every build, and no driver-generated command comes near it; the cost is that
/// a document nested 63 to 100 levels deep, which real MongoDB would store, is refused.
pub const MAX_BSON_DEPTH: usize = 64;

/// The verdict on a BSON document at the front of a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BsonScan {
    /// One whole, structurally sound document occupies the first `consumed` bytes.
    Complete { consumed: usize },
    /// Nested deeper than the limit. Refused as soon as the first level past it opens.
    TooDeep { limit: usize },
    /// Not a BSON document: a length that is negative, too small or larger than what contains
    /// it, a missing terminator, an unknown element type, or an element that runs past its
    /// document. `bson` would reject it too, but possibly only after recursing into it.
    Malformed,
}

/// Walk one BSON document iteratively, refusing anything nested deeper than [`MAX_BSON_DEPTH`].
///
/// Call it on every document that arrives from the network before `bson` decodes it. It is
/// O(n), allocates nothing, and never recurses. Bytes after the document are ignored, as
/// `Document::from_reader` ignores them.
pub fn scan_bson_document(data: &[u8]) -> BsonScan {
    scan_bson_document_with_limit(data, MAX_BSON_DEPTH)
}

/// [`scan_bson_document`] with an explicit limit, clamped to [`MAX_BSON_DEPTH`] (which sizes
/// the fixed array of open documents).
pub fn scan_bson_document_with_limit(data: &[u8], max_depth: usize) -> BsonScan {
    let max_depth = max_depth.min(MAX_BSON_DEPTH);
    // `ends[d]` is one past the last byte (the terminator) of the document open at depth d+1.
    let mut ends = [0usize; MAX_BSON_DEPTH];
    let mut depth = 0usize;

    let Some(end) = document_end(data, 0, data.len()) else {
        return BsonScan::Malformed;
    };
    if max_depth == 0 {
        return BsonScan::TooDeep { limit: max_depth };
    }
    ends[0] = end;
    depth += 1;
    let mut pos = 4usize;

    loop {
        let end = ends[depth - 1];
        // The terminator: `document_end` already checked it is a zero byte.
        if pos == end - 1 {
            pos = end;
            depth -= 1;
            if depth == 0 {
                return BsonScan::Complete { consumed: end };
            }
            continue;
        }
        // Elements occupy [4, end - 1); anything reaching the terminator is malformed.
        let limit = end - 1;
        if pos > limit {
            return BsonScan::Malformed;
        }

        let kind = data[pos];
        pos += 1;
        // The key: a NUL-terminated string that must end before the terminator.
        let Some(nul) = data[pos..limit].iter().position(|&b| b == 0) else {
            return BsonScan::Malformed;
        };
        pos += nul + 1;

        let size = match kind {
            // Fixed-size values.
            0x08 => 1,                      // boolean
            0x10 => 4,                      // int32
            0x01 | 0x09 | 0x11 | 0x12 => 8, // double, datetime, timestamp, int64
            0x07 => 12,                     // ObjectId
            0x13 => 16,                     // decimal128
            0x06 | 0x0A | 0x7F | 0xFF => 0, // undefined, null, max key, min key
            // Length-prefixed strings: int32 length (including the NUL) >= 1, then the bytes.
            0x02 | 0x0D | 0x0E => match string_size(data, pos, limit) {
                Some(n) => n,
                None => return BsonScan::Malformed,
            },
            // DBPointer: a string, then an ObjectId.
            0x0C => match string_size(data, pos, limit) {
                Some(n) => n + 12,
                None => return BsonScan::Malformed,
            },
            // Binary: int32 length >= 0, a subtype byte, then the bytes.
            0x05 => match read_i32(data, pos, limit).and_then(|n| usize::try_from(n).ok()) {
                Some(n) => match n.checked_add(5) {
                    Some(n) => n,
                    None => return BsonScan::Malformed,
                },
                None => return BsonScan::Malformed,
            },
            // Regular expression: two C strings.
            0x0B => {
                let Some(p) = data[pos..limit].iter().position(|&b| b == 0) else {
                    return BsonScan::Malformed;
                };
                let Some(o) = data[pos + p + 1..limit].iter().position(|&b| b == 0) else {
                    return BsonScan::Malformed;
                };
                p + 1 + o + 1
            }
            // Embedded document or array: a nested level.
            0x03 | 0x04 => {
                let Some(child_end) = document_end(data, pos, limit) else {
                    return BsonScan::Malformed;
                };
                if depth >= max_depth {
                    return BsonScan::TooDeep { limit: max_depth };
                }
                ends[depth] = child_end;
                depth += 1;
                pos += 4;
                continue;
            }
            // Code with scope: int32 total, a string, then a document that must end exactly
            // where the total says — `bson` parses the scope with `RawDocument::from_bytes` on
            // the rest of the element, which demands that. The scope is a nested level.
            0x0F => {
                let Some(total) = read_i32(data, pos, limit).and_then(|n| usize::try_from(n).ok())
                else {
                    return BsonScan::Malformed;
                };
                // 4 (total) + 5 (the shortest string) + 5 (the shortest document).
                if total < 14 {
                    return BsonScan::Malformed;
                }
                let element_end = match pos.checked_add(total) {
                    Some(e) if e <= limit => e,
                    _ => return BsonScan::Malformed,
                };
                let Some(code) = string_size(data, pos + 4, element_end) else {
                    return BsonScan::Malformed;
                };
                let scope = pos + 4 + code;
                match document_end(data, scope, element_end) {
                    Some(scope_end) if scope_end == element_end => {}
                    _ => return BsonScan::Malformed,
                }
                if depth >= max_depth {
                    return BsonScan::TooDeep { limit: max_depth };
                }
                ends[depth] = element_end;
                depth += 1;
                pos = scope + 4;
                continue;
            }
            _ => return BsonScan::Malformed,
        };

        match pos.checked_add(size) {
            Some(next) if next <= limit => pos = next,
            _ => return BsonScan::Malformed,
        }
    }
}

/// The end of the document starting at `start`, which must lie wholly before `limit`: its
/// declared length is at least 5, fits, and its last byte is the zero terminator.
fn document_end(data: &[u8], start: usize, limit: usize) -> Option<usize> {
    let len = usize::try_from(read_i32(data, start, limit)?).ok()?;
    if len < 5 {
        return None;
    }
    let end = start.checked_add(len)?;
    if end > limit || data[end - 1] != 0 {
        return None;
    }
    Some(end)
}

/// The size of a length-prefixed BSON string at `start` — the four length bytes plus the
/// declared length — if the declared length is at least 1 and it fits before `limit`.
fn string_size(data: &[u8], start: usize, limit: usize) -> Option<usize> {
    let len = usize::try_from(read_i32(data, start, limit)?).ok()?;
    if len < 1 {
        return None;
    }
    let size = len.checked_add(4)?;
    if start.checked_add(size)? > limit {
        return None;
    }
    Some(size)
}

/// A little-endian i32 at `start`, if all four bytes lie before `limit`.
fn read_i32(data: &[u8], start: usize, limit: usize) -> Option<i32> {
    let bytes = data.get(start..start.checked_add(4)?)?;
    if start + 4 > limit {
        return None;
    }
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}
