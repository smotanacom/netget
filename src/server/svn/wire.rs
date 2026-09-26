//! ra_svn framing: reading one tuple item off the wire.
//!
//! ra_svn is not a line protocol. A message is one **item**, and an item's extent is decided by
//! its own structure — matching parentheses and byte-counted strings — never by a newline. A
//! real `svn` client's very first message ends in a space and contains no newline at all, and a
//! counted string may contain any byte, newline included (the client's `ANONYMOUS` auth token
//! ends in one). Reading with `read_line` therefore blocks forever on message one; that is
//! exactly what it did here until this module existed.
//!
//! ```text
//! item    ::= word | number | string | list
//! word    ::= a run of non-space, non-paren bytes
//! number  ::= digits
//! string  ::= <byte-length> ":" <that many raw bytes>
//! list    ::= "(" item* ")"
//! ```
//!
//! # Bounds
//!
//! The parser is **iterative** — an explicit stack of partial lists, no recursion — because a
//! recursive descent over attacker-controlled nesting overflows the stack, and a Rust stack
//! overflow is a `SIGSEGV` against the guard page rather than a catchable panic: it takes the
//! whole NetGet process with it, not just the connection's task. This is the sixth wire format
//! in this repository to need that treatment; `src/utils/bencode.rs` is the precedent.
//!
//! Iterative parsing is not enough on its own, because the value it builds is recursive: see
//! [`MAX_TUPLE_DEPTH`] for what the depth bound protects.
//!
//! Three limits, all applied to the number the peer *declared* rather than to what has already
//! arrived:
//!
//! - [`MAX_TUPLE_DEPTH`] — `(` is one byte, so without it ~64 KiB of `(` is ~64 000 frames.
//! - `max_bytes` (the caller's per-message cap) — counted over the whole item, so a list of a
//!   million empty lists is refused on size as well as depth.
//! - a declared string length is checked against the remaining budget **before** anything is
//!   allocated for it, so `4294967295:` costs nothing.

use std::fmt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

/// How deeply lists may nest before the message is refused.
///
/// Real ra_svn traffic is three or four deep (a command's params, a dirent's date/author
/// sub-tuples). 64 leaves an order of magnitude of headroom.
///
/// The reader's own stack is an explicit `Vec`, so reading a deep tuple cannot overflow the
/// thread stack — but the `Item` it returns is a recursive enum, and everything downstream walks
/// it recursively: `Display` and `to_json` in `command_event_data`, and `Drop`. Without this cap
/// 32 000 closed lists — 64 000 bytes, inside `MAX_COMMAND_BYTES` — become a 32 000-deep `Item`
/// whose first walk overflows the stack and takes the process with it. That was verified with
/// `fuzz/fuzz_targets/svn_tuple.rs`: disabling this check makes its `depth_bomb` seed `SIGSEGV`.
pub const MAX_TUPLE_DEPTH: usize = 64;

/// One ra_svn item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A bare token: `success`, `get-dir`, `ANONYMOUS`.
    Word(String),
    /// A bare integer.
    Number(u64),
    /// A counted string, `<len>:<bytes>`. Any byte may appear in it.
    Str(Vec<u8>),
    /// `( … )`.
    List(Vec<Item>),
}

impl Item {
    /// The first element, if this is a non-empty list.
    pub fn head(&self) -> Option<&Item> {
        match self {
            Item::List(items) => items.first(),
            _ => None,
        }
    }

    /// This item as JSON, for event data the model reads.
    ///
    /// Strings are decoded lossily: event data carries text the model can reason about, never
    /// raw bytes or base64 (the project's action/event design rule).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Item::Word(w) => serde_json::Value::String(w.clone()),
            Item::Number(n) => serde_json::Value::from(*n),
            Item::Str(bytes) => serde_json::Value::String(String::from_utf8_lossy(bytes).into()),
            Item::List(items) => {
                serde_json::Value::Array(items.iter().map(Item::to_json).collect())
            }
        }
    }
}

impl fmt::Display for Item {
    /// Render back into ra_svn's own text, counted strings and all.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Item::Word(w) => f.write_str(w),
            Item::Number(n) => write!(f, "{n}"),
            Item::Str(bytes) => {
                write!(f, "{}:{}", bytes.len(), String::from_utf8_lossy(bytes))
            }
            Item::List(items) => {
                f.write_str("(")?;
                for item in items {
                    write!(f, " {item}")?;
                }
                f.write_str(" )")
            }
        }
    }
}

/// Why a message was refused. Every variant closes the connection; none reaches the peer as
/// text (the peer gets a [`crate::utils::WireFailure`] category, the log gets this).
#[derive(Debug)]
pub enum WireError {
    /// The peer hung up part-way through an item.
    UnexpectedEof,
    /// More than [`MAX_TUPLE_DEPTH`] nested lists.
    TooDeep,
    /// The item is larger than the caller's cap, or declares a string that would be.
    TooLarge { limit: u64 },
    /// Not ra_svn at all: a stray `)`, a string length that is not a number, and so on.
    Malformed(&'static str),
    /// The socket failed.
    Io(std::io::Error),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::UnexpectedEof => f.write_str("connection closed mid-tuple"),
            WireError::TooDeep => {
                write!(f, "tuple nested deeper than {MAX_TUPLE_DEPTH} lists")
            }
            WireError::TooLarge { limit } => {
                write!(f, "tuple larger than the {limit}-byte message cap")
            }
            WireError::Malformed(why) => write!(f, "malformed ra_svn tuple: {why}"),
            WireError::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        WireError::Io(e)
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t' | 0x0b | 0x0c)
}

/// A buffered reader that yields whole ra_svn items.
pub struct ItemReader<R> {
    inner: BufReader<R>,
    /// One byte of push-back: a word or a number ends when the byte *after* it is read, and
    /// that byte belongs to whatever comes next.
    pending: Option<u8>,
}

impl<R: AsyncRead + Unpin> ItemReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
            pending: None,
        }
    }

    async fn byte(&mut self) -> Result<Option<u8>, WireError> {
        if let Some(b) = self.pending.take() {
            return Ok(Some(b));
        }
        let mut one = [0u8; 1];
        match self.inner.read_exact(&mut one).await {
            Ok(_) => Ok(Some(one[0])),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(WireError::Io(e)),
        }
    }

    /// Read one complete item.
    ///
    /// Returns `Ok(None)` for a clean end of stream — the peer hung up *between* messages,
    /// which is how an svn session normally ends. EOF part-way through an item is
    /// [`WireError::UnexpectedEof`] instead, because those are different events and only one
    /// of them is an error.
    ///
    /// The second half of the tuple is the number of bytes consumed, for connection stats.
    pub async fn read_item(&mut self, max_bytes: u64) -> Result<Option<(Item, u64)>, WireError> {
        let mut consumed: u64 = 0;
        // Partial lists, outermost first. `stack.len()` is the current nesting depth.
        let mut stack: Vec<Vec<Item>> = Vec::new();

        loop {
            let Some(b) = self.byte().await? else {
                return if stack.is_empty() && consumed == 0 {
                    Ok(None)
                } else {
                    Err(WireError::UnexpectedEof)
                };
            };
            consumed += 1;
            if consumed > max_bytes {
                return Err(WireError::TooLarge { limit: max_bytes });
            }

            // Leading and separating whitespace. ra_svn writes a single space; a newline is
            // equally legal and NetGet's own replies end with one.
            if is_space(b) {
                continue;
            }

            let item = match b {
                b'(' => {
                    if stack.len() >= MAX_TUPLE_DEPTH {
                        return Err(WireError::TooDeep);
                    }
                    stack.push(Vec::new());
                    continue;
                }
                b')' => {
                    let Some(items) = stack.pop() else {
                        return Err(WireError::Malformed("closing paren with no list open"));
                    };
                    Item::List(items)
                }
                b'0'..=b'9' => self.number_or_string(b, &mut consumed, max_bytes).await?,
                _ => self.word(b, &mut consumed, max_bytes).await?,
            };

            match stack.last_mut() {
                Some(open) => open.push(item),
                // Nothing is open, so this item was the whole message.
                None => {
                    consumed += self.drain_buffered_whitespace();
                    return Ok(Some((item, consumed)));
                }
            }
        }
    }

    /// Consume trailing whitespace that has **already arrived**, and report how much.
    ///
    /// An item's extent ends at its last byte, so the space or newline a peer writes after it
    /// belongs to no message and would otherwise be attributed to the next one — leaving the
    /// connection's byte counter one short for as long as the peer stays quiet. This never
    /// awaits: it looks only at what the buffer is already holding, so a peer that sends
    /// nothing more cannot make it block.
    fn drain_buffered_whitespace(&mut self) -> u64 {
        let mut drained = 0u64;
        if let Some(b) = self.pending {
            if is_space(b) {
                self.pending = None;
                drained += 1;
            } else {
                return 0;
            }
        }
        loop {
            let take = self
                .inner
                .buffer()
                .iter()
                .take_while(|b| is_space(**b))
                .count();
            if take == 0 {
                return drained;
            }
            self.inner.consume(take);
            drained += take as u64;
        }
    }

    /// `<digits>` on its own is a number; `<digits>:` introduces a counted string.
    async fn number_or_string(
        &mut self,
        first: u8,
        consumed: &mut u64,
        max_bytes: u64,
    ) -> Result<Item, WireError> {
        let mut value: u64 = (first - b'0') as u64;
        let mut digits = 1usize;
        loop {
            let Some(b) = self.byte().await? else {
                // A bare number at end of stream: the caller decides whether that is an error.
                return Ok(Item::Number(value));
            };
            *consumed += 1;
            if *consumed > max_bytes {
                return Err(WireError::TooLarge { limit: max_bytes });
            }
            match b {
                b'0'..=b'9' => {
                    digits += 1;
                    // 20 digits is u64::MAX's width; anything longer is not a length or a
                    // revision, it is someone testing what happens.
                    if digits > 20 {
                        return Err(WireError::Malformed("number too long"));
                    }
                    value = value
                        .checked_mul(10)
                        .and_then(|v| v.checked_add((b - b'0') as u64))
                        .ok_or(WireError::Malformed("number overflows 64 bits"))?;
                }
                b':' => {
                    // Bound the DECLARED length against what is left of the budget, before
                    // allocating a single byte for it.
                    let remaining = max_bytes.saturating_sub(*consumed);
                    if value > remaining {
                        return Err(WireError::TooLarge { limit: max_bytes });
                    }
                    let len = value as usize;
                    let mut bytes = vec![0u8; len];
                    if len > 0 {
                        // `pending` is always empty here: the ':' was the last byte read.
                        debug_assert!(self.pending.is_none());
                        match self.inner.read_exact(&mut bytes).await {
                            Ok(_) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                                return Err(WireError::UnexpectedEof)
                            }
                            Err(e) => return Err(WireError::Io(e)),
                        }
                    }
                    *consumed += value;
                    return Ok(Item::Str(bytes));
                }
                other => {
                    self.pending = Some(other);
                    *consumed -= 1;
                    return Ok(Item::Number(value));
                }
            }
        }
    }

    /// A bare token, ended by whitespace or a paren.
    ///
    /// Deliberately lenient about what a word may contain: ra_svn's grammar says
    /// `[a-zA-Z][a-zA-Z0-9-]*`, but refusing anything else would turn a peer's typo into a
    /// dropped connection where the model could have answered, and NetGet's own tests speak
    /// tuples like `( stat /nonexistent )`.
    async fn word(
        &mut self,
        first: u8,
        consumed: &mut u64,
        max_bytes: u64,
    ) -> Result<Item, WireError> {
        let mut bytes = vec![first];
        loop {
            let Some(b) = self.byte().await? else {
                break;
            };
            if is_space(b) || b == b'(' || b == b')' {
                self.pending = Some(b);
                break;
            }
            *consumed += 1;
            if *consumed > max_bytes {
                return Err(WireError::TooLarge { limit: max_bytes });
            }
            bytes.push(b);
        }
        Ok(Item::Word(String::from_utf8_lossy(&bytes).into_owned()))
    }
}
