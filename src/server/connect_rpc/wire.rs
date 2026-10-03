//! Bounded Connect protocol boundary; only protobuf messages enter the shared typed codec.
pub use crate::server::grpc::http1::bounded_headers;
use bytes::{Buf, Bytes};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::Frame;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tonic::{body::BoxBody, Code, Status};
pub const MAX_MESSAGE_BYTES: usize = crate::server::grpc::stream_codec::MAX_MESSAGE_BYTES;
pub const MAX_END_BYTES: usize = 16 * 1024;
pub const MAX_MESSAGES: usize = 256;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Unary,
    Stream,
}
impl Shape {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Unary => "application/proto",
            Self::Stream => "application/connect+proto",
        }
    }
}
pub fn shape(headers: &HeaderMap) -> Result<Shape, Status> {
    match unique(headers, "content-type")? {
        Some("application/proto") => Ok(Shape::Unary),
        Some("application/connect+proto") => Ok(Shape::Stream),
        _ => Err(Status::unimplemented(
            "binary protobuf Connect content type required",
        )),
    }
}
pub fn unique<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Status> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(Status::invalid_argument("duplicate reserved header"));
    }
    value
        .map(|value| {
            value
                .to_str()
                .map_err(|_| Status::invalid_argument("invalid reserved header"))
        })
        .transpose()
}
pub fn encoding(headers: &HeaderMap, name: &str) -> Result<bool, Status> {
    match unique(headers, name)? {
        None | Some("identity") => Ok(false),
        Some("gzip") => Ok(true),
        _ => Err(Status::unimplemented("supported encodings: identity, gzip")),
    }
}
pub fn accepts_gzip(headers: &HeaderMap, name: &str, fallback: bool) -> Result<bool, Status> {
    let Some(value) = unique(headers, name)? else {
        return Ok(fallback);
    };
    if value.len() > 1024 || value.split(',').count() > 16 {
        return Err(Status::invalid_argument("encoding list exceeds bound"));
    }
    Ok(value.split(',').any(|value| value.trim() == "gzip"))
}
pub fn timeout_ms(headers: &HeaderMap) -> Result<Option<u64>, Status> {
    let Some(value) = unique(headers, "connect-timeout-ms")? else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > 10 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Status::invalid_argument(
            "connect-timeout-ms must have 1..10 decimal digits",
        ));
    }
    let n = value
        .parse::<u64>()
        .map_err(|_| Status::invalid_argument("invalid Connect timeout"))?;
    if n == 0 {
        return Err(Status::invalid_argument("Connect timeout must be positive"));
    }
    Ok(Some(n))
}
pub fn inflate(input: &[u8], maximum: usize) -> Result<Vec<u8>, Status> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    flate2::bufread::MultiGzDecoder::new(input)
        .take((maximum + 1) as u64)
        .read_to_end(&mut output)
        .map_err(|_| Status::invalid_argument("invalid gzip member or checksum"))?;
    if output.len() > maximum {
        return Err(Status::resource_exhausted("expanded payload exceeds bound"));
    }
    Ok(output)
}
pub fn frame(flag: u8, payload: &[u8]) -> Bytes {
    let mut bytes = Vec::with_capacity(payload.len() + 5);
    bytes.push(flag);
    bytes.extend((payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    bytes.into()
}
pub fn body(bytes: Bytes) -> BoxBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}
pub async fn collect(body: BoxBody, maximum: usize) -> Result<Bytes, Status> {
    Limited::new(request_body(body), maximum + 1)
        .collect()
        .await
        .map_err(|_| Status::resource_exhausted("body exceeds bound or has HTTP trailers"))
        .map(|collected| collected.to_bytes())
}
pub fn request_body(body: BoxBody) -> BoxBody {
    StreamBody::new(futures::stream::try_unfold(body, |mut body| async move {
        let Some(frame) = body.frame().await else {
            return Ok(None);
        };
        let frame = frame?;
        if !frame.is_data() {
            return Err(Status::invalid_argument("Connect forbids HTTP trailers"));
        }
        Ok(Some((frame, body)))
    }))
    .boxed_unsync()
}
pub fn unary_request(bytes: &[u8], compressed: bool) -> Result<Bytes, Status> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Status::resource_exhausted("request exceeds 4 MiB"));
    }
    let expanded;
    let bytes = if compressed {
        expanded = inflate(bytes, MAX_MESSAGE_BYTES)?;
        expanded.as_slice()
    } else {
        bytes
    };
    Ok(frame(0, bytes))
}
pub fn streaming_request(bytes: &[u8], gzip: bool) -> Result<Bytes, Status> {
    if bytes.len() < 5 {
        return Err(Status::invalid_argument(
            "one complete request envelope required",
        ));
    }
    let length = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
    if length > MAX_MESSAGE_BYTES || bytes.len() > MAX_MESSAGE_BYTES + 5 {
        return Err(Status::resource_exhausted("request exceeds 4 MiB"));
    }
    if !matches!(bytes[0], 0 | 1) || (bytes[0] == 1 && !gzip) || length != bytes.len() - 5 {
        return Err(Status::invalid_argument(
            "one protobuf request envelope and EOF required",
        ));
    }
    // Expanded size/checksum is independently validated before the native decoder.
    if bytes[0] == 1 {
        inflate(&bytes[5..], MAX_MESSAGE_BYTES)?;
    }
    Ok(Bytes::copy_from_slice(bytes))
}
pub fn code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "ok",
        Code::Cancelled => "canceled",
        Code::Unknown => "unknown",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
    }
}
fn named_code(name: &str) -> Option<Code> {
    (1..=16)
        .map(Code::from_i32)
        .find(|code| code_name(*code) == name)
}
pub fn http_status(code: Code) -> StatusCode {
    StatusCode::from_u16(match code {
        Code::Ok => 200,
        Code::Cancelled => 499,
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => 400,
        Code::DeadlineExceeded => 504,
        Code::NotFound => 404,
        Code::AlreadyExists | Code::Aborted => 409,
        Code::PermissionDenied => 403,
        Code::ResourceExhausted => 429,
        Code::Unimplemented => 501,
        Code::Unavailable => 503,
        Code::Unauthenticated => 401,
        _ => 500,
    })
    .unwrap()
}
pub fn inferred_code(status: StatusCode) -> Code {
    match status.as_u16() {
        400 => Code::Internal,
        401 => Code::Unauthenticated,
        403 => Code::PermissionDenied,
        404 => Code::Unimplemented,
        429 | 502 | 503 | 504 => Code::Unavailable,
        _ => Code::Unknown,
    }
}
pub fn error_json(status: &Status) -> Value {
    json!({"code":code_name(status.code()),"message":crate::utils::truncate_for_llm(status.message(),4096)})
}
#[derive(Deserialize)]
struct Error {
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    details: Vec<Value>,
}
impl Error {
    fn status(self) -> Result<Status, Status> {
        let code =
            named_code(&self.code).ok_or_else(|| Status::internal("invalid Connect error code"))?;
        if self.message.len() > 4096 || !self.details.is_empty() {
            return Err(Status::internal(
                "error message bound or unsupported binary details",
            ));
        }
        Ok(Status::new(code, self.message))
    }
}
#[derive(Default, Clone, Debug)]
pub struct Metadata(pub BTreeMap<String, Vec<String>>);
impl<'de> Deserialize<'de> for Metadata {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Metadata;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bounded ASCII metadata arrays")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Metadata, A::Error> {
                let mut result = Metadata::default();
                let mut total = 0usize;
                let mut values = 0usize;
                while let Some(key) = map.next_key::<String>()? {
                    let key = key.to_ascii_lowercase();
                    if result.0.len() >= 16 || !metadata_key(&key) || result.0.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "metadata name/count/duplicate bound",
                        ));
                    }
                    let entries = map.next_value::<Vec<String>>()?;
                    if entries.is_empty() {
                        return Err(serde::de::Error::custom("metadata needs a value"));
                    }
                    for value in &entries {
                        values += 1;
                        total = total.saturating_add(key.len() + value.len());
                        if values > 32 || total > 8192 || !metadata_value(value) {
                            return Err(serde::de::Error::custom("metadata value/total bound"));
                        }
                    }
                    result.0.insert(key, entries);
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}
fn metadata_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
        && !key.ends_with("-bin")
        && !key.starts_with("connect-")
        && !key.starts_with("grpc-")
        && !key.starts_with("trailer-")
        && !matches!(
            key,
            "host"
                | "te"
                | "content-type"
                | "content-length"
                | "transfer-encoding"
                | "content-encoding"
                | "accept-encoding"
                | "connection"
                | "origin"
                | "accept"
                | "user-agent"
                | "x-user-agent"
                | "x-grpc-web"
        )
}
fn metadata_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.bytes().all(|byte| (32..=126).contains(&byte))
}
impl Metadata {
    pub fn json(&self) -> Value {
        json!(self.0)
    }
    pub fn from_action(value: &Value) -> Result<Self, Status> {
        let object = value
            .as_object()
            .ok_or_else(|| Status::invalid_argument("metadata must be an object"))?;
        let arrays: serde_json::Map<String, Value> = object
            .iter()
            .map(|(key, value)| (key.clone(), json!([value])))
            .collect();
        serde_json::from_value(Value::Object(arrays))
            .map_err(|_| Status::invalid_argument("metadata exceeds selected ASCII bounds"))
    }
    pub fn append_headers(&self, headers: &mut HeaderMap, prefix: &str) -> Result<(), Status> {
        for (key, values) in &self.0 {
            let name: HeaderName = format!("{prefix}{key}")
                .parse()
                .map_err(|_| Status::internal("invalid metadata name"))?;
            for value in values {
                headers.append(
                    name.clone(),
                    HeaderValue::from_str(value)
                        .map_err(|_| Status::internal("invalid metadata value"))?,
                );
            }
        }
        Ok(())
    }
    pub fn from_headers(headers: &HeaderMap, prefix: &str) -> Result<Self, Status> {
        let mut value = serde_json::Map::new();
        for key in headers.keys() {
            let name = key.as_str();
            let selected = if prefix.is_empty() {
                if metadata_key(name)
                    && !name.starts_with("access-control-")
                    && !matches!(
                        name,
                        "date" | "server" | "vary" | "retry-after" | "keep-alive"
                    )
                {
                    Some(name)
                } else {
                    None
                }
            } else {
                name.strip_prefix(prefix)
            };
            if let Some(name) = selected {
                let values = headers
                    .get_all(key)
                    .iter()
                    .map(|value| {
                        value
                            .to_str()
                            .map(Value::from)
                            .map_err(|_| Status::internal("non-ASCII metadata"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                value.insert(name.into(), Value::Array(values));
            }
        }
        serde_json::from_value(Value::Object(value))
            .map_err(|_| Status::resource_exhausted("response metadata exceeds bounds"))
    }
}
fn present_error<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Error>, D::Error> {
    Error::deserialize(d).map(Some)
}
#[derive(Deserialize)]
struct EndStream {
    #[serde(default, deserialize_with = "present_error")]
    error: Option<Error>,
    #[serde(default)]
    metadata: Metadata,
}
pub fn end_stream(bytes: &[u8]) -> Result<HeaderMap, Status> {
    if bytes.len() > MAX_END_BYTES {
        return Err(Status::resource_exhausted("EndStream exceeds 16 KiB"));
    }
    let end: EndStream =
        serde_json::from_slice(bytes).map_err(|_| Status::internal("invalid EndStream JSON"))?;
    let status = match end.error {
        Some(error) => error.status()?,
        None => Status::ok(""),
    };
    let mut headers = HeaderMap::new();
    end.metadata.append_headers(&mut headers, "")?;
    status.add_header(&mut headers)?;
    Ok(headers)
}
#[derive(Default)]
pub struct ResponseMetadata {
    pub leading: Metadata,
    pub trailing: Metadata,
    pub sealed: bool,
}
#[derive(Clone)]
pub struct RpcMetadata {
    pub request: Value,
    pub response: Arc<Mutex<ResponseMetadata>>,
}
impl RpcMetadata {
    pub fn new(headers: &HeaderMap) -> Result<Self, Status> {
        Ok(Self {
            request: Metadata::from_headers(headers, "")?.json(),
            response: Arc::new(Mutex::new(ResponseMetadata::default())),
        })
    }
    pub fn apply(&self, action: &Value) -> Result<(), Status> {
        let value = Metadata::from_action(&action["metadata"])?;
        let mut response = self.response.lock().unwrap_or_else(|e| e.into_inner());
        match action["phase"].as_str() {
            Some("headers") if !response.sealed => response.leading = value,
            Some("trailers") => response.trailing = value,
            _ => {
                return Err(Status::failed_precondition(
                    "leading metadata requires first response not yet emitted",
                ))
            }
        }
        Ok(())
    }
}
pub fn metadata_action() -> crate::llm::actions::ActionDefinition {
    use crate::llm::actions::{ActionDefinition, Parameter};
    ActionDefinition {name:"connect_rpc_metadata".into(),description:"Replace bounded ASCII response metadata; headers before first message, trailers before finish".into(),parameters:vec![Parameter{name:"phase".into(),type_hint:"string".into(),description:"headers or trailers".into(),required:true},Parameter{name:"metadata".into(),type_hint:"object".into(),description:"At most 16 lowercase ASCII keys/string values, 8 KiB; no reserved or binary names".into(),required:true}],example:json!({"type":"connect_rpc_metadata","phase":"trailers","metadata":{"x-note":"done"}}),log_template:None}
}
pub fn validate_metadata_action(action: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(action["phase"].as_str(), Some("headers" | "trailers")),
        "phase must be headers or trailers"
    );
    Metadata::from_action(&action["metadata"])?;
    Ok(())
}

enum Item {
    Data(u8, Bytes),
    Trailers(HeaderMap),
    Eof,
}
struct Reader {
    body: BoxBody,
    chunk: Bytes,
    trailers: Option<HeaderMap>,
    ended: bool,
    native: bool,
}
impl Reader {
    fn new(body: BoxBody, native: bool) -> Self {
        Self {
            body,
            chunk: Bytes::new(),
            trailers: None,
            ended: false,
            native,
        }
    }
    async fn available(&mut self) -> Result<bool, Status> {
        while self.chunk.is_empty() && !self.ended && self.trailers.is_none() {
            match self.body.frame().await {
                None => self.ended = true,
                Some(Err(status)) => return Err(status),
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => self.chunk = data,
                    Err(frame) => {
                        if !self.native {
                            return Err(Status::internal("Connect forbids HTTP trailers"));
                        }
                        self.trailers = Some(
                            frame
                                .into_trailers()
                                .map_err(|_| Status::internal("invalid native frame"))?,
                        );
                    }
                },
            }
        }
        Ok(!self.chunk.is_empty())
    }
    async fn read(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), Status> {
        let mut left = n;
        while left > 0 {
            if !self.available().await? {
                return Err(Status::internal("truncated envelope"));
            }
            let n = left.min(self.chunk.len());
            out.extend(&self.chunk[..n]);
            self.chunk.advance(n);
            left -= n;
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Item, Status> {
        if !self.available().await? {
            return Ok(match self.trailers.take() {
                Some(headers) => Item::Trailers(headers),
                None => Item::Eof,
            });
        }
        let mut prefix = Vec::with_capacity(5);
        self.read(5, &mut prefix).await?;
        let flag = prefix[0];
        if (self.native && !matches!(flag, 0 | 1)) || (!self.native && flag > 3) {
            return Err(Status::internal("unsupported envelope flag"));
        }
        let length = u32::from_be_bytes(prefix[1..5].try_into().unwrap()) as usize;
        let maximum = if !self.native && flag & 2 != 0 {
            MAX_END_BYTES
        } else {
            MAX_MESSAGE_BYTES
        };
        if length > maximum {
            return Err(Status::resource_exhausted("envelope prefix exceeds bound"));
        }
        let mut payload = Vec::with_capacity(length);
        self.read(length, &mut payload).await?;
        Ok(Item::Data(flag, payload.into()))
    }
}
pub fn client_stream_body(body: BoxBody, gzip: bool, complete: Arc<AtomicBool>) -> BoxBody {
    StreamBody::new(futures::stream::try_unfold(
        (Reader::new(body, false), 0usize, false),
        move |(mut reader, count, ended)| {
            let complete = complete.clone();
            async move {
                if ended {
                    return Ok(None);
                }
                let Item::Data(flag, payload) = reader.next().await? else {
                    return Err(Status::internal("response lacks final EndStream"));
                };
                if flag & 1 != 0 && !gzip {
                    return Err(Status::internal(
                        "compression flag without gzip negotiation",
                    ));
                }
                if flag & 2 != 0 {
                    let expanded;
                    let payload = if flag & 1 != 0 {
                        expanded = inflate(&payload, MAX_END_BYTES)?;
                        expanded.as_slice()
                    } else {
                        payload.as_ref()
                    };
                    let trailers = end_stream(payload)?;
                    if !matches!(reader.next().await?, Item::Eof) {
                        return Err(Status::internal("data follows EndStream"));
                    }
                    complete.store(true, Ordering::Relaxed);
                    Ok(Some((Frame::trailers(trailers), (reader, count, true))))
                } else {
                    if count >= MAX_MESSAGES {
                        return Err(Status::resource_exhausted("response exceeds 256 messages"));
                    }
                    Ok(Some((
                        Frame::data(frame(flag, &payload)),
                        (reader, count + 1, false),
                    )))
                }
            }
        },
    ))
    .boxed_unsync()
}
pub fn failure(status: Status, shape: Shape, close: bool) -> Response<BoxBody> {
    let value = if shape == Shape::Stream {
        json!({"error":error_json(&status)})
    } else {
        error_json(&status)
    };
    let bytes = serde_json::to_vec(&value).expect("bounded error JSON");
    let mut response = Response::new(body(if shape == Shape::Stream {
        frame(2, &bytes)
    } else {
        bytes.into()
    }));
    *response.status_mut() = if shape == Shape::Stream {
        StatusCode::OK
    } else {
        http_status(status.code())
    };
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static(if shape == Shape::Stream {
            shape.content_type()
        } else {
            "application/json"
        }),
    );
    if close {
        response
            .headers_mut()
            .insert("connection", HeaderValue::from_static("close"));
    }
    response
}
pub async fn server_response(
    response: Response<BoxBody>,
    shape: Shape,
    metadata: RpcMetadata,
) -> Result<Response<BoxBody>, Status> {
    let gzip = encoding(response.headers(), "grpc-encoding")?;
    if let Some(status) = Status::from_header_map(response.headers()) {
        return Ok(failure(status, shape, false));
    }
    let mut reader = Reader::new(response.into_body(), true);
    if shape == Shape::Unary {
        let mut result = None;
        let mut count = 0;
        let status = loop {
            match reader.next().await? {
                Item::Data(flag, payload) => {
                    count += 1;
                    if count > 1 {
                        return Err(Status::internal("unary requires one response"));
                    }
                    if flag == 1 && !gzip {
                        return Err(Status::internal("unnegotiated response compression"));
                    }
                    result = Some((flag, payload));
                }
                Item::Trailers(headers) => {
                    break Status::from_header_map(&headers)
                        .ok_or_else(|| Status::internal("missing native status"))?
                }
                Item::Eof => return Err(Status::internal("missing native status")),
            }
        };
        if !matches!(reader.next().await?, Item::Eof) {
            return Err(Status::internal("data after native status"));
        }
        if status.code() != Code::Ok {
            let mut response = failure(status, shape, false);
            let mut md = metadata.response.lock().unwrap_or_else(|e| e.into_inner());
            md.sealed = true;
            md.leading.append_headers(response.headers_mut(), "")?;
            md.trailing
                .append_headers(response.headers_mut(), "trailer-")?;
            return Ok(response);
        }
        let (flag, payload) =
            result.ok_or_else(|| Status::internal("unary requires one response"))?;
        let mut response = Response::new(body(payload));
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static(shape.content_type()),
        );
        if flag == 1 {
            response
                .headers_mut()
                .insert("content-encoding", HeaderValue::from_static("gzip"));
        }
        let mut md = metadata.response.lock().unwrap_or_else(|e| e.into_inner());
        md.sealed = true;
        md.leading.append_headers(response.headers_mut(), "")?;
        md.trailing
            .append_headers(response.headers_mut(), "trailer-")?;
        Ok(response)
    } else {
        // Poll once before sending headers, allowing opening controls to set leading metadata.
        let first = reader.next().await?;
        let mut response = Response::new(tonic::body::empty_body());
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static(shape.content_type()),
        );
        if gzip {
            response
                .headers_mut()
                .insert("connect-content-encoding", HeaderValue::from_static("gzip"));
        }
        {
            let mut md = metadata.response.lock().unwrap_or_else(|e| e.into_inner());
            md.sealed = true;
            md.leading.append_headers(response.headers_mut(), "")?;
        }
        *response.body_mut() = StreamBody::new(futures::stream::try_unfold(
            (reader, Some(first), 0usize, false),
            move |(mut reader, first, count, ended)| {
                let metadata = metadata.clone();
                async move {
                    if ended {
                        return Ok(None);
                    }
                    let item = match first {
                        Some(item) => item,
                        None => reader.next().await?,
                    };
                    match item {
                        Item::Data(flag, payload) => {
                            if count >= MAX_MESSAGES {
                                return Err(Status::resource_exhausted(
                                    "response count exceeds 256",
                                ));
                            }
                            Ok(Some((
                                Frame::data(frame(flag, &payload)),
                                (reader, None, count + 1, false),
                            )))
                        }
                        Item::Trailers(headers) => {
                            let status = Status::from_header_map(&headers)
                                .ok_or_else(|| Status::internal("missing native status"))?;
                            if !matches!(reader.next().await?, Item::Eof) {
                                return Err(Status::internal("data after native status"));
                            }
                            let md = metadata.response.lock().unwrap_or_else(|e| e.into_inner());
                            let mut end = json!({"metadata":md.trailing.0});
                            if status.code() != Code::Ok {
                                end["error"] = error_json(&status);
                            }
                            let bytes = serde_json::to_vec(&end)
                                .map_err(|_| Status::internal("EndStream encoding failed"))?;
                            if bytes.len() > MAX_END_BYTES {
                                return Err(Status::resource_exhausted("EndStream exceeds bound"));
                            }
                            Ok(Some((
                                Frame::data(frame(2, &bytes)),
                                (reader, None, count, true),
                            )))
                        }
                        Item::Eof => Err(Status::internal("missing native status")),
                    }
                }
            },
        ))
        .boxed_unsync();
        Ok(response)
    }
}
pub async fn client_request(
    mut request: Request<BoxBody>,
    shape: Shape,
) -> Result<Request<BoxBody>, Status> {
    let gzip = encoding(request.headers(), "grpc-encoding")?;
    let bytes = collect(
        std::mem::replace(request.body_mut(), tonic::body::empty_body()),
        MAX_MESSAGE_BYTES + 5,
    )
    .await?;
    let bytes = streaming_request(&bytes, gzip)?;
    let timeout = crate::server::grpc::streaming::timeout(
        request.headers(),
        std::time::Duration::from_secs(3600),
    )?;
    for name in [
        "grpc-encoding",
        "grpc-accept-encoding",
        "grpc-timeout",
        "content-type",
        "te",
        "user-agent",
    ] {
        request.headers_mut().remove(name);
    }
    request.headers_mut().insert(
        "content-type",
        HeaderValue::from_static(shape.content_type()),
    );
    request
        .headers_mut()
        .insert("connect-protocol-version", HeaderValue::from_static("1"));
    request.headers_mut().insert(
        "connect-timeout-ms",
        HeaderValue::from_str(&timeout.as_millis().max(1).to_string()).unwrap(),
    );
    let actual_gzip = bytes[0] == 1;
    if shape == Shape::Unary {
        *request.body_mut() = body(bytes.slice(5..));
        if actual_gzip {
            request
                .headers_mut()
                .insert("content-encoding", HeaderValue::from_static("gzip"));
        }
        request
            .headers_mut()
            .insert("accept-encoding", HeaderValue::from_static("gzip"));
    } else {
        *request.body_mut() = body(bytes);
        if gzip {
            request
                .headers_mut()
                .insert("connect-content-encoding", HeaderValue::from_static("gzip"));
        }
        request
            .headers_mut()
            .insert("connect-accept-encoding", HeaderValue::from_static("gzip"));
    }
    Ok(request)
}
pub async fn client_response(
    response: Response<BoxBody>,
    shape: Shape,
    complete: Arc<AtomicBool>,
) -> Result<Response<BoxBody>, Status> {
    if !bounded_headers(response.headers()) {
        return Err(Status::resource_exhausted("response headers exceed bound"));
    }
    let status = response.status();
    let ct = unique(response.headers(), "content-type")?.map(str::to_owned);
    let gzip = encoding(
        response.headers(),
        if shape == Shape::Unary {
            "content-encoding"
        } else {
            "connect-content-encoding"
        },
    )?;
    if shape == Shape::Stream && unique(response.headers(), "content-encoding")?.is_some() {
        return Err(Status::internal("stream HTTP content encoding excluded"));
    }
    let leading = Metadata::from_headers(response.headers(), "")?;
    let trailing = Metadata::from_headers(response.headers(), "trailer-")?;
    let (mut parts, body) = response.into_parts();
    parts.status = StatusCode::OK;
    parts.headers = HeaderMap::new();
    parts
        .headers
        .insert("content-type", HeaderValue::from_static("application/grpc"));
    leading.append_headers(&mut parts.headers, "")?;
    if shape == Shape::Unary || status != StatusCode::OK {
        let maximum = if status == StatusCode::OK {
            MAX_MESSAGE_BYTES
        } else {
            MAX_END_BYTES
        };
        let bytes = collect(body, maximum).await?;
        if bytes.len() > maximum {
            return Err(Status::resource_exhausted("response body exceeds bound"));
        }
        let expanded;
        let bytes = if gzip {
            expanded = inflate(&bytes, maximum)?;
            expanded.as_slice()
        } else {
            bytes.as_ref()
        };
        complete.store(true, Ordering::Relaxed);
        if status != StatusCode::OK {
            let fallback = Status::new(inferred_code(status), "Connect HTTP error");
            let error = if ct.as_deref() == Some("application/json") {
                serde_json::from_slice::<Error>(bytes)
                    .ok()
                    .and_then(|e| e.status().ok())
                    .unwrap_or(fallback)
            } else {
                fallback
            };
            error.add_header(&mut parts.headers)?;
            trailing.append_headers(&mut parts.headers, "")?;
            return Ok(Response::from_parts(parts, tonic::body::empty_body()));
        }
        if ct.as_deref() != Some(shape.content_type()) {
            return Err(Status::internal("expected binary Connect response"));
        }
        let mut headers = HeaderMap::new();
        trailing.append_headers(&mut headers, "")?;
        Status::ok("").add_header(&mut headers)?;
        let body = StreamBody::new(futures::stream::iter([
            Ok::<_, Status>(Frame::data(frame(0, bytes))),
            Ok(Frame::trailers(headers)),
        ]))
        .boxed_unsync();
        Ok(Response::from_parts(parts, body))
    } else {
        if ct.as_deref() != Some(shape.content_type()) {
            return Err(Status::internal("expected binary Connect stream"));
        }
        if gzip {
            parts
                .headers
                .insert("grpc-encoding", HeaderValue::from_static("gzip"));
        }
        Ok(Response::from_parts(
            parts,
            client_stream_body(body, gzip, complete),
        ))
    }
}
