//! JetStream on NetGet's NATS server, when started with `jetstream: true`.
//!
//! JetStream is an API carried over ordinary NATS request/reply: a client publishes JSON to
//! `$JS.API.<operation>` with a reply inbox, publishes to a stream's subjects with a reply inbox
//! and waits for a publish ack, pulls with `$JS.API.CONSUMER.MSG.NEXT.<stream>.<consumer>`, and
//! acknowledges by publishing to the `$JS.ACK.…` reply subject each delivery carried.
//!
//! Rust owns the subjects, the response envelopes (each `type`, the defaults a client needs to
//! decode one), the ack subjects, the end-of-batch status headers, and the table of which
//! subjects belong to which stream — learned from the stream configs the handler accepted,
//! which is routing, like the subscription table. **No message is stored here**: the handler
//! keeps streams' contents (in its memory, or a script's file) and answers every pull.
use super::actions::{check_token, decode_payload, encode_headers};
use super::subject_matches;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::actions::{ActionDefinition, Parameter};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::{Event, EventType};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

/// Most messages one pull may deliver.
pub const MAX_BATCH: u64 = 256;
/// Most streams whose subjects the server routes.
pub const MAX_STREAMS: usize = 1024;

/// Server-wide JetStream state: which subjects each stream captures.
#[derive(Default, Debug)]
pub struct Shared {
    streams: Mutex<BTreeMap<String, Vec<String>>>,
}

/// What a published subject means to JetStream.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// `$JS.API.<operation…>`: the operation and the stream and consumer it names.
    Api {
        operation: String,
        stream: Option<String>,
        consumer: Option<String>,
    },
    /// A pull request.
    Pull { stream: String, consumer: String },
    /// An acknowledgement on a delivery's reply subject.
    Ack {
        stream: String,
        consumer: String,
        delivered: u64,
        stream_seq: u64,
        consumer_seq: u64,
    },
    /// A message on a subject a stream captures.
    Publish { stream: String },
}

impl Shared {
    pub fn classify(&self, subject: &str) -> Option<Kind> {
        let tokens: Vec<&str> = subject.split('.').collect();
        if let Some(rest) = subject.strip_prefix("$JS.API.") {
            let t: Vec<&str> = rest.split('.').collect();
            return Some(match t.as_slice() {
                ["CONSUMER", "MSG", "NEXT", s, c] => Kind::Pull {
                    stream: s.to_string(),
                    consumer: c.to_string(),
                },
                ["INFO"] => api("INFO", None, None),
                ["STREAM", op @ ("NAMES" | "LIST")] => api(&format!("STREAM.{op}"), None, None),
                ["STREAM", op @ ("CREATE" | "UPDATE" | "INFO" | "DELETE" | "PURGE"), s] => {
                    api(&format!("STREAM.{op}"), Some(s), None)
                }
                ["CONSUMER", op @ ("NAMES" | "LIST"), s] => {
                    api(&format!("CONSUMER.{op}"), Some(s), None)
                }
                ["CONSUMER", "CREATE", s] => api("CONSUMER.CREATE", Some(s), None),
                ["CONSUMER", "CREATE", s, c, ..] | ["CONSUMER", "DURABLE", "CREATE", s, c] => {
                    api("CONSUMER.CREATE", Some(s), Some(c))
                }
                ["CONSUMER", op @ ("INFO" | "DELETE"), s, c] => {
                    api(&format!("CONSUMER.{op}"), Some(s), Some(c))
                }
                _ => api(rest, None, None),
            });
        }
        if tokens.len() == 9 && tokens[0] == "$JS" && tokens[1] == "ACK" {
            let n = |i: usize| tokens[i].parse::<u64>().ok();
            return Some(Kind::Ack {
                stream: tokens[2].to_string(),
                consumer: tokens[3].to_string(),
                delivered: n(4)?,
                stream_seq: n(5)?,
                consumer_seq: n(6)?,
            });
        }
        let streams = self.streams.lock().ok()?;
        streams
            .iter()
            .find(|(_, subjects)| subjects.iter().any(|p| subject_matches(p, subject)))
            .map(|(name, _)| Kind::Publish {
                stream: name.clone(),
            })
    }

    fn remember(&self, stream: &str, config: &Value) {
        let subjects = config["subjects"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| vec![stream.to_string()]);
        if let Ok(mut streams) = self.streams.lock() {
            if streams.len() < MAX_STREAMS || streams.contains_key(stream) {
                streams.insert(stream.to_string(), subjects);
            }
        }
    }

    fn forget(&self, stream: &str) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.remove(stream);
        }
    }
}

fn api(operation: &str, stream: Option<&&str>, consumer: Option<&&str>) -> Kind {
    Kind::Api {
        operation: operation.to_string(),
        stream: stream.map(|s| s.to_string()),
        consumer: consumer.map(|s| s.to_string()),
    }
}

/// The `type` a response to `operation` carries.
fn response_type(operation: &str) -> Option<&'static str> {
    Some(match operation {
        "INFO" => "io.nats.jetstream.api.v1.account_info_response",
        "STREAM.CREATE" => "io.nats.jetstream.api.v1.stream_create_response",
        "STREAM.UPDATE" => "io.nats.jetstream.api.v1.stream_update_response",
        "STREAM.INFO" => "io.nats.jetstream.api.v1.stream_info_response",
        "STREAM.DELETE" => "io.nats.jetstream.api.v1.stream_delete_response",
        "STREAM.PURGE" => "io.nats.jetstream.api.v1.stream_purge_response",
        "STREAM.NAMES" => "io.nats.jetstream.api.v1.stream_names_response",
        "STREAM.LIST" => "io.nats.jetstream.api.v1.stream_list_response",
        "CONSUMER.CREATE" => "io.nats.jetstream.api.v1.consumer_create_response",
        "CONSUMER.INFO" => "io.nats.jetstream.api.v1.consumer_info_response",
        "CONSUMER.DELETE" => "io.nats.jetstream.api.v1.consumer_delete_response",
        "CONSUMER.NAMES" => "io.nats.jetstream.api.v1.consumer_names_response",
        "CONSUMER.LIST" => "io.nats.jetstream.api.v1.consumer_list_response",
        _ => return None,
    })
}

const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn stream_state() -> Value {
    json!({"messages": 0, "bytes": 0, "first_seq": 0, "first_ts": ZERO_TIME,
           "last_seq": 0, "last_ts": ZERO_TIME, "consumer_count": 0})
}

/// A stream's info with every field a client decodes present.
fn stream_info(stream: &str, config: &Value, given: &Map<String, Value>) -> Value {
    let mut config = given
        .get("config")
        .cloned()
        .unwrap_or_else(|| config.clone());
    if !config.is_object() {
        config = json!({});
    }
    config["name"] = json!(stream);
    for (k, v) in [
        ("retention", json!("limits")),
        ("storage", json!("file")),
        ("discard", json!("old")),
        ("num_replicas", json!(1)),
        ("max_msgs", json!(-1)),
        ("max_bytes", json!(-1)),
        ("max_age", json!(0)),
        ("max_consumers", json!(-1)),
    ] {
        if config.get(k).is_none_or(Value::is_null) {
            config[k] = v;
        }
    }
    if config.get("subjects").is_none_or(Value::is_null) {
        config["subjects"] = json!([stream]);
    }
    let mut state = stream_state();
    if let Some(Value::Object(s)) = given.get("state") {
        for (k, v) in s {
            state[k] = v.clone();
        }
    }
    json!({"config": config, "created": given.get("created").cloned().unwrap_or_else(|| json!(now())), "state": state})
}

/// A consumer's info with every field a client decodes present.
fn consumer_info(
    stream: &str,
    consumer: &str,
    request: &Value,
    given: &Map<String, Value>,
) -> Value {
    let mut config = given
        .get("config")
        .cloned()
        .unwrap_or_else(|| request["config"].clone());
    if !config.is_object() {
        config = json!({});
    }
    for (k, v) in [
        ("deliver_policy", json!("all")),
        ("ack_policy", json!("explicit")),
        ("replay_policy", json!("instant")),
        ("ack_wait", json!(30_000_000_000u64)),
        ("max_deliver", json!(-1)),
        ("max_ack_pending", json!(1000)),
        ("max_waiting", json!(512)),
        ("num_replicas", json!(0)),
    ] {
        if config.get(k).is_none_or(Value::is_null) {
            config[k] = v;
        }
    }
    let mut out = json!({
        "stream_name": stream, "name": consumer, "created": now(), "config": config,
        "delivered": {"consumer_seq": 0, "stream_seq": 0},
        "ack_floor": {"consumer_seq": 0, "stream_seq": 0},
        "num_ack_pending": 0, "num_redelivered": 0, "num_waiting": 0, "num_pending": 0,
    });
    for (k, v) in given {
        if k != "config" {
            out[k] = v.clone();
        }
    }
    out
}

/// The full API response for `operation`, from the handler's (possibly partial) `response`.
fn build_response(
    operation: &str,
    stream: Option<&str>,
    consumer: Option<&str>,
    request: &Value,
    given: &Map<String, Value>,
) -> Value {
    let stream = stream.unwrap_or_default();
    let mut out = match operation {
        "INFO" => json!({"memory": 0, "storage": 0, "reserved_memory": 0, "reserved_storage": 0,
            "streams": 0, "consumers": 0,
            "limits": {"max_memory": -1, "max_storage": -1, "max_streams": -1, "max_consumers": -1,
                       "max_ack_pending": -1, "memory_max_stream_bytes": -1,
                       "storage_max_stream_bytes": -1, "max_bytes_required": false},
            "api": {"total": 0, "errors": 0}}),
        "STREAM.CREATE" | "STREAM.UPDATE" | "STREAM.INFO" => stream_info(stream, request, given),
        "STREAM.DELETE" | "CONSUMER.DELETE" => json!({"success": true}),
        "STREAM.PURGE" => json!({"success": true, "purged": 0}),
        "STREAM.NAMES" => json!({"total": 0, "offset": 0, "limit": 1024, "streams": []}),
        "STREAM.LIST" => {
            json!({"total": 0, "offset": 0, "limit": 256, "streams": [], "missing": []})
        }
        "CONSUMER.NAMES" => json!({"total": 0, "offset": 0, "limit": 1024, "consumers": []}),
        "CONSUMER.LIST" => json!({"total": 0, "offset": 0, "limit": 256, "consumers": []}),
        "CONSUMER.CREATE" | "CONSUMER.INFO" => {
            let name = consumer
                .map(str::to_string)
                .or_else(|| request["config"]["name"].as_str().map(str::to_string))
                .or_else(|| {
                    request["config"]["durable_name"]
                        .as_str()
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "ephemeral".into());
            consumer_info(stream, &name, request, given)
        }
        _ => json!({}),
    };
    if !operation.starts_with("STREAM.C")
        && !operation.starts_with("STREAM.U")
        && operation != "STREAM.INFO"
        && !operation.starts_with("CONSUMER.C")
        && operation != "CONSUMER.INFO"
    {
        for (k, v) in given {
            out[k] = v.clone();
        }
    }
    if let Some(t) = response_type(operation) {
        out["type"] = json!(t);
    }
    // Totals follow the lists the handler gave.
    for key in ["streams", "consumers"] {
        if let Some(n) = out[key].as_array().map(Vec::len) {
            if operation.ends_with("NAMES") || operation.ends_with("LIST") {
                out["total"] = json!(n);
            }
        }
    }
    out
}

fn error_response(operation: &str, code: u64, err_code: u64, description: &str) -> Value {
    let mut out =
        json!({"error": {"code": code, "err_code": err_code, "description": description}});
    if let Some(t) = response_type(operation) {
        out["type"] = json!(t);
    }
    out
}

/// One delivery to `inbox` on subscription `sid`, acknowledged on the subject it carries.
#[allow(clippy::too_many_arguments)]
fn delivery(
    inbox: &str,
    sid: &str,
    stream: &str,
    consumer: &str,
    m: &Value,
    consumer_seq: u64,
    pending: u64,
) -> Result<Vec<u8>> {
    let stream_seq = m["stream_seq"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("each message needs a positive stream_seq")?;
    let delivered = m["delivered"].as_u64().unwrap_or(1).max(1);
    let consumer_seq = m["consumer_seq"].as_u64().unwrap_or(consumer_seq);
    let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let ack = format!(
        "$JS.ACK.{stream}.{consumer}.{delivered}.{stream_seq}.{consumer_seq}.{ts}.{pending}"
    );
    let subject = m["subject"]
        .as_str()
        .context("each message needs a subject")?;
    check_token(subject, "subject")?;
    let payload = decode_payload(m)?;
    let headers = m["headers"].as_object().filter(|h| !h.is_empty());
    let mut out = Vec::new();
    // A delivery is addressed to the pull's inbox; its original subject travels as the
    // subject the client reads back from the ack metadata and headers. NATS delivers JetStream
    // messages on their own subject through the inbox subscription, so the MSG line names the
    // stored subject and the inbox subscription's sid.
    let _ = inbox;
    match headers {
        Some(h) => {
            let block = encode_headers(h)?;
            out.extend(
                format!(
                    "HMSG {subject} {sid} {ack} {} {}\r\n",
                    block.len(),
                    block.len() + payload.len()
                )
                .into_bytes(),
            );
            out.extend(block);
        }
        None => out.extend(format!("MSG {subject} {sid} {ack} {}\r\n", payload.len()).into_bytes()),
    }
    out.extend(payload);
    out.extend(b"\r\n");
    Ok(out)
}

/// A status-only message (`NATS/1.0 404 No Messages`), as the real server ends a pull.
fn status(inbox: &str, sid: &str, code: u16, text: &str, pending: u64) -> Vec<u8> {
    let block = if code == 408 {
        format!("NATS/1.0 {code} {text}\r\nNats-Pending-Messages: {pending}\r\nNats-Pending-Bytes: 0\r\n\r\n")
    } else {
        format!("NATS/1.0 {code} {text}\r\n\r\n")
    };
    let mut out = format!("HMSG {inbox} {sid} {} {}\r\n", block.len(), block.len()).into_bytes();
    out.extend(block.into_bytes());
    out.extend(b"\r\n");
    out
}

/// A plain reply to `inbox`.
pub fn reply(inbox: &str, sid: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("MSG {inbox} {sid} {}\r\n", body.len()).into_bytes();
    out.extend(body);
    out.extend(b"\r\n");
    out
}

// ---------------------------------------------------------------------------
// Events and actions
// ---------------------------------------------------------------------------

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

fn reply_action() -> ActionDefinition {
    a(
        "nats_js_reply",
        "Answer a JetStream API request. Give only what you decide (e.g. {\"streams\": [\"ORDERS\"]} for STREAM.NAMES, {\"state\": {\"messages\": 3, \"last_seq\": 3}} for STREAM.INFO); the response type and every other field a client needs are filled in.",
        vec![p("response", "object", "The response fields you decide", true)],
        json!({"type":"nats_js_reply","response":{"streams":["ORDERS"]}}),
        "-> JetStream reply {preview(response,100)}",
    )
}

fn error_action() -> ActionDefinition {
    a(
        "nats_js_error",
        "Refuse a JetStream request with an API error, e.g. 404/10059 stream not found, 404/10014 consumer not found.",
        vec![
            p("code", "number", "HTTP-like status: 400, 404, 409, 500 or 503", true),
            p("err_code", "number", "JetStream error code, e.g. 10059 (stream not found), 10014 (consumer not found), 10058 (stream name in use)", false),
            p("description", "string", "Explanation, at most 256 bytes", true),
        ],
        json!({"type":"nats_js_error","code":404,"err_code":10059,"description":"stream not found"}),
        "-> JetStream error {code} {description}",
    )
}

fn ack_action() -> ActionDefinition {
    a(
        "nats_js_ack",
        "Acknowledge the publish as stored at stream sequence seq.",
        vec![
            p(
                "seq",
                "number",
                "The stream sequence this message was stored at (1, 2, …)",
                true,
            ),
            p(
                "duplicate",
                "boolean",
                "True if this was a duplicate of an already stored message",
                false,
            ),
        ],
        json!({"type":"nats_js_ack","seq":1}),
        "-> JetStream publish ack seq={seq}",
    )
}

fn deliver_action() -> ActionDefinition {
    a(
        "nats_js_deliver",
        "Deliver up to batch messages for the pull; fewer (or none) ends it early with a status, as an expired pull does.",
        vec![
            p("messages", "array", "Each {subject, payload, encoding? (utf8|hex), headers?, stream_seq, consumer_seq?, delivered?}", true),
            p("pending", "number", "Messages still waiting after these (default 0)", false),
        ],
        json!({"type":"nats_js_deliver","messages":[{"subject":"orders.new","payload":"{\"id\":1}","stream_seq":1}]}),
        "-> JetStream deliver {preview(messages,100)}",
    )
}

fn noted_action() -> ActionDefinition {
    a(
        "nats_js_noted",
        "Take note of an acknowledgement; nothing is sent beyond confirming it if the client asked.",
        vec![],
        json!({"type":"nats_js_noted"}),
        "-> JetStream ack noted",
    )
}

pub fn actions() -> Vec<ActionDefinition> {
    vec![
        reply_action(),
        error_action(),
        ack_action(),
        deliver_action(),
        noted_action(),
    ]
}

pub static API_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_api",
        "A JetStream API request ($JS.API.…): stream and consumer management and account info. NetGet stores nothing — you own the streams, their messages and consumers.",
        reply_action().example.clone(),
    )
    .with_parameters(vec![
        p("operation", "string", "INFO, STREAM.CREATE/UPDATE/INFO/DELETE/PURGE/NAMES/LIST, CONSUMER.CREATE/INFO/DELETE/NAMES/LIST", true),
        p("stream", "string", "The stream the request names, if any", false),
        p("consumer", "string", "The consumer the request names, if any", false),
        p("request", "object", "The request's JSON body (e.g. the stream or consumer config)", true),
    ])
    .with_actions(vec![reply_action(), error_action()])
});

pub static PUBLISH_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_publish",
        "A message was published to a subject a stream captures. Store it (remember it) and ack with its sequence, or refuse.",
        ack_action().example.clone(),
    )
    .with_parameters(vec![
        p("stream", "string", "The stream whose subjects matched", true),
        p("subject", "string", "The subject published to", true),
        p("payload", "string", "Message body (text, or hex per payload_encoding)", true),
        p("payload_encoding", "string", "How payload is encoded: utf8 or hex", true),
        p("headers", "object", "Message headers", true),
        p("wants_ack", "boolean", "Whether the publisher waits for an ack", true),
    ])
    .with_actions(vec![ack_action(), error_action()])
});

pub static PULL_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_pull",
        "A consumer pulled messages. Deliver the next ones you hold for it (at most batch).",
        deliver_action().example.clone(),
    )
    .with_parameters(vec![
        p("stream", "string", "The JetStream stream's name", true),
        p("consumer", "string", "The pull consumer's name", true),
        p("batch", "number", "Most messages wanted", true),
        p(
            "no_wait",
            "boolean",
            "The client wants an immediate answer, even an empty one",
            true,
        ),
        p(
            "expires_ms",
            "number",
            "How long the client will wait",
            true,
        ),
    ])
    .with_actions(vec![deliver_action()])
});

pub static ACK_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "nats_js_acked",
        "A consumer acknowledged a delivery (ack, nak, in-progress or term).",
        noted_action().example.clone(),
    )
    .with_parameters(vec![
        p("stream", "string", "The JetStream stream's name", true),
        p("consumer", "string", "The pull consumer's name", true),
        p(
            "stream_seq",
            "number",
            "Stream sequence of the acknowledged message",
            true,
        ),
        p(
            "consumer_seq",
            "number",
            "Consumer sequence of the delivery",
            true,
        ),
        p(
            "delivered",
            "number",
            "How many times it had been delivered",
            true,
        ),
        p("kind", "string", "ack, nak, progress, term or next", true),
    ])
    .with_actions(vec![noted_action()])
});

pub fn event_types() -> Vec<EventType> {
    vec![
        API_EVENT.clone(),
        PUBLISH_EVENT.clone(),
        PULL_EVENT.clone(),
        ACK_EVENT.clone(),
    ]
}

/// Validate one JetStream action; `None` when the name is not JetStream's.
pub fn execute(action: &Value) -> Option<Result<ActionResult>> {
    let name = action["type"].as_str()?;
    let checked = match name {
        "nats_js_reply" => action["response"]
            .is_object()
            .then_some(())
            .context("response must be an object"),
        "nats_js_error" => (|| {
            let code = action["code"].as_u64().context("code required")?;
            ensure!(
                [400, 404, 409, 500, 503].contains(&code),
                "code must be 400, 404, 409, 500 or 503"
            );
            let d = action["description"]
                .as_str()
                .context("description required")?;
            ensure!(
                d.len() <= 256 && !d.contains(['\r', '\n']),
                "description at most 256 bytes, one line"
            );
            Ok(())
        })(),
        "nats_js_ack" => action["seq"]
            .as_u64()
            .filter(|n| *n > 0)
            .map(|_| ())
            .context("seq must be a positive integer"),
        "nats_js_deliver" => (|| {
            let messages = action["messages"]
                .as_array()
                .context("messages must be an array")?;
            ensure!(
                messages.len() as u64 <= MAX_BATCH,
                "at most {MAX_BATCH} messages"
            );
            for m in messages {
                delivery("_", "1", "S", "C", m, 1, 0)?;
            }
            Ok(())
        })(),
        "nats_js_noted" => Ok(()),
        _ => return None,
    };
    Some(checked.map(|_| ActionResult::Custom {
        name: name.to_string(),
        data: action.clone(),
    }))
}

// ---------------------------------------------------------------------------
// Answering
// ---------------------------------------------------------------------------

/// What the handler said, flattened to JetStream actions.
fn answers(result: crate::llm::actions::executor::ExecutionResult) -> Result<Vec<(String, Value)>> {
    ensure!(result.failures.is_empty(), "the handler's actions failed");
    let mut out = Vec::new();
    let mut stack = result.protocol_results;
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("nats_js_") => {
                out.push((name, data))
            }
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    out.reverse();
    Ok(out)
}

pub struct Frame<'a> {
    pub subject: &'a str,
    pub reply_to: Option<&'a str>,
    pub headers: &'a BTreeMap<String, String>,
    pub payload: &'a [u8],
}

/// Ask the handler about one JetStream frame and return the bytes to write.
/// `sid_for` finds the subscription a reply subject reaches.
pub async fn answer(
    js: &Shared,
    kind: Kind,
    frame: Frame<'_>,
    sid_for: impl Fn(&str) -> Option<String>,
    ask: impl AsyncFnOnce(Event) -> Result<crate::llm::actions::executor::ExecutionResult>,
    log: &crate::logging::emit::Log<'_>,
) -> Vec<u8> {
    let target = frame
        .reply_to
        .and_then(|r| sid_for(r).map(|sid| (r.to_string(), sid)));
    let outcome = |op: &str, decision: &str| {
        let line = format!("NATS JetStream operation={op} decision={decision}");
        if decision.starts_with("fail_closed") {
            log.error(line)
        } else {
            log.info(line)
        }
    };
    let failure_text = |e: Option<&anyhow::Error>| match e {
        Some(e) => crate::utils::wire_failure::prefixed_wire_failure_text(e),
        None => crate::utils::WireFailure::Unavailable.prefixed_text(),
    };
    let send = |body: Value| -> Vec<u8> {
        match &target {
            Some((inbox, sid)) => reply(inbox, sid, body.to_string().as_bytes()),
            None => Vec::new(),
        }
    };
    match kind {
        Kind::Api {
            operation,
            stream,
            consumer,
        } => {
            let request: Value = if frame.payload.is_empty() {
                json!({})
            } else {
                match serde_json::from_slice(frame.payload) {
                    Ok(v) => v,
                    Err(_) => {
                        outcome(&operation, "fail_closed_bad_request");
                        return send(error_response(
                            &operation,
                            400,
                            10025,
                            "bad request: the body is not JSON",
                        ));
                    }
                }
            };
            if response_type(&operation).is_none() {
                outcome(&operation, "unsupported");
                return send(error_response(
                    &operation,
                    400,
                    10025,
                    "this JetStream API is not supported",
                ));
            }
            for name in [&stream, &consumer].into_iter().flatten() {
                if check_token(name, "name").is_err() || name.contains(['*', '>']) {
                    return send(error_response(&operation, 400, 10025, "invalid name"));
                }
            }
            let event = Event::new(
                &API_EVENT,
                json!({"operation": operation, "stream": stream, "consumer": consumer, "request": request}),
            );
            let result = match ask(event).await.and_then(answers) {
                Ok(r) => r,
                Err(e) => {
                    outcome(&operation, "fail_closed_llm_error");
                    return send(error_response(
                        &operation,
                        503,
                        10000,
                        failure_text(Some(&e)),
                    ));
                }
            };
            match result.as_slice() {
                [(n, d)] if n == "nats_js_reply" => {
                    let given = d["response"].as_object().cloned().unwrap_or_default();
                    let body = build_response(
                        &operation,
                        stream.as_deref(),
                        consumer.as_deref(),
                        &request,
                        &given,
                    );
                    match (operation.as_str(), stream.as_deref()) {
                        ("STREAM.CREATE" | "STREAM.UPDATE", Some(s)) => {
                            js.remember(s, &body["config"])
                        }
                        ("STREAM.DELETE", Some(s)) => js.forget(s),
                        _ => {}
                    }
                    outcome(&operation, "model_answer");
                    send(body)
                }
                [(n, d)] if n == "nats_js_error" => {
                    outcome(&operation, "model_reject");
                    send(error_response(
                        &operation,
                        d["code"].as_u64().unwrap_or(500),
                        d["err_code"].as_u64().unwrap_or(10000),
                        d["description"].as_str().unwrap_or_default(),
                    ))
                }
                [] => {
                    outcome(&operation, "fail_closed_model_silent");
                    send(error_response(&operation, 503, 10000, failure_text(None)))
                }
                _ => {
                    outcome(&operation, "fail_closed_invalid_reply");
                    send(error_response(&operation, 503, 10000, failure_text(None)))
                }
            }
        }
        Kind::Publish { stream } => {
            let (payload, encoding) = if frame
                .payload
                .iter()
                .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
            {
                (String::from_utf8_lossy(frame.payload).to_string(), "utf8")
            } else {
                (hex::encode(frame.payload), "hex")
            };
            let event = Event::new(
                &PUBLISH_EVENT,
                json!({"stream": stream, "subject": frame.subject, "payload": payload,
                       "payload_encoding": encoding, "headers": frame.headers,
                       "wants_ack": frame.reply_to.is_some()}),
            );
            let fail = |text: &str| json!({"error": {"code": 503, "err_code": 10000, "description": text}});
            let result = match ask(event).await.and_then(answers) {
                Ok(r) => r,
                Err(e) => {
                    outcome("PUBLISH", "fail_closed_llm_error");
                    return send(fail(failure_text(Some(&e))));
                }
            };
            match result.as_slice() {
                [(n, d)] if n == "nats_js_ack" => {
                    outcome("PUBLISH", "model_answer");
                    let mut ack = json!({"stream": stream, "seq": d["seq"]});
                    if d["duplicate"] == true {
                        ack["duplicate"] = json!(true);
                    }
                    send(ack)
                }
                [(n, d)] if n == "nats_js_error" => {
                    outcome("PUBLISH", "model_reject");
                    send(
                        json!({"error": {"code": d["code"], "err_code": d["err_code"].as_u64().unwrap_or(10000),
                                          "description": d["description"]}}),
                    )
                }
                _ => {
                    outcome("PUBLISH", "fail_closed_invalid_reply");
                    send(fail(failure_text(None)))
                }
            }
        }
        Kind::Pull { stream, consumer } => {
            let request: Value = serde_json::from_slice(frame.payload).unwrap_or(json!({}));
            let batch = request["batch"].as_u64().unwrap_or(1).clamp(1, MAX_BATCH);
            let no_wait = request["no_wait"].as_bool().unwrap_or(false);
            let expires_ms = request["expires"].as_u64().unwrap_or(0) / 1_000_000;
            let Some((inbox, sid)) = target else {
                log.warn(format!("NATS JetStream pull on {stream}.{consumer} has no subscribed inbox to deliver to"));
                return Vec::new();
            };
            let event = Event::new(
                &PULL_EVENT,
                json!({"stream": stream, "consumer": consumer, "batch": batch,
                       "no_wait": no_wait, "expires_ms": expires_ms}),
            );
            let (messages, pending) = match ask(event).await.and_then(answers) {
                Ok(r) => match r.as_slice() {
                    [(n, d)] if n == "nats_js_deliver" => {
                        outcome("PULL", "model_answer");
                        (
                            d["messages"].as_array().cloned().unwrap_or_default(),
                            d["pending"].as_u64().unwrap_or(0),
                        )
                    }
                    [] => {
                        outcome("PULL", "model_silent");
                        (Vec::new(), 0)
                    }
                    _ => {
                        outcome("PULL", "fail_closed_invalid_reply");
                        (Vec::new(), 0)
                    }
                },
                // Nothing is fabricated: the pull ends empty.
                Err(_) => {
                    outcome("PULL", "fail_closed_llm_error");
                    (Vec::new(), 0)
                }
            };
            let mut out = Vec::new();
            let count = (messages.len() as u64).min(batch);
            for (i, m) in messages.iter().take(batch as usize).enumerate() {
                let left = pending + count - i as u64 - 1;
                match delivery(&inbox, &sid, &stream, &consumer, m, i as u64 + 1, left) {
                    Ok(bytes) => out.extend(bytes),
                    Err(e) => log.warn(format!("NATS JetStream delivery dropped: {e:#}")),
                }
            }
            if count < batch {
                out.extend(if no_wait && count == 0 {
                    status(&inbox, &sid, 404, "No Messages", 0)
                } else {
                    status(&inbox, &sid, 408, "Request Timeout", batch - count)
                });
            }
            out
        }
        Kind::Ack {
            stream,
            consumer,
            delivered,
            stream_seq,
            consumer_seq,
        } => {
            let body = String::from_utf8_lossy(frame.payload);
            let kind = match body.split_whitespace().next().unwrap_or("+ACK") {
                "+ACK" | "" => "ack",
                "-NAK" => "nak",
                "+WPI" => "progress",
                "+TERM" => "term",
                "+NXT" => "next",
                _ => "ack",
            };
            let event = Event::new(
                &ACK_EVENT,
                json!({"stream": stream, "consumer": consumer, "stream_seq": stream_seq,
                       "consumer_seq": consumer_seq, "delivered": delivered, "kind": kind}),
            );
            if let Err(e) = ask(event).await.and_then(answers) {
                log.warn(format!("NATS JetStream ack handler failed: {e:#}"));
            }
            outcome("ACK", "noted");
            // A synchronous ack waits for an empty reply.
            match &target {
                Some((inbox, sid)) => reply(inbox, sid, b""),
                None => Vec::new(),
            }
        }
    }
}
