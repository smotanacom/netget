//! Bounded framing for text responses from remote servers.

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::utils::line_reader::{read_bounded_line, BoundedLine};

/// Generous control/response-line limit, including CRLF.
pub const MAX_RESPONSE_LINE_BYTES: usize = 64 * 1024;
/// Total retained text of a dot-terminated POP3/NNTP response.
pub const MAX_MULTILINE_BYTES: usize = 8 * 1024 * 1024;
/// Absolute deadline after the first byte of a line or multiline body.
pub const RESPONSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Replace the line buffer with a complete bounded line. Partial EOF is an error;
/// callers must close on an error because the stream is no longer synchronized.
/// Like `read_line`, this must not be cancelled midway through a response.
pub async fn read_response_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut String,
) -> std::io::Result<usize> {
    read_response_line_with_timeout(reader, line, RESPONSE_DEADLINE).await
}

/// Variant with an explicit partial-line deadline (also useful to test slow peers).
pub async fn read_response_line_with_timeout<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut String,
    deadline: std::time::Duration,
) -> std::io::Result<usize> {
    line.clear();
    // Idle connections may wait indefinitely; a peer that starts a frame must finish it.
    if reader.fill_buf().await?.is_empty() {
        return Ok(0);
    }
    let result = tokio::time::timeout(deadline, read_bounded_line(reader, MAX_RESPONSE_LINE_BYTES))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "response line deadline exceeded",
            )
        })??;
    match result {
        (BoundedLine::Line(value), consumed) => {
            *line = value;
            Ok(consumed)
        }
        (BoundedLine::Eof, 0) => Ok(0),
        (BoundedLine::Eof, _) => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "server closed midway through a response line",
        )),
        (BoundedLine::TooLong, _) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("response line exceeds the {MAX_RESPONSE_LINE_BYTES}-byte cap"),
        )),
    }
}

/// A solicited greeting/response must begin and finish within the handshake deadline.
pub async fn read_expected_response_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut String,
) -> std::io::Result<usize> {
    tokio::time::timeout(RESPONSE_DEADLINE, read_response_line(reader, line))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "expected response deadline exceeded",
            )
        })?
}

/// Read through the exact `.` terminator. Preserve content whitespace, undo dot
/// stuffing, and reject EOF or a size violation instead of reporting a partial reply.
pub async fn read_dot_response<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    response: String,
) -> std::io::Result<String> {
    tokio::time::timeout(RESPONSE_DEADLINE, read_dot_response_inner(reader, response))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "multiline response deadline exceeded",
            )
        })?
}

async fn read_dot_response_inner<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    mut response: String,
) -> std::io::Result<String> {
    if response.len() > MAX_MULTILINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "multiline response exceeds the byte cap",
        ));
    }
    let mut line = String::new();
    loop {
        if read_response_line(reader, &mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "server closed before the multiline response terminator",
            ));
        }
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "." {
            return Ok(response);
        }
        let line = if line.starts_with("..") {
            &line[1..]
        } else {
            line
        };
        if line.len() + 1 > MAX_MULTILINE_BYTES.saturating_sub(response.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("multiline response exceeds the {MAX_MULTILINE_BYTES}-byte cap"),
            ));
        }
        response.push('\n');
        response.push_str(line);
    }
}

/// An absolute deadline for an explicitly started binary frame. No clock runs
/// between frames or while the caller handles the completed event.
pub struct FrameReader<R> {
    inner: R,
    deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}
impl<R> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            deadline: None,
        }
    }
    pub fn start_frame(&mut self, duration: std::time::Duration) {
        self.deadline = Some(Box::pin(tokio::time::sleep(duration)));
    }
    pub fn end_frame(&mut self) {
        self.deadline = None;
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for FrameReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::future::Future;
        let this = self.get_mut();
        if this
            .deadline
            .as_mut()
            .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready())
        {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "partial response frame deadline exceeded",
            )));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}
