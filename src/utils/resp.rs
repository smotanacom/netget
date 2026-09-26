//! A bound on RESP2 nesting and declared lengths, applied before a decoder ever sees the bytes.
//!
//! `redis-protocol` 6.0 has no depth limit. `resp2::decode` walks a frame with
//! `d_parse_frame` → `d_parse_array` → `nom::multi::count(d_parse_frame, len)`, one level of
//! Rust recursion per nested array, and `build_owned_frame` then recurses over the result a
//! second time. Nothing in either path counts how deep it has gone.
//!
//! `*1\r\n` opens a level in **four bytes**, so a few hundred kilobytes overflow a tokio
//! worker's 2 MiB stack, and the Redis server's only size bound is its 64 MiB
//! incomplete-frame buffer. **This is not a panic.** A Rust stack overflow raises `SIGSEGV`
//! against the guard page and the runtime aborts; `tokio::spawn` cannot contain it. One
//! unauthenticated connection takes the whole of netget down with it. Confirmed by sending
//! 100 000 levels to the unguarded server: the test binary died with
//! `fatal runtime error: stack overflow`.
//!
//! So the bytes are walked *iteratively* first. [`scan_resp2_frame`] keeps one counter per
//! open aggregate in a fixed array of [`MAX_RESP_DEPTH`] slots, allocates nothing, never
//! recurses, and reads each byte at most twice (once to find a line's CRLF, once to parse it).
//! Only a frame it reports as [`RespScan::Complete`] is handed to the decoder.
//!
//! It follows the decoder's grammar exactly where the two could otherwise disagree — a length
//! is parsed with the same `str::parse::<isize>` the crate uses, a bulk string's two trailing
//! bytes are skipped without being checked for CRLF just as `nom_take(2)` does — because a
//! guard that rejected something the decoder would have accepted is a behaviour change, and one
//! that accepted something the decoder would then recurse into is no guard at all.
//!
//! **Declared lengths.** `nom` 7.1.3's `count` caps its initial `Vec::with_capacity` at
//! 64 KiB (`MAX_INITIAL_CAPACITY_BYTES`, `src/multi/mod.rs:577`), so `*4000000000\r\n` is not
//! an allocation bomb by itself. With depth bounded, the most the decoder can pre-allocate at
//! once is [`MAX_RESP_DEPTH`] × 64 KiB. What a declared length *is* is a promise about bytes
//! that have not arrived, and one the caller's buffer cap guarantees can never be kept should
//! be refused on the header rather than after the peer has filled the buffer.
//! [`RespLimits::max_bulk_len`] does that for `$N`, and [`RespLimits::max_elements`] bounds
//! both any single `*N` and the total number of values in one frame, which is what bounds the
//! decoder's per-value allocation (a `RangeFrame` and then an `OwnedFrame` for every three-byte
//! `+\r\n` the peer sends).

/// Deepest nesting of arrays a RESP2 frame may use before it is refused.
///
/// A Redis *request* never nests at all — real Redis rejects an array inside a command with
/// `Protocol error: expected '$', got '*'` — and the deepest reply shapes in common use (an
/// `EXEC` of `XREAD`s) are four or five levels. 32 is far above both and several thousand
/// levels short of what overflows a worker thread.
pub const MAX_RESP_DEPTH: usize = 32;

/// Largest declared array length, and the most values one frame may contain in total.
///
/// 2^20 values is far past any command a model is asked to answer (NetGet flattens a command
/// into one string for the event), and bounds what `decode` allocates for one frame to a few
/// tens of megabytes — the same order as the 64 MiB buffer cap the Redis server applies to the
/// bytes.
pub const MAX_RESP_ELEMENTS: usize = 1 << 20;

/// The bounds [`scan_resp2_frame`] enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RespLimits {
    /// Deepest array nesting accepted. Clamped to [`MAX_RESP_DEPTH`], which sizes the scan's
    /// fixed counter array.
    pub max_depth: usize,
    /// Largest declared `*N`, and the most values in one frame.
    pub max_elements: usize,
    /// Largest declared `$N`. Set it to the caller's buffer cap: a bulk string longer than the
    /// buffer can never complete.
    pub max_bulk_len: usize,
}

impl RespLimits {
    /// [`MAX_RESP_DEPTH`] and [`MAX_RESP_ELEMENTS`], with the caller's bulk-length cap.
    pub const fn with_max_bulk_len(max_bulk_len: usize) -> Self {
        Self {
            max_depth: MAX_RESP_DEPTH,
            max_elements: MAX_RESP_ELEMENTS,
            max_bulk_len,
        }
    }
}

/// Which declared length was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespLength {
    /// An `*N` longer than [`RespLimits::max_elements`], or one that would take the frame's
    /// total past it.
    Aggregate,
    /// A `$N` longer than [`RespLimits::max_bulk_len`].
    Bulk,
}

/// The verdict on the front of a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespScan {
    /// One whole frame occupies the first `consumed` bytes. `resp2::decode` will return the
    /// same count for it.
    Complete { consumed: usize },
    /// Nothing wrong so far and the frame is not finished; read more.
    Incomplete,
    /// Arrays nested deeper than the limit. Refused as soon as the first level past it is
    /// opened, whether or not the rest of the frame has arrived.
    TooDeep { limit: usize },
    /// A declared length past its limit. Refused on the header line.
    TooLong(RespLength),
    /// Not RESP2: a type byte the decoder does not know, or a length or integer it cannot
    /// parse. The decoder would return an error at the same point.
    Malformed,
}

/// Scan the RESP2 frame at the front of `data` without recursing or allocating.
///
/// Call it on the unconsumed buffer before every `resp2::decode`, and decode only on
/// [`RespScan::Complete`]. It is O(n) in the bytes up to the end of the frame or the point of
/// refusal.
pub fn scan_resp2_frame(data: &[u8], limits: &RespLimits) -> RespScan {
    let max_depth = limits.max_depth.min(MAX_RESP_DEPTH);
    // `remaining[d]` is how many values the array open at depth `d + 1` still expects.
    let mut remaining = [0usize; MAX_RESP_DEPTH];
    let mut depth = 0usize;
    let mut values = 0usize;
    let mut i = 0usize;

    loop {
        let Some(&kind) = data.get(i) else {
            return RespScan::Incomplete;
        };
        i += 1;
        values += 1;
        if values > limits.max_elements {
            return RespScan::TooLong(RespLength::Aggregate);
        }

        match kind {
            b'+' | b'-' => match find_crlf(data, i) {
                Some(end) => i = end + 2,
                None => return RespScan::Incomplete,
            },
            b':' => {
                let Some(end) = find_crlf(data, i) else {
                    return RespScan::Incomplete;
                };
                if parse_i64(&data[i..end]).is_none() {
                    return RespScan::Malformed;
                }
                i = end + 2;
            }
            b'$' => {
                let Some(end) = find_crlf(data, i) else {
                    return RespScan::Incomplete;
                };
                let Some(len) = parse_isize(&data[i..end]) else {
                    return RespScan::Malformed;
                };
                i = end + 2;
                if len != -1 {
                    let Ok(len) = usize::try_from(len) else {
                        return RespScan::Malformed;
                    };
                    if len > limits.max_bulk_len {
                        return RespScan::TooLong(RespLength::Bulk);
                    }
                    // The payload and the two bytes after it, which the decoder takes
                    // without checking that they are CRLF.
                    let Some(next) = i.checked_add(len).and_then(|v| v.checked_add(2)) else {
                        return RespScan::TooLong(RespLength::Bulk);
                    };
                    if next > data.len() {
                        return RespScan::Incomplete;
                    }
                    i = next;
                }
            }
            b'*' => {
                let Some(end) = find_crlf(data, i) else {
                    return RespScan::Incomplete;
                };
                let Some(len) = parse_isize(&data[i..end]) else {
                    return RespScan::Malformed;
                };
                i = end + 2;
                if len != -1 {
                    let Ok(len) = usize::try_from(len) else {
                        return RespScan::Malformed;
                    };
                    // Refused on the header: the values this promises, on top of those
                    // already counted, would pass the per-frame total.
                    if len > limits.max_elements.saturating_sub(values) {
                        return RespScan::TooLong(RespLength::Aggregate);
                    }
                    // An empty array still costs the decoder a level of recursion, so it
                    // counts against depth too.
                    if depth + 1 > max_depth {
                        return RespScan::TooDeep { limit: max_depth };
                    }
                    if len > 0 {
                        remaining[depth] = len;
                        depth += 1;
                        continue;
                    }
                }
            }
            _ => return RespScan::Malformed,
        }

        // A value finished. Close every array it was the last element of.
        loop {
            if depth == 0 {
                return RespScan::Complete { consumed: i };
            }
            remaining[depth - 1] -= 1;
            if remaining[depth - 1] > 0 {
                break;
            }
            depth -= 1;
        }
    }
}

/// Index of the first `\r\n` at or after `from`, as `nom`'s `take_until("\r\n")` finds it.
fn find_crlf(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|p| from + p)
}

/// The decoder's own `str::from_utf8(s)?.parse::<isize>()`, so the two agree on every input.
fn parse_isize(line: &[u8]) -> Option<isize> {
    std::str::from_utf8(line).ok()?.parse::<isize>().ok()
}

/// The decoder's `to_i64`.
fn parse_i64(line: &[u8]) -> Option<i64> {
    std::str::from_utf8(line).ok()?.parse::<i64>().ok()
}
