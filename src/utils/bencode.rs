//! A bound on bencode nesting, applied before a decoder ever sees the bytes.
//!
//! `serde_bencode` has no depth limit. Its `Deserializer::deserialize_any` recurses into
//! `visit_seq`/`visit_map` for every `l` or `d` it meets, and serde's derived struct
//! deserialisers reach the same code through `IgnoredAny` when they skip an unknown field,
//! so a *typed* decode is no safer than decoding into [`serde_bencode::value::Value`].
//! Nothing in either path counts how deep it has gone.
//!
//! Bencode is nested by construction and one `l` byte opens a level, so the cost to an
//! attacker is one byte per stack frame. Measured against 0.2.4, decoding `l` repeated N
//! times on a thread of a given stack size:
//!
//! | nesting | stack | result |
//! |---|---|---|
//! | 800 | 2 MiB (a tokio worker) | returns `Err("End of stream")` |
//! | 1,000 | 2 MiB | **`fatal runtime error: stack overflow, aborting`** |
//! | 9,215 | 8 MiB (a main thread) | **`fatal runtime error: stack overflow, aborting`** |
//!
//! Roughly **one kilobyte on the wire** is enough to kill a worker, and the largest UDP
//! datagram macOS will send (`net.inet.udp.maxdgram` = 9216) overflows even the main thread.
//! **This is not a panic.** A Rust stack overflow raises `SIGSEGV` against the guard page and
//! the runtime aborts; `tokio::spawn` cannot contain it and there is no `catch_unwind` that
//! sees it. One datagram from any host that can reach the socket takes the whole of netget
//! down with it.
//!
//! So the bytes are walked *iteratively* first. [`check_bencode_structure`] uses a depth
//! counter and an index, allocates nothing, and reads each byte once; only input it accepts
//! is handed to `serde_bencode`. It scans exactly one top-level value and ignores whatever
//! follows, which is what `serde_bencode::from_bytes` does too — being stricter about
//! trailing bytes would reject input the decoder would have accepted.
//!
//! It also bounds a declared byte-string length against the bytes actually present rather
//! than trusting it, so `99999999999:` cannot be used to make the caller reserve on a
//! promise the datagram does not keep.

use std::fmt;

/// Deepest nesting a bencode value may use before it is refused.
///
/// Far above anything the protocols that use bencode define. KRPC's deepest legitimate shape
/// is the outer dict, the `a`/`r` dict inside it, and at most a list of dicts below that —
/// four. A tracker announce reply is `d` → `peers` list → peer dict — three. 32 leaves room
/// for an extension nobody has written yet while still being ~2000x short of the frame count
/// that overflows a worker thread.
pub const MAX_BENCODE_DEPTH: usize = 32;

/// Why a bencode byte string was refused before decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BencodeStructureError {
    /// Nesting exceeded the limit. This is the one that matters: it is refused here because
    /// letting it reach `serde_bencode` aborts the process.
    TooDeep { limit: usize },
    /// The value ends mid-way through: an unterminated container, integer or byte string.
    Truncated,
    /// An `e` with no container open.
    UnbalancedEnd,
    /// A byte-string or integer length that is not a number, or a byte-string length longer
    /// than the input that is supposed to contain it.
    BadLength,
    /// A byte where a value could not begin.
    InvalidByte(u8),
    /// No value at all.
    Empty,
}

impl fmt::Display for BencodeStructureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooDeep { limit } => {
                write!(f, "bencode nesting deeper than the {} level limit", limit)
            }
            Self::Truncated => write!(f, "bencode value is truncated"),
            Self::UnbalancedEnd => write!(f, "bencode 'e' with no open container"),
            Self::BadLength => write!(f, "bencode length prefix is invalid or exceeds the input"),
            Self::InvalidByte(b) => {
                write!(f, "byte 0x{:02x} cannot begin a bencode value", b)
            }
            Self::Empty => write!(f, "bencode input is empty"),
        }
    }
}

impl std::error::Error for BencodeStructureError {}

/// Walk one bencode value iteratively, refusing anything nested deeper than
/// [`MAX_BENCODE_DEPTH`].
///
/// Call this on every byte string that arrives from the network before handing it to
/// `serde_bencode`. It is O(n), allocates nothing, and never recurses.
pub fn check_bencode_structure(data: &[u8]) -> Result<(), BencodeStructureError> {
    check_bencode_structure_with_limit(data, MAX_BENCODE_DEPTH)
}

/// [`check_bencode_structure`] with an explicit limit, for tests.
pub fn check_bencode_structure_with_limit(
    data: &[u8],
    max_depth: usize,
) -> Result<(), BencodeStructureError> {
    if data.is_empty() {
        return Err(BencodeStructureError::Empty);
    }

    let mut i = 0usize;
    let mut depth = 0usize;

    loop {
        let Some(&byte) = data.get(i) else {
            // Ran off the end with a container still open, or before any value began.
            return Err(BencodeStructureError::Truncated);
        };

        match byte {
            // A container opens a level. Depth is now at least 1, so the "finished the
            // top-level value" check below is unreachable from here.
            b'd' | b'l' => {
                depth += 1;
                if depth > max_depth {
                    return Err(BencodeStructureError::TooDeep { limit: max_depth });
                }
                i += 1;
            }
            b'e' => {
                if depth == 0 {
                    return Err(BencodeStructureError::UnbalancedEnd);
                }
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            b'i' => {
                i = scan_integer(data, i)?;
                if depth == 0 {
                    return Ok(());
                }
            }
            b'0'..=b'9' => {
                i = scan_byte_string(data, i)?;
                if depth == 0 {
                    return Ok(());
                }
            }
            other => return Err(BencodeStructureError::InvalidByte(other)),
        }
    }
}

/// `i<digits>e` starting at `start`. Returns the index just past the `e`.
fn scan_integer(data: &[u8], start: usize) -> Result<usize, BencodeStructureError> {
    debug_assert_eq!(data[start], b'i');
    let mut i = start + 1;
    let mut digits = 0usize;
    // A leading '-' is legal; anything else non-digit before the 'e' is not.
    if data.get(i) == Some(&b'-') {
        i += 1;
    }
    while let Some(&b) = data.get(i) {
        match b {
            b'0'..=b'9' => {
                digits += 1;
                i += 1;
            }
            b'e' => {
                if digits == 0 {
                    return Err(BencodeStructureError::BadLength);
                }
                return Ok(i + 1);
            }
            _ => return Err(BencodeStructureError::BadLength),
        }
    }
    Err(BencodeStructureError::Truncated)
}

/// `<digits>:<len bytes>` starting at `start`. Returns the index just past the payload.
///
/// The declared length is checked against the bytes actually remaining, not merely parsed —
/// the other half of the pair of defects this module exists for.
fn scan_byte_string(data: &[u8], start: usize) -> Result<usize, BencodeStructureError> {
    let mut i = start;
    let mut len: usize = 0;
    let mut digits = 0usize;

    while let Some(&b) = data.get(i) {
        match b {
            b'0'..=b'9' => {
                // A length that cannot fit in a usize cannot fit in the input either.
                len = len
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((b - b'0') as usize))
                    .ok_or(BencodeStructureError::BadLength)?;
                digits += 1;
                i += 1;
                if len > data.len() {
                    // Bounded against the whole input, so an arbitrarily long run of digits
                    // cannot spin here either.
                    return Err(BencodeStructureError::BadLength);
                }
            }
            b':' => {
                if digits == 0 {
                    return Err(BencodeStructureError::BadLength);
                }
                let payload_start = i + 1;
                let end = payload_start
                    .checked_add(len)
                    .ok_or(BencodeStructureError::BadLength)?;
                if end > data.len() {
                    return Err(BencodeStructureError::Truncated);
                }
                return Ok(end);
            }
            _ => return Err(BencodeStructureError::BadLength),
        }
    }
    Err(BencodeStructureError::Truncated)
}
