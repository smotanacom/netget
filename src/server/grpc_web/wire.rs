//! Bounded binary response framing at the seam before tonic decodes messages.
//! tonic-web owns server framing. Its client adapter is deliberately not used: it
//! buffers declared messages before tonic can enforce its limit and accepts incomplete trailers.
use bytes::{Buf, Bytes};
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tonic::{body::BoxBody, Status};

pub const MAX_MESSAGE_BYTES: usize = crate::server::grpc::stream_codec::MAX_MESSAGE_BYTES;
pub const MAX_TRAILER_BYTES: usize = 16 * 1024;
pub const MAX_TRAILER_FIELDS: usize = 32;
pub const MAX_MESSAGES: usize = 256;

/// Keep affine admission through the final EOF poll, not an inner iterator's early
/// end hint after yielding its last frame. Error and cancellation drop the guard too.
pub fn hold_body<G: Send + 'static>(body: BoxBody, guard: G) -> BoxBody {
    StreamBody::new(futures::stream::try_unfold(
        (body, guard),
        |(mut body, guard)| async move {
            let frame = body.frame().await.transpose()?;
            Ok::<_, Status>(frame.map(|frame| (frame, (body, guard))))
        },
    ))
    .boxed_unsync()
}

pub fn bounded_headers(headers: &HeaderMap) -> bool {
    headers.len() <= 64
        && headers
            .iter()
            .try_fold(0usize, |total, (name, value)| {
                total
                    .checked_add(name.as_str().len())?
                    .checked_add(value.as_bytes().len())?
                    .checked_add(4)
            })
            .is_some_and(|total| total <= 32768)
}

pub fn binary_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all("content-type").iter();
    values.next().is_some_and(|value| {
        matches!(
            value.to_str(),
            Ok("application/grpc-web") | Ok("application/grpc-web+proto")
        )
    }) && values.next().is_none()
}
pub fn status(headers: &HeaderMap) -> Result<u8, Status> {
    let mut values = headers.get_all("grpc-status").iter();
    let value = values
        .next()
        .ok_or_else(|| Status::internal("missing gRPC-Web status"))?;
    if values.next().is_some() {
        return Err(Status::internal("duplicate gRPC-Web status"));
    }
    let value = value
        .to_str()
        .map_err(|_| Status::internal("invalid gRPC-Web status"))?;
    if value.is_empty() || value.len() > 2 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Status::internal("invalid gRPC-Web status"));
    }
    let code: u8 = value
        .parse()
        .map_err(|_| Status::internal("invalid gRPC-Web status"))?;
    if code > 16 {
        return Err(Status::internal("invalid gRPC-Web status"));
    }
    Ok(code)
}
pub fn trailers(bytes: &[u8]) -> Result<HeaderMap, Status> {
    if bytes.len() > MAX_TRAILER_BYTES {
        return Err(Status::resource_exhausted(
            "gRPC-Web trailers exceed 16 KiB",
        ));
    }
    let mut map = HeaderMap::new();
    let mut remaining = bytes;
    let mut fields = 0;
    while !remaining.is_empty() {
        let (line, rest) = match remaining.windows(2).position(|window| window == b"\r\n") {
            Some(end) => (&remaining[..end], &remaining[end + 2..]),
            None => (remaining, &[][..]),
        };
        remaining = rest;
        fields += 1;
        if fields > MAX_TRAILER_FIELDS {
            return Err(Status::resource_exhausted(
                "gRPC-Web trailers exceed 32 fields",
            ));
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| Status::internal("malformed gRPC-Web trailer"))?;
        let name = &line[..colon];
        let value = line[colon + 1..]
            .strip_prefix(b" ")
            .unwrap_or(&line[colon + 1..]);
        if name.is_empty()
            || name.len() > 128
            || value.len() > 4096
            || !name.iter().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
            || !value.iter().all(|byte| (b' '..=b'~').contains(byte))
        {
            return Err(Status::internal("invalid gRPC-Web trailer field"));
        }
        let name = HeaderName::from_bytes(name)
            .map_err(|_| Status::internal("invalid gRPC-Web trailer name"))?;
        if matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length" | "content-type" | "te" | "host"
        ) || (matches!(
            name.as_str(),
            "grpc-status" | "grpc-message" | "grpc-status-details-bin"
        ) && map.contains_key(&name))
        {
            return Err(Status::internal("forbidden or duplicate gRPC-Web trailer"));
        }
        map.append(
            name,
            HeaderValue::from_bytes(value)
                .map_err(|_| Status::internal("invalid gRPC-Web trailer value"))?,
        );
    }
    status(&map)?;
    Ok(map)
}
struct Reader {
    body: BoxBody,
    chunk: Bytes,
    count: usize,
    ended: bool,
    gzip: bool,
    complete: Option<Arc<AtomicBool>>,
}
impl Reader {
    async fn available(&mut self) -> Result<bool, Status> {
        while self.chunk.is_empty() {
            let Some(frame) = self.body.frame().await else {
                return Ok(false);
            };
            self.chunk = frame?
                .into_data()
                .map_err(|_| Status::internal("gRPC-Web forbids HTTP trailers"))?;
        }
        Ok(true)
    }
    async fn read(&mut self, length: usize, output: &mut Vec<u8>) -> Result<(), Status> {
        let mut left = length;
        while left > 0 {
            if !self.available().await? {
                return Err(Status::internal("truncated gRPC-Web frame"));
            }
            let take = left.min(self.chunk.len());
            output.extend_from_slice(&self.chunk[..take]);
            self.chunk.advance(take);
            left -= take;
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Frame<Bytes>>, Status> {
        if self.ended {
            return Ok(None);
        }
        if !self.available().await? {
            return Err(Status::internal("gRPC-Web response ended without trailers"));
        }
        let mut prefix = Vec::with_capacity(5);
        self.read(5, &mut prefix).await?;
        let flag = prefix[0];
        if !matches!(flag, 0 | 1 | 128 | 129) || (flag == 129 && !self.gzip) {
            return Err(Status::internal("unsupported gRPC-Web frame flag"));
        }
        let length = u32::from_be_bytes(prefix[1..5].try_into().unwrap()) as usize;
        let trailer = flag & 128 != 0;
        let maximum = if trailer {
            MAX_TRAILER_BYTES
        } else {
            MAX_MESSAGE_BYTES
        };
        if length > maximum {
            return Err(Status::resource_exhausted(
                "gRPC-Web frame exceeds declared bound",
            ));
        }
        if !trailer && self.count >= MAX_MESSAGES {
            return Err(Status::resource_exhausted("gRPC-Web exceeds 256 messages"));
        }
        // Check the advertised length before reserving or copying its body.
        let mut frame = Vec::with_capacity(length + 5);
        frame.extend_from_slice(&prefix);
        self.read(length, &mut frame).await?;
        if trailer {
            let expanded;
            let payload = if flag == 129 {
                use std::io::Read;
                let decoder = flate2::bufread::MultiGzDecoder::new(&frame[5..]);
                let mut output = Vec::new();
                decoder
                    .take((MAX_TRAILER_BYTES + 1) as u64)
                    .read_to_end(&mut output)
                    .map_err(|_| Status::internal("invalid compressed gRPC-Web trailers"))?;
                if output.len() > MAX_TRAILER_BYTES {
                    return Err(Status::resource_exhausted(
                        "expanded gRPC-Web trailers exceed 16 KiB",
                    ));
                }
                expanded = output;
                expanded.as_slice()
            } else {
                &frame[5..]
            };
            let headers = trailers(payload)?;
            // A trailer is the final envelope. Check real EOF before tonic sees success:
            // otherwise a consumer can stop at status0 and silently accept trailing garbage.
            if self.available().await? {
                return Err(Status::internal("data follows gRPC-Web trailers"));
            }
            self.ended = true;
            if let Some(complete) = &self.complete {
                complete.store(true, Ordering::Relaxed);
            }
            Ok(Some(Frame::trailers(headers)))
        } else {
            self.count += 1;
            Ok(Some(Frame::data(frame.into())))
        }
    }
}
pub fn response_body(body: BoxBody) -> BoxBody {
    response_body_with_gzip(body, false)
}
pub fn response_body_with_gzip(body: BoxBody, gzip: bool) -> BoxBody {
    response_body_tracked(body, gzip, None)
}
pub fn response_body_tracked(
    body: BoxBody,
    gzip: bool,
    complete: Option<Arc<AtomicBool>>,
) -> BoxBody {
    let state = Reader {
        body,
        chunk: Bytes::new(),
        count: 0,
        ended: false,
        gzip,
        complete,
    };
    StreamBody::new(futures::stream::try_unfold(
        state,
        |mut reader| async move { Ok(reader.next().await?.map(|frame| (frame, reader))) },
    ))
    .boxed_unsync()
}
pub fn request_body(body: BoxBody) -> BoxBody {
    StreamBody::new(futures::stream::try_unfold(body, |mut body| async move {
        let Some(frame) = body.frame().await else {
            return Ok(None);
        };
        let frame = frame?;
        if !frame.is_data() {
            return Err(Status::invalid_argument(
                "gRPC-Web request forbids HTTP trailers",
            ));
        }
        Ok(Some((frame, body)))
    }))
    .boxed_unsync()
}
