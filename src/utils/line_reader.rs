//! One bounded, `\n`-terminated read, shared by the line-oriented text protocols.
//!
//! `tokio::io::AsyncBufReadExt::read_line` grows its `String` until it finds a newline and
//! caps nothing. A peer that connects and streams printable bytes without ever sending `\n`
//! therefore makes the server allocate without bound — a one-connection out-of-memory, from an
//! unauthenticated socket, before any model call. That defect was found in `ftp`, `irc` and
//! `smtp` and fixed three separate times, then found again in `imap`, `nntp` and `pop3`.
//!
//! Three copies of a `fill_buf`/`consume` loop is how the control-character filters ended up
//! with six different semantics before `utils::sanitize` existed, so this is one
//! implementation rather than a sixth. Each protocol still owns its own `MAX_*` constant and
//! its own refusal wording — the bound is a protocol decision, the reading is not.
//!
//! **The cap is on what is buffered, not on what is consumed.** A line longer than the cap is
//! reported as [`BoundedLine::TooLong`] with the offending bytes left unconsumed, because there
//! is no resynchronisation point: the peer is mid-line and the next byte is not the start of a
//! command. Every caller answers in its own vocabulary and closes.
//!
//! Decoding is lossy. `read_line` returns `ErrorKind::InvalidData` for any non-UTF-8 byte and
//! kills the session on it, which is wrong for protocols that advertise 8-bit transports
//! (`smtp`'s `8BITMIME`, `imap`'s literals). These bytes become an event payload and a log
//! line; nothing here is re-emitted on the wire, so a lossy decode loses nothing a peer can
//! observe.

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// The outcome of one bounded line read.
#[derive(Debug)]
pub enum BoundedLine {
    /// A complete line. The trailing `\n` (and `\r`, if any) is still attached; callers trim.
    Line(String),
    /// The peer closed, or sent a partial line and then closed. Half a command is not a
    /// command, so a trailing fragment at EOF is reported here rather than as a `Line`.
    Eof,
    /// `max_len` bytes went by with no `\n` in them.
    TooLong,
}

/// Read one `\n`-terminated line, buffering at most `max_len` bytes.
///
/// Returns the line and the number of bytes consumed from the socket, so a caller can feed
/// `update_connection_stats` exactly as `read_line`'s return value allowed.
///
/// `max_len` bounds the buffer the peer can make this function allocate. It is compared
/// *before* each `extend_from_slice`, so the high-water mark is `max_len`, not `max_len` plus
/// however much happened to be in the `BufReader` when the limit was crossed.
pub async fn read_bounded_line<R>(
    reader: &mut BufReader<R>,
    max_len: usize,
) -> std::io::Result<(BoundedLine, usize)>
where
    R: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut consumed = 0usize;

    loop {
        // `fill_buf` borrows the reader, so decide what to take and drop the borrow before
        // calling `consume`.
        let (newline_at, available_len) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok((BoundedLine::Eof, consumed));
            }
            (available.iter().position(|&b| b == b'\n'), available.len())
        };

        match newline_at {
            Some(idx) => {
                if buf.len() + idx + 1 > max_len {
                    return Ok((BoundedLine::TooLong, consumed));
                }
                let take = idx + 1;
                buf.extend_from_slice(&reader.buffer()[..take]);
                reader.consume(take);
                consumed += take;
                return Ok((
                    BoundedLine::Line(String::from_utf8_lossy(&buf).into_owned()),
                    consumed,
                ));
            }
            None => {
                // No newline in what is buffered. Refuse before growing past the cap: the
                // check has to come first, or the peer decides the allocation.
                if buf.len() + available_len > max_len {
                    return Ok((BoundedLine::TooLong, consumed));
                }
                buf.extend_from_slice(&reader.buffer()[..available_len]);
                reader.consume(available_len);
                consumed += available_len;
            }
        }
    }
}
