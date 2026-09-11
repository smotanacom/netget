//! Framing rules for a single IRC protocol line, shared by the IRC server and the IRC client.
//!
//! IRC is line-oriented: a message is terminated by CRLF and the peer reads whatever follows
//! as a *new* message. Everything in here exists because both directions of that framing are
//! attacker-influenced — the peer chooses what it sends us, and the **model** chooses the text
//! we put on the wire, which is then relayed verbatim to other humans in a channel.

use anyhow::Result;
use tokio::io::AsyncBufReadExt;
use tracing::warn;

/// RFC 1459 §2.3.1: a message is at most 512 bytes *including* the trailing CRLF.
///
/// A longer line is not "a long message" — a real client or server truncates or drops it, so
/// the tail the model wrote is lost either way. Truncating here at least keeps the line
/// well-formed and keeps the loss in our own log.
pub(crate) const MAX_IRC_LINE: usize = 512;

/// Largest line this implementation will accumulate from a peer before giving up on it.
///
/// The protocol limit is [`MAX_IRC_LINE`], but IRCv3 message tags raise the wire maximum to
/// 8191 bytes of tags plus the 512-byte message. Accepting up to that and refusing beyond it
/// is generous to real peers and still bounded, which is the whole point:
/// [`tokio::io::AsyncBufReadExt::read_line`] grows its buffer until it finds a newline, so an
/// unauthenticated peer that connects and streams bytes with no `\n` is a one-connection
/// out-of-memory.
pub(crate) const MAX_IRC_READ_LINE: usize = 8704;

/// The result of trying to read one line from an IRC peer.
pub(crate) enum IrcLine {
    /// A complete line. The trailing CRLF (or bare LF) is still attached; the caller trims it.
    Line(String),
    /// The peer closed the connection.
    Eof,
    /// The peer sent [`MAX_IRC_READ_LINE`] bytes with no newline among them.
    TooLong,
}

/// Read one CRLF-terminated IRC line, refusing to buffer more than `max_len` bytes.
///
/// Returns the number of bytes consumed from the socket alongside the line so the caller can
/// keep the connection's `↓` counter accurate exactly as `read_line` allowed.
///
/// There is nothing to resynchronise with after a `TooLong`: the peer is mid-line and we have
/// thrown its prefix away, so every caller closes the connection instead of trying to recover.
pub(crate) async fn read_irc_line<R>(
    reader: &mut tokio::io::BufReader<R>,
    max_len: usize,
) -> std::io::Result<(IrcLine, usize)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // `fill_buf` borrows the reader, so decide what to take and drop the borrow before
        // calling `consume`.
        let (newline_at, available_len) = {
            let available = reader.fill_buf().await?;
            (available.iter().position(|&b| b == b'\n'), available.len())
        };

        if available_len == 0 {
            // EOF. A trailing fragment with no newline is not a message; the peer hung up
            // mid-line, which is the same as hanging up.
            return Ok((IrcLine::Eof, buf.len()));
        }

        let take = match newline_at {
            Some(idx) => idx + 1,
            None => available_len,
        };

        if buf.len() + take > max_len {
            reader.consume(take);
            return Ok((IrcLine::TooLong, buf.len() + take));
        }

        {
            let available = reader.fill_buf().await?;
            buf.extend_from_slice(&available[..take]);
        }
        reader.consume(take);

        if newline_at.is_some() {
            let consumed = buf.len();
            // `from_utf8_lossy` rather than a hard error: IRC predates any encoding agreement
            // and a stray non-UTF-8 byte in someone's chat text is not a reason to drop the
            // link without a word.
            return Ok((
                IrcLine::Line(String::from_utf8_lossy(&buf).into_owned()),
                consumed,
            ));
        }
    }
}

/// Reject CR, LF or NUL inside a piece of text that is about to be interpolated into a line.
///
/// This is the IRC form of the FTP response-splitting defect (`src/server/ftp/actions.rs`),
/// and it is worse here because the text is usually a **chat message the model composed**: a
/// `PRIVMSG` body containing `\r\n` does not produce a longer message, it forges a second
/// command from the same source. `"hi\r\nPRIVMSG #ops :op me"` sent through `send_irc_privmsg`
/// is one message and one forged command; through the client's `send_privmsg` it is a message
/// and a forged `QUIT`, `JOIN` or `NICK`. NUL is rejected with them because RFC 1459 §2.3.1
/// forbids it in a message outright.
///
/// Erroring rather than stripping is deliberate: a handler that meant to send two messages
/// should emit two actions, and silently rewriting its text would hide the mistake — the same
/// reasoning `reject_line_breaks` gives in FTP.
pub(crate) fn reject_line_breaks(field: &str, value: &str) -> Result<()> {
    if let Some(pos) = value.find(['\r', '\n', '\0']) {
        return Err(anyhow::anyhow!(
            "IRC '{field}' must not contain CR, LF or NUL (found one at byte {pos}): an IRC \
             message is terminated by CRLF, so an embedded line break forges a second message \
             rather than continuing this one. Send one action per message."
        ));
    }
    Ok(())
}

/// Reject anything that cannot stand where IRC expects a single word.
///
/// Nicknames, channels, targets, sources and server names are positional parameters separated
/// by spaces, and a leading `:` starts the trailing parameter that runs to end of line. So a
/// `target` of `"alice :hello"` does not send to a strangely-named user — it shifts every
/// later parameter along and the client parses a message nobody wrote. Callers pass fields
/// that land in word position; the trailing parameter (a message body, a quit reason) only
/// goes through [`reject_line_breaks`], because spaces are exactly what belongs there.
pub(crate) fn reject_not_a_word(field: &str, value: &str) -> Result<()> {
    reject_line_breaks(field, value)?;
    if value.is_empty() {
        return Err(anyhow::anyhow!(
            "IRC '{field}' must not be empty: it is a positional parameter, and an empty one \
             shifts every parameter after it."
        ));
    }
    if value.contains(' ') {
        return Err(anyhow::anyhow!(
            "IRC '{field}' must not contain a space: IRC parameters are space-separated, so a \
             space here shifts every later parameter and the peer parses a different message."
        ));
    }
    if value.starts_with(':') {
        return Err(anyhow::anyhow!(
            "IRC '{field}' must not start with ':': a leading colon marks the trailing \
             parameter, which runs to the end of the line."
        ));
    }
    Ok(())
}

/// Cap a finished, CRLF-terminated line at [`MAX_IRC_LINE`] bytes.
///
/// Truncation rather than rejection, because this is what every real IRC daemon does with an
/// over-long line and because dropping the message entirely is a worse answer for a chat
/// protocol than delivering the first 500 bytes of it. The cut is on a `char` boundary — the
/// byte-index slicing this codebase has been bitten by before would panic on any multi-byte
/// character straddling byte 510, i.e. on ordinary non-ASCII chat text.
///
/// `what` names the action for the log, so an operator can see which of the model's messages
/// was shortened rather than wondering why a client saw half a sentence.
pub(crate) fn cap_line(what: &str, line: String) -> Vec<u8> {
    if line.len() <= MAX_IRC_LINE {
        return line.into_bytes();
    }

    let body = line.strip_suffix("\r\n").unwrap_or(&line);
    let mut end = MAX_IRC_LINE - 2;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }

    warn!(
        "IRC {} produced a {}-byte line; truncated to the RFC 1459 limit of {} bytes \
         (including CRLF)",
        what,
        line.len(),
        MAX_IRC_LINE
    );

    let mut out = String::with_capacity(MAX_IRC_LINE);
    out.push_str(&body[..end]);
    out.push_str("\r\n");
    out.into_bytes()
}
