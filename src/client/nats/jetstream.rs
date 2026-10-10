//! JetStream for NetGet's NATS client, used when the server's `INFO` advertises it.
//!
//! The client subscribes once to its own inbox (`<inbox>.>`, sid `js`) and addresses every
//! JetStream request's reply to a subject under it that names what was asked, so each reply is
//! raised as a typed event without any request table: `<inbox>.api.<OP>.<stream>.<consumer>`
//! for API calls, `<inbox>.pub` for publish acks, `<inbox>.fetch.<stream>.<consumer>` for a
//! pull's end-of-batch status. Pulled messages arrive on their own subjects carrying a
//! `$JS.ACK.…` reply, which is parsed into their stream, consumer and sequences.
use crate::llm::actions::client_trait::ClientActionResult;
use crate::llm::actions::{ActionDefinition, Parameter};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::{Event, EventType};
use crate::server::nats::actions::{check_token, decode_payload, encode_headers};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The sid of the client's inbox subscription.
pub const INBOX_SID: &str = "js";
/// Most messages one fetch asks for.
pub const MAX_BATCH: u64 = 256;
/// How long a fetch waits when the action names no expiry.
pub const DEFAULT_EXPIRES_MS: u64 = 5000;

const OPERATIONS: [&str; 13] = [
    "INFO",
    "STREAM.CREATE",
    "STREAM.UPDATE",
    "STREAM.INFO",
    "STREAM.DELETE",
    "STREAM.PURGE",
    "STREAM.NAMES",
    "STREAM.LIST",
    "CONSUMER.CREATE",
    "CONSUMER.INFO",
    "CONSUMER.DELETE",
    "CONSUMER.NAMES",
    "CONSUMER.LIST",
];

/// A fresh inbox prefix for one client: its id and the moment it connected.
pub fn inbox_for(client_id: impl std::fmt::Display) -> String {
    let nanos = crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("_INBOX.ngjs{client_id}x{nanos:x}")
}

fn p(name: &str, kind: &str, description: &str, required: bool) -> Parameter {
    Parameter {
        name: name.into(),
        type_hint: kind.into(),
        description: description.into(),
        required,
    }
}

fn a(
    name: &str,
    description: &str,
    parameters: Vec<Parameter>,
    example: Value,
    log: &str,
) -> ActionDefinition {
    ActionDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
        example,
        log_template: Some(LogTemplate::new().with_info(log)),
    }
}

fn api_action() -> ActionDefinition {
    a(
        "nats_js_api",
        "Call the JetStream API; the answer arrives as nats_js_response. Needs a server whose INFO advertises jetstream.",
        vec![
            p("operation", "string", "INFO, STREAM.CREATE/UPDATE/INFO/DELETE/PURGE/NAMES/LIST, CONSUMER.CREATE/INFO/DELETE/NAMES/LIST", true),
            p("stream", "string", "Stream name, for STREAM.* (except NAMES/LIST) and every CONSUMER.*", false),
            p("consumer", "string", "Consumer name, for CONSUMER.CREATE/INFO/DELETE", false),
            p("request", "object", "The JSON body, e.g. {\"name\":\"ORDERS\",\"subjects\":[\"orders.*\"]} or {\"stream_name\":\"ORDERS\",\"config\":{\"durable_name\":\"proc\",\"ack_policy\":\"explicit\"}}", false),
        ],
        json!({"type":"nats_js_api","operation":"STREAM.CREATE","stream":"ORDERS","request":{"name":"ORDERS","subjects":["orders.*"]}}),
        "-> JetStream {operation} {stream}",
    )
}

fn publish_action() -> ActionDefinition {
    a(
        "nats_js_publish",
        "Publish to a stream's subject and wait for the stream's ack (nats_js_publish_ack).",
        vec![
            p("subject", "string", "A subject a stream captures", true),
            p("payload", "string", "The message body as text (or hex)", true),
            p("encoding", "string", "utf8 (default) or hex", false),
            p("headers", "object", "Optional message headers", false),
        ],
        json!({"type":"nats_js_publish","subject":"orders.new","payload":"{\"id\":1}"}),
        "-> JetStream publish {subject}",
    )
}

fn fetch_action() -> ActionDefinition {
    a(
        "nats_js_fetch",
        "Pull up to batch messages from a pull consumer; each arrives as nats_js_message, and nats_js_fetch_done ends a short batch.",
        vec![
            p("stream", "string", "The JetStream stream's name", true),
            p("consumer", "string", "Durable consumer name", true),
            p("batch", "number", "Most messages wanted (1..=256, default 1)", false),
            p("no_wait", "boolean", "Answer at once, even with nothing (default false)", false),
            p("expires_ms", "number", "How long the server may hold the pull (default 5000)", false),
        ],
        json!({"type":"nats_js_fetch","stream":"ORDERS","consumer":"proc","batch":10}),
        "-> JetStream fetch {stream}/{consumer}",
    )
}

fn ack_action() -> ActionDefinition {
    a(
        "nats_js_ack",
        "Acknowledge a pulled message on the ack_subject its nats_js_message carried.",
        vec![
            p(
                "ack_subject",
                "string",
                "The message's ack_subject ($JS.ACK.…)",
                true,
            ),
            p(
                "kind",
                "string",
                "ack (default), nak, progress or term",
                false,
            ),
        ],
        json!({"type":"nats_js_ack","ack_subject":"$JS.ACK.ORDERS.proc.1.1.1.1700000000000000000.0"}),
        "-> JetStream {kind} {ack_subject}",
    )
}

pub fn actions() -> Vec<ActionDefinition> {
    vec![api_action(), publish_action(), fetch_action(), ack_action()]
}

pub static RESPONSE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_response",
        "The server answered a JetStream API call.",
        fetch_action().example.clone(),
    )
    .with_parameters(vec![
        p("operation", "string", "The operation called", true),
        p("stream", "string", "The stream named, if any", false),
        p("consumer", "string", "The consumer named, if any", false),
        p("response", "object", "The response JSON", true),
        p(
            "error",
            "object|null",
            "{code, err_code, description} when the server refused",
            true,
        ),
    ])
    .with_actions(actions())
});

pub static PUBLISH_ACK_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_publish_ack",
        "The stream acknowledged (or refused) a nats_js_publish.",
        fetch_action().example.clone(),
    )
    .with_parameters(vec![
        p("stream", "string", "The stream that stored it", false),
        p("seq", "number", "Its stream sequence", false),
        p(
            "duplicate",
            "boolean",
            "The server saw it as a duplicate",
            false,
        ),
        p(
            "error",
            "object|null",
            "{code, err_code, description} when refused",
            true,
        ),
    ])
    .with_actions(actions())
});

pub static MESSAGE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_message",
        "A message delivered by a fetch. Acknowledge it with nats_js_ack on its ack_subject.",
        ack_action().example.clone(),
    )
    .with_parameters(vec![
        p("subject", "string", "The message's subject", true),
        p(
            "payload",
            "string",
            "Body (text, or hex per payload_encoding)",
            true,
        ),
        p("payload_encoding", "string", "How payload is encoded: utf8 or hex", true),
        p("headers", "object", "Message headers", true),
        p("stream", "string", "The JetStream stream's name", true),
        p("consumer", "string", "The pull consumer's name", true),
        p("stream_seq", "number", "Stream sequence", true),
        p("consumer_seq", "number", "Consumer sequence", true),
        p("delivered", "number", "How many times it has been delivered", true),
        p(
            "pending",
            "number",
            "Messages still pending after this one",
            true,
        ),
        p("ack_subject", "string", "Where to acknowledge it", true),
    ])
    .with_actions(actions())
});

pub static FETCH_DONE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_fetch_done",
        "A fetch ended short of its batch: 404 nothing pending (no_wait), 408 the pull expired.",
        fetch_action().example.clone(),
    )
    .with_parameters(vec![
        p("stream", "string", "The JetStream stream's name", true),
        p("consumer", "string", "The pull consumer's name", true),
        p("status", "number", "404, 408, 409 …", true),
        p("description", "string", "The server's status text", true),
    ])
    .with_actions(actions())
});

pub fn event_types() -> Vec<EventType> {
    vec![
        RESPONSE_EVENT.clone(),
        PUBLISH_ACK_EVENT.clone(),
        MESSAGE_EVENT.clone(),
        FETCH_DONE_EVENT.clone(),
    ]
}

fn name(v: &Value, field: &str) -> Result<Option<String>> {
    match v.get(field).and_then(Value::as_str) {
        None => Ok(None),
        Some(s) => {
            check_token(s, field)?;
            ensure!(
                !s.contains(['.', '*', '>']),
                "{field} must not contain '.', '*' or '>'"
            );
            Ok(Some(s.to_string()))
        }
    }
}

fn pub_frame(
    subject: &str,
    reply: &str,
    headers: Option<&serde_json::Map<String, Value>>,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match headers.filter(|h| !h.is_empty()) {
        Some(h) => {
            let block = encode_headers(h)?;
            out.extend(
                format!(
                    "HPUB {subject} {reply} {} {}\r\n",
                    block.len(),
                    block.len() + payload.len()
                )
                .into_bytes(),
            );
            out.extend(block);
        }
        None => out.extend(format!("PUB {subject} {reply} {}\r\n", payload.len()).into_bytes()),
    }
    out.extend(payload);
    out.extend(b"\r\n");
    Ok(out)
}

/// The frame for one JetStream action; `None` when the action is not JetStream's.
pub fn execute(inbox: &str, action: &Value) -> Option<Result<ClientActionResult>> {
    let kind = action["type"].as_str()?;
    let built = match kind {
        "nats_js_api" => (|| {
            let op = action["operation"].as_str().context("operation required")?;
            ensure!(
                OPERATIONS.contains(&op),
                "unsupported JetStream operation {op}"
            );
            let stream = name(action, "stream")?;
            let consumer = name(action, "consumer")?;
            let needs_stream = !matches!(op, "INFO" | "STREAM.NAMES" | "STREAM.LIST");
            ensure!(!needs_stream || stream.is_some(), "{op} needs a stream");
            ensure!(
                !matches!(op, "CONSUMER.INFO" | "CONSUMER.DELETE") || consumer.is_some(),
                "{op} needs a consumer"
            );
            let mut subject = format!("$JS.API.{op}");
            for part in [&stream, &consumer].into_iter().flatten() {
                subject.push('.');
                subject.push_str(part);
            }
            let reply = format!(
                "{inbox}.api.{}.{}.{}",
                op.replace('.', "_"),
                stream.as_deref().unwrap_or("_"),
                consumer.as_deref().unwrap_or("_")
            );
            let body = match action.get("request").filter(|r| !r.is_null()) {
                Some(r) => {
                    ensure!(r.is_object(), "request must be an object");
                    r.to_string()
                }
                None => String::new(),
            };
            pub_frame(&subject, &reply, None, body.as_bytes())
        })(),
        "nats_js_publish" => (|| {
            let subject = action["subject"].as_str().context("subject required")?;
            check_token(subject, "subject")?;
            ensure!(
                !subject.starts_with("$JS."),
                "publish to a stream's subject, not the API"
            );
            let payload = decode_payload(action)?;
            pub_frame(
                subject,
                &format!("{inbox}.pub"),
                action["headers"].as_object(),
                &payload,
            )
        })(),
        "nats_js_fetch" => (|| {
            let stream = name(action, "stream")?.context("stream required")?;
            let consumer = name(action, "consumer")?.context("consumer required")?;
            let batch = action["batch"].as_u64().unwrap_or(1);
            ensure!(
                (1..=MAX_BATCH).contains(&batch),
                "batch must be 1..={MAX_BATCH}"
            );
            let no_wait = action["no_wait"].as_bool().unwrap_or(false);
            let mut request = json!({"batch": batch});
            if no_wait {
                request["no_wait"] = json!(true);
            } else {
                let ms = action["expires_ms"]
                    .as_u64()
                    .unwrap_or(DEFAULT_EXPIRES_MS)
                    .clamp(1, 3_600_000);
                request["expires"] = json!(ms * 1_000_000);
            }
            pub_frame(
                &format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.{consumer}"),
                &format!("{inbox}.fetch.{stream}.{consumer}"),
                None,
                request.to_string().as_bytes(),
            )
        })(),
        "nats_js_ack" => (|| {
            let subject = action["ack_subject"]
                .as_str()
                .context("ack_subject required")?;
            check_token(subject, "ack_subject")?;
            ensure!(
                subject.starts_with("$JS.ACK."),
                "ack_subject must be a $JS.ACK subject"
            );
            let body: &[u8] = match action["kind"].as_str().unwrap_or("ack") {
                "ack" => b"+ACK",
                "nak" => b"-NAK",
                "progress" => b"+WPI",
                "term" => b"+TERM",
                other => bail!("kind must be ack, nak, progress or term, not {other}"),
            };
            let mut out = format!("PUB {subject} {}\r\n", body.len()).into_bytes();
            out.extend(body);
            out.extend(b"\r\n");
            Ok(out)
        })(),
        _ => return None,
    };
    Some(built.map(ClientActionResult::SendData))
}

/// The status a status-only message carries (`NATS/1.0 404 No Messages`).
pub fn status_of(headers: &BTreeMap<String, String>) -> Option<(u64, String)> {
    let code = headers.get("Status")?.parse().ok()?;
    Some((
        code,
        headers.get("Description").cloned().unwrap_or_default(),
    ))
}

fn error_of(body: &Value) -> Value {
    body.get("error").cloned().unwrap_or(Value::Null)
}

/// The event an incoming message is, when it belongs to JetStream.
pub fn event_for(
    inbox: &str,
    subject: &str,
    reply_to: Option<&str>,
    headers: &BTreeMap<String, String>,
    payload: &[u8],
) -> Option<Event> {
    if let Some(ack) = reply_to.filter(|r| r.starts_with("$JS.ACK.")) {
        let t: Vec<&str> = ack.split('.').collect();
        // $JS.ACK.<stream>.<consumer>.<delivered>.<sseq>.<cseq>.<ts>.<pending>, or the v2 form
        // with a domain and account hash in front.
        let base = if t.len() >= 12 { 4 } else { 2 };
        let n = |i: usize| {
            t.get(base + i)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let (text, encoding) = crate::client::nats::payload_for_event(payload);
        return Some(Event::new(
            &MESSAGE_EVENT,
            json!({"subject": subject, "payload": text, "payload_encoding": encoding, "headers": headers,
                   "stream": t.get(base).copied().unwrap_or_default(),
                   "consumer": t.get(base + 1).copied().unwrap_or_default(),
                   "delivered": n(2), "stream_seq": n(3), "consumer_seq": n(4), "pending": n(6),
                   "ack_subject": ack}),
        ));
    }
    let rest = subject.strip_prefix(inbox)?.strip_prefix('.')?;
    let parts: Vec<&str> = rest.split('.').collect();
    let body: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let opt = |s: &str| (s != "_").then(|| s.to_string());
    Some(match parts.as_slice() {
        ["api", op, stream, consumer] => Event::new(
            &RESPONSE_EVENT,
            json!({"operation": op.replace('_', "."), "stream": opt(stream), "consumer": opt(consumer),
                   "error": error_of(&body), "response": body}),
        ),
        ["pub"] => Event::new(
            &PUBLISH_ACK_EVENT,
            json!({"stream": body["stream"], "seq": body["seq"], "duplicate": body["duplicate"].as_bool().unwrap_or(false),
                   "error": error_of(&body)}),
        ),
        ["fetch", stream, consumer] => {
            let (status, description) = status_of(headers)?;
            Event::new(
                &FETCH_DONE_EVENT,
                json!({"stream": stream, "consumer": consumer, "status": status, "description": description}),
            )
        }
        _ => return None,
    })
}
