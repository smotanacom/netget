//! Helpers shared by the OTLP suites: an in-process receiver started through `ServerForm`, a raw
//! HTTP/1.1 client that shows the exact status, headers and body, and builders for export
//! requests in both encodings.

#![allow(dead_code)]

use std::io::Write;
use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Not listening: any call that reaches the model fails with a transport error.
pub const DEAD_LLM: &str = "http://127.0.0.1:1";

pub async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, DEAD_LLM.to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(DEAD_LLM.to_string()))
        .await;
    state
}

pub async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("OTLP server #{} never bound a port", id.as_u32());
}

/// Start a receiver with the given handlers, and an instruction (empty: nothing reaches a
/// default instruction behind the handlers' backs).
pub async fn start_with(
    state: &AppState,
    instruction: &str,
    handlers: Vec<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "otlp".to_string(),
        port: Some(0),
        instruction: Some(instruction.to_string()),
        event_handlers: if handlers.is_empty() {
            None
        } else {
            Some(handlers)
        },
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create otlp server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    start_with(state, "", handlers).await
}

pub fn static_handler(actions: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "otlp_export",
        "handler": {"type": "static", "actions": actions}
    })
}

/// A response, as it crossed the wire.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", String::from_utf8_lossy(&self.body)))
    }
}

/// `POST path` with the given headers and body over a fresh connection; the whole response.
pub async fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    request(port, "POST", path, headers, body).await
}

pub async fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    // A refusal can close the socket while the body is still being written; what matters is
    // the response that comes back.
    let _ = stream.write_all(&bytes).await;
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(120), stream.read_to_end(&mut raw))
        .await
        .expect("no response within 120s");
    parse_reply(&raw)
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no complete response: {:?}", String::from_utf8_lossy(raw)));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    if chunked {
        body = dechunk(&body);
    }
    Reply {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(nl) = data.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&data[..nl]).trim(), 16).unwrap_or(0);
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[nl + 2..nl + 2 + size]);
        data = &data[nl + 4 + size..];
    }
}

pub const JSON: (&str, &str) = ("Content-Type", "application/json");
pub const PROTOBUF: (&str, &str) = ("Content-Type", "application/x-protobuf");
pub const GZIP: (&str, &str) = ("Content-Encoding", "gzip");

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

pub fn string_attribute(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
    }
}

/// A protobuf traces export: one resource with `service.name`, one span per name.
pub fn traces_protobuf(service: &str, span_names: &[&str]) -> Vec<u8> {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string_attribute("service.name", service)],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: span_names
                    .iter()
                    .map(|n| Span {
                        name: n.to_string(),
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

/// An OTLP/JSON traces export, as the specification writes it (lowerCamelCase, hex ids).
pub fn traces_json(service: &str, span_names: &[&str]) -> Vec<u8> {
    let spans: Vec<serde_json::Value> = span_names
        .iter()
        .map(|n| {
            serde_json::json!({
                "traceId": "5b8efff798038103d269b633813fc60c",
                "spanId": "eee19b7ec3c1b174",
                "name": n,
                "kind": 2,
                "startTimeUnixNano": "1544712660000000000",
                "endTimeUnixNano": "1544712661000000000"
            })
        })
        .collect();
    serde_json::json!({
        "resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": service}}
            ]},
            "scopeSpans": [{"scope": {"name": "test"}, "spans": spans}]
        }]
    })
    .to_string()
    .into_bytes()
}

/// Drain everything the server logged so far.
pub fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = rx.try_recv() {
        out.push(line);
    }
    out
}

/// Wait until a log line containing `needle` arrives; returns everything seen.
pub async fn wait_for_log(
    rx: &mut mpsc::UnboundedReceiver<String>,
    needle: &str,
    secs: u64,
) -> Vec<String> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(line)) => {
                let hit = line.contains(needle);
                seen.push(line);
                if hit {
                    return seen;
                }
            }
            _ => panic!("no log line containing {needle:?} within {secs}s; saw {seen:#?}"),
        }
    }
}
