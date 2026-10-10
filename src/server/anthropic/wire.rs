//! The Anthropic Messages API's shapes, shared by the server and the client: request validation
//! with Anthropic's own error envelope, the summary of a request the handler sees, the `message`
//! object, its server-sent-event stream, and the models list.
use anyhow::{bail, ensure, Context, Result};
use rand::{distributions::Alphanumeric, Rng};
use serde_json::{json, Map, Value};

/// Largest request body accepted. Anthropic's own limit is 32 MB; a mock answering text has no
/// use for images that size, and the handler is shown a summary of every block.
pub const MAX_BODY: usize = 8 * 1024 * 1024;
/// Most messages, content blocks per message and tools in one request.
pub const MAX_MESSAGES: usize = 1000;
pub const MAX_BLOCKS: usize = 1000;
pub const MAX_TOOLS: usize = 256;
/// Most content blocks, and text bytes, a handler may answer with.
pub const MAX_REPLY_BLOCKS: usize = 64;
pub const MAX_REPLY_TEXT: usize = 1024 * 1024;
/// Text deltas in a stream are cut at about this many bytes (on a character boundary).
pub const DELTA_BYTES: usize = 16;
pub const DEFAULT_MODEL: &str = "claude-netget-1";

/// Anthropic's error types and the HTTP status each is sent with.
pub const ERROR_TYPES: [(&str, u16); 8] = [
    ("invalid_request_error", 400),
    ("authentication_error", 401),
    ("permission_error", 403),
    ("not_found_error", 404),
    ("request_too_large", 413),
    ("rate_limit_error", 429),
    ("api_error", 500),
    ("overloaded_error", 529),
];

pub const STOP_REASONS: [&str; 6] = [
    "end_turn",
    "max_tokens",
    "stop_sequence",
    "tool_use",
    "pause_turn",
    "refusal",
];

pub fn status_of(error_type: &str) -> Option<u16> {
    ERROR_TYPES
        .iter()
        .find(|(t, _)| *t == error_type)
        .map(|(_, s)| *s)
}

pub fn random_id(prefix: &str) -> String {
    let tail: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    format!("{prefix}{tail}")
}

/// `{"type":"error","error":{...},"request_id":...}`
pub fn error_body(error_type: &str, message: &str, request_id: &str) -> Value {
    json!({"type": "error", "error": {"type": error_type, "message": message}, "request_id": request_id})
}

/// Rough token estimate: a quarter of the characters, rounded up. Every count NetGet reports
/// without a handler's figure is this, and says so in its metadata.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

fn block_summary(b: &Value) -> Result<Value> {
    let kind = b["type"].as_str().context("content block without a type")?;
    Ok(match kind {
        "text" => {
            json!({"type": "text", "text": b["text"].as_str().context("text block without text")?})
        }
        "tool_use" => {
            json!({"type": "tool_use", "id": b["id"], "name": b["name"], "input": b["input"]})
        }
        "tool_result" => {
            let content = match &b["content"] {
                Value::String(s) => json!(s),
                Value::Array(parts) => json!(parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")),
                _ => json!(""),
            };
            json!({"type": "tool_result", "tool_use_id": b["tool_use_id"], "content": content,
                   "is_error": b["is_error"].as_bool().unwrap_or(false)})
        }
        "image" | "document" => {
            let source = &b["source"];
            let bytes = source["data"].as_str().map(|d| d.len() * 3 / 4);
            json!({"type": kind, "source_type": source["type"], "media_type": source["media_type"],
                   "approx_bytes": bytes, "url": source["url"]})
        }
        "thinking" | "redacted_thinking" => json!({"type": kind}),
        other => json!({"type": other}),
    })
}

fn content_summary(c: &Value, what: &str) -> Result<Value> {
    match c {
        Value::String(s) => Ok(json!([{"type": "text", "text": s}])),
        Value::Array(blocks) => {
            ensure!(
                blocks.len() <= MAX_BLOCKS,
                "{what}: at most {MAX_BLOCKS} content blocks"
            );
            Ok(Value::Array(
                blocks.iter().map(block_summary).collect::<Result<_>>()?,
            ))
        }
        _ => bail!("{what}: Input should be a valid string or list of content blocks"),
    }
}

/// Check a `/v1/messages` (or count_tokens, `for_count`) body the way the API does, and return
/// what the handler sees: base64 is never shown, only what each block is.
pub fn request_summary(body: &Value, for_count: bool) -> Result<Value> {
    let obj = body.as_object().context("body must be a JSON object")?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .context("model: Field required")?;
    ensure!(
        !model.is_empty(),
        "model: String should have at least 1 character"
    );
    let max_tokens = match obj.get("max_tokens") {
        Some(v) => Some(
            v.as_u64()
                .filter(|n| *n >= 1)
                .context("max_tokens: Input should be greater than or equal to 1")?,
        ),
        None if for_count => None,
        None => bail!("max_tokens: Field required"),
    };
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .context("messages: Field required")?;
    ensure!(
        !messages.is_empty(),
        "messages: at least one message is required"
    );
    ensure!(
        messages.len() <= MAX_MESSAGES,
        "messages: at most {MAX_MESSAGES} messages"
    );
    let mut out_messages = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let role = m["role"].as_str().unwrap_or_default();
        ensure!(
            role == "user" || role == "assistant",
            "messages.{i}.role: Input should be 'user' or 'assistant'"
        );
        let content = content_summary(&m["content"], &format!("messages.{i}.content"))?;
        out_messages.push(json!({"role": role, "content": content}));
    }
    let system = match obj.get("system") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => json!(s),
        Some(Value::Array(blocks)) => json!(blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")),
        Some(_) => bail!("system: Input should be a valid string or list of text blocks"),
    };
    let tools = match obj.get("tools") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(t)) => {
            ensure!(t.len() <= MAX_TOOLS, "tools: at most {MAX_TOOLS} tools");
            t.iter()
                .enumerate()
                .map(|(i, tool)| {
                    let name = tool["name"]
                        .as_str()
                        .with_context(|| format!("tools.{i}.name: Field required"))?;
                    Ok(json!({"name": name, "description": tool["description"], "input_schema": tool["input_schema"], "type": tool["type"]}))
                })
                .collect::<Result<_>>()?
        }
        Some(_) => bail!("tools: Input should be a valid list"),
    };
    let stream = match obj.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => bail!("stream: Input should be a valid boolean"),
    };
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("max_tokens".into(), json!(max_tokens));
    out.insert("system".into(), system);
    out.insert("messages".into(), Value::Array(out_messages));
    out.insert("tools".into(), Value::Array(tools));
    for key in [
        "tool_choice",
        "temperature",
        "top_p",
        "top_k",
        "stop_sequences",
        "thinking",
    ] {
        out.insert(key.into(), obj.get(key).cloned().unwrap_or(Value::Null));
    }
    out.insert(
        "user_id".into(),
        obj.get("metadata")
            .map(|m| m["user_id"].clone())
            .unwrap_or(Value::Null),
    );
    out.insert("stream".into(), json!(stream));
    Ok(Value::Object(out))
}

/// Every character of the request's text, for the token estimate.
pub fn request_text(body: &Value) -> String {
    let mut s = String::new();
    s.push_str(&body["system"].to_string());
    s.push_str(&body["messages"].to_string());
    if !body["tools"].is_null() {
        s.push_str(&body["tools"].to_string());
    }
    s
}

/// The handler's content, checked and completed into API content blocks.
pub fn reply_content(v: &Value) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    if let Some(t) = v["text"].as_str() {
        blocks.push(json!({"type": "text", "text": t}));
    }
    if let Some(list) = v["content"].as_array() {
        for b in list {
            match b["type"].as_str().unwrap_or_default() {
                "text" => blocks.push(json!({"type": "text",
                    "text": b["text"].as_str().context("a text block needs text")?})),
                "tool_use" => {
                    let name = b["name"]
                        .as_str()
                        .context("a tool_use block needs a name")?;
                    ensure!(
                        !name.is_empty() && name.len() <= 128,
                        "tool name must be 1..=128 characters"
                    );
                    let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                    ensure!(input.is_object(), "tool_use input must be an object");
                    let id = match b["id"].as_str() {
                        Some(id) if !id.is_empty() => id.to_string(),
                        _ => random_id("toolu_"),
                    };
                    blocks
                        .push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                }
                other => bail!(
                    "content block type {other:?} is not one a reply can carry (text, tool_use)"
                ),
            }
        }
    }
    ensure!(!blocks.is_empty(), "a reply needs text or content");
    ensure!(
        blocks.len() <= MAX_REPLY_BLOCKS,
        "at most {MAX_REPLY_BLOCKS} content blocks"
    );
    let text: usize = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .map(str::len)
        .sum();
    ensure!(text <= MAX_REPLY_TEXT, "reply text larger than 1 MiB");
    Ok(blocks)
}

/// The stop reason: the handler's, or tool_use when the reply calls a tool, else end_turn.
pub fn stop_reason(v: &Value, content: &[Value]) -> Result<String> {
    match v["stop_reason"].as_str() {
        Some(r) => {
            ensure!(
                STOP_REASONS.contains(&r),
                "stop_reason must be one of {STOP_REASONS:?}"
            );
            Ok(r.to_string())
        }
        None if content.iter().any(|b| b["type"] == "tool_use") => Ok("tool_use".into()),
        None => Ok("end_turn".into()),
    }
}

/// A complete `message` object.
pub fn message(
    id: &str,
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    stop_sequence: Option<&str>,
    input_tokens: u64,
    output_tokens: u64,
) -> Value {
    json!({
        "id": id, "type": "message", "role": "assistant", "model": model, "content": content,
        "stop_reason": stop_reason, "stop_sequence": stop_sequence,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens,
                  "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
    })
}

fn sse(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

/// Cut `s` into pieces of about `n` bytes on character boundaries.
pub fn pieces(s: &str, n: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + n).min(s.len());
        while !s.is_char_boundary(end) {
            end += 1;
        }
        out.push(&s[start..end]);
        start = end;
    }
    out
}

/// The event stream Anthropic sends for `message`: message_start, a ping, each block's start,
/// deltas and stop, message_delta with the stop reason and usage, message_stop.
pub fn event_stream(message: &Value) -> String {
    let mut out = String::new();
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["stop_sequence"] = Value::Null;
    start["usage"]["output_tokens"] = json!(1);
    sse(
        &mut out,
        "message_start",
        &json!({"type": "message_start", "message": start}),
    );
    sse(&mut out, "ping", &json!({"type": "ping"}));
    for (index, block) in message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if block["type"] == "tool_use" {
            let mut empty = block.clone();
            empty["input"] = json!({});
            sse(
                &mut out,
                "content_block_start",
                &json!({"type": "content_block_start", "index": index, "content_block": empty}),
            );
            let input = block["input"].to_string();
            for part in pieces(&input, DELTA_BYTES) {
                sse(
                    &mut out,
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": part}}),
                );
            }
        } else {
            sse(
                &mut out,
                "content_block_start",
                &json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "text", "text": ""}}),
            );
            for part in pieces(block["text"].as_str().unwrap_or_default(), DELTA_BYTES) {
                sse(
                    &mut out,
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index,
                    "delta": {"type": "text_delta", "text": part}}),
                );
            }
        }
        sse(
            &mut out,
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
    }
    sse(
        &mut out,
        "message_delta",
        &json!({"type": "message_delta",
        "delta": {"stop_reason": message["stop_reason"], "stop_sequence": message["stop_sequence"]},
        "usage": {"output_tokens": message["usage"]["output_tokens"]}}),
    );
    sse(&mut out, "message_stop", &json!({"type": "message_stop"}));
    out
}

/// A `model` object as `/v1/models` lists it.
pub fn model_object(id: &str) -> Value {
    json!({"type": "model", "id": id, "display_name": id, "created_at": "2026-01-01T00:00:00Z"})
}
