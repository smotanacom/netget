//! A2A protocol 1.0, JSON-RPC binding: the wire shapes (protobuf-JSON, camelCase) both roles
//! build and check. Pinned to the released 1.0 specification as implemented by a2a-sdk 1.2.1.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

pub const PROTOCOL_VERSION: &str = "1.0";
pub const VERSION_HEADER: &str = "A2A-Version";
pub const CARD_PATH: &str = "/.well-known/agent-card.json";
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_PARTS: usize = 64;
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

pub const TASK_NOT_FOUND: i64 = -32001;
pub const TASK_NOT_CANCELABLE: i64 = -32002;
pub const PUSH_NOT_SUPPORTED: i64 = -32003;
pub const UNSUPPORTED_OPERATION: i64 = -32004;
pub const CONTENT_TYPE_NOT_SUPPORTED: i64 = -32005;
pub const INVALID_AGENT_RESPONSE: i64 = -32006;
pub const EXTENDED_CARD_NOT_CONFIGURED: i64 = -32007;
pub const VERSION_NOT_SUPPORTED: i64 = -32009;

pub const STATES: &[(&str, &str)] = &[
    ("submitted", "TASK_STATE_SUBMITTED"),
    ("working", "TASK_STATE_WORKING"),
    ("completed", "TASK_STATE_COMPLETED"),
    ("failed", "TASK_STATE_FAILED"),
    ("canceled", "TASK_STATE_CANCELED"),
    ("input_required", "TASK_STATE_INPUT_REQUIRED"),
    ("rejected", "TASK_STATE_REJECTED"),
    ("auth_required", "TASK_STATE_AUTH_REQUIRED"),
];

pub fn state_wire(short: &str) -> Result<&'static str> {
    STATES.iter().find(|(s, _)| *s == short).map(|(_, w)| *w).with_context(|| format!("unknown task state '{short}'; use one of submitted, working, completed, failed, canceled, input_required, rejected, auth_required"))
}
pub fn state_short(wire: &str) -> Option<&'static str> {
    STATES.iter().find(|(_, w)| *w == wire).map(|(s, _)| *s)
}
pub fn terminal(short: &str) -> bool {
    matches!(short, "completed" | "failed" | "canceled" | "rejected")
}

pub fn budget_ok(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_BODY_BYTES, 50_000, 32)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `A2A-Version` must name 1.0 (patch ignored); missing means 0.3, which is refused.
pub fn version_ok(header: Option<&str>) -> bool {
    header.is_some_and(|v| {
        let mut parts = v.trim().split('.');
        parts.next() == Some("1")
            && parts.next() == Some("0")
            && parts.all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    })
}

pub fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
pub fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One SSE event carrying a JSON-RPC response.
pub fn sse(value: &Value) -> String {
    format!("data: {}\n\n", value)
}

/// Parts as handed to a handler: text and structured data as-is, files by reference/size.
fn parts_view(parts: &[Value]) -> Result<(String, Vec<Value>)> {
    ensure!(
        !parts.is_empty() && parts.len() <= MAX_PARTS,
        "a message carries 1..=64 parts"
    );
    let mut text = String::new();
    let mut view = Vec::new();
    for p in parts {
        let o = p.as_object().context("each part must be an object")?;
        if let Some(t) = o.get("text").and_then(Value::as_str) {
            ensure!(t.len() <= MAX_TEXT_BYTES, "text part exceeds 256 KiB");
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
            view.push(json!({"kind": "text", "text": t}));
        } else if let Some(d) = o.get("data") {
            view.push(json!({"kind": "data", "data": d}));
        } else if let Some(u) = o.get("url").and_then(Value::as_str) {
            view.push(json!({"kind": "file", "url": u, "filename": o.get("filename"), "media_type": o.get("mediaType")}));
        } else if let Some(r) = o.get("raw").and_then(Value::as_str) {
            // Inline bytes are not handed to a model; their size and type are.
            view.push(json!({"kind": "file", "inline_bytes": r.len() / 4 * 3, "filename": o.get("filename"), "media_type": o.get("mediaType")}));
        } else {
            bail!("a part needs text, data, url or raw");
        }
    }
    ensure!(text.len() <= MAX_TEXT_BYTES, "message text exceeds 256 KiB");
    Ok((text, view))
}

/// A message sent to the agent, checked.
pub struct Incoming {
    pub message_id: String,
    pub context_id: Option<String>,
    pub task_id: Option<String>,
    pub text: String,
    pub parts: Vec<Value>,
}

pub fn incoming_message(params: &Value) -> Result<Incoming> {
    let m = params
        .get("message")
        .and_then(Value::as_object)
        .context("params.message is required")?;
    let message_id = m
        .get("messageId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .context("message.messageId is required")?;
    ensure!(
        m.get("role").and_then(Value::as_str) == Some("ROLE_USER"),
        "message.role must be ROLE_USER"
    );
    let parts = m
        .get("parts")
        .and_then(Value::as_array)
        .context("message.parts is required")?;
    let (text, view) = parts_view(parts)?;
    let id = |k: &str| -> Result<Option<String>> {
        match m.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 256 => Ok(Some(s.clone())),
            _ => bail!("message.{k} must be a non-empty string"),
        }
    };
    Ok(Incoming {
        message_id: message_id.into(),
        context_id: id("contextId")?,
        task_id: id("taskId")?,
        text,
        parts: view,
    })
}

/// Parts a handler supplies: text, data or file references (never inline bytes).
pub fn parts_from(text: Option<&str>, parts: Option<&Value>) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    if let Some(t) = text {
        ensure!(t.len() <= MAX_TEXT_BYTES, "text exceeds 256 KiB");
        out.push(json!({"text": t}));
    }
    if let Some(list) = parts.filter(|p| !p.is_null()) {
        for p in list.as_array().context("parts must be an array")? {
            let o = p.as_object().context("each part must be an object")?;
            let part = if let Some(t) = o.get("text").and_then(Value::as_str) {
                json!({"text": t})
            } else if let Some(d) = o.get("data") {
                json!({"data": d})
            } else if let Some(u) = o.get("url").and_then(Value::as_str) {
                ensure!(
                    u.starts_with("http://") || u.starts_with("https://"),
                    "file parts reference an http(s) url"
                );
                let mut f = json!({"url": u});
                if let Some(n) = o.get("filename").and_then(Value::as_str) {
                    f["filename"] = json!(n);
                }
                if let Some(m) = o.get("media_type").and_then(Value::as_str) {
                    f["mediaType"] = json!(m);
                }
                f
            } else {
                bail!("a part needs text, data or url");
            };
            out.push(part);
        }
    }
    ensure!(
        !out.is_empty() && out.len() <= MAX_PARTS,
        "an answer carries 1..=64 parts"
    );
    Ok(out)
}

pub fn agent_message(
    text: Option<&str>,
    parts: Option<&Value>,
    context_id: &str,
    task_id: Option<&str>,
) -> Result<Value> {
    let mut m = json!({"messageId": new_id(), "contextId": context_id, "role": "ROLE_AGENT", "parts": parts_from(text, parts)?});
    if let Some(t) = task_id {
        m["taskId"] = json!(t);
    }
    Ok(m)
}

pub fn timestamp() -> String {
    let now = crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A task from a handler's `{id, state, text, artifacts}` description.
pub fn task(spec: &Value, default_id: &str, context_id: &str) -> Result<Value> {
    let o = spec.as_object().context("task must be an object")?;
    let id = o.get("id").and_then(Value::as_str).unwrap_or(default_id);
    ensure!(
        !id.is_empty() && id.len() <= 256,
        "task id must be 1..=256 characters"
    );
    let state = state_wire(
        o.get("state")
            .and_then(Value::as_str)
            .context("task.state is required")?,
    )?;
    let mut status = json!({"state": state, "timestamp": timestamp()});
    if let Some(t) = o.get("text").and_then(Value::as_str) {
        status["message"] = agent_message(Some(t), None, context_id, Some(id))?;
    }
    let mut t = json!({"id": id, "contextId": o.get("context_id").and_then(Value::as_str).unwrap_or(context_id), "status": status});
    if let Some(list) = o.get("artifacts").filter(|a| !a.is_null()) {
        let mut arts = Vec::new();
        for a in list.as_array().context("artifacts must be an array")? {
            ensure!(arts.len() < MAX_PARTS, "too many artifacts");
            let mut art = json!({"artifactId": a.get("id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(new_id), "parts": parts_from(a.get("text").and_then(Value::as_str), a.get("parts"))?});
            if let Some(n) = a.get("name").and_then(Value::as_str) {
                art["name"] = json!(n);
            }
            arts.push(art);
        }
        t["artifacts"] = Value::Array(arts);
    }
    ensure!(budget_ok(&t), "task exceeds the A2A bounds");
    Ok(t)
}

/// The SSE sequence for a streamed task: snapshot, working, artifacts, final status.
pub fn stream_events(id: &Value, task: &Value) -> Vec<Value> {
    let task_id = task["id"].clone();
    let context_id = task["contextId"].clone();
    let mut out = vec![rpc_result(
        id,
        json!({"task": {"id": task_id, "contextId": context_id, "status": {"state": "TASK_STATE_SUBMITTED"}}}),
    )];
    let final_state = task["status"]["state"]
        .as_str()
        .unwrap_or("TASK_STATE_COMPLETED");
    if final_state != "TASK_STATE_SUBMITTED" {
        out.push(rpc_result(id, json!({"statusUpdate": {"taskId": task_id, "contextId": context_id, "status": {"state": "TASK_STATE_WORKING", "timestamp": timestamp()}}})));
    }
    if let Some(arts) = task["artifacts"].as_array() {
        for a in arts {
            out.push(rpc_result(id, json!({"artifactUpdate": {"taskId": task_id, "contextId": context_id, "artifact": a, "lastChunk": true}})));
        }
    }
    if !matches!(final_state, "TASK_STATE_SUBMITTED" | "TASK_STATE_WORKING") {
        out.push(rpc_result(id, json!({"statusUpdate": {"taskId": task_id, "contextId": context_id, "status": task["status"]}})));
    }
    out
}

/// Agent card built from configuration.
pub fn card(
    name: &str,
    description: &str,
    version: &str,
    url: &str,
    streaming: bool,
    skills: &[Value],
) -> Value {
    json!({
        "name": name,
        "description": description,
        "version": version,
        "supportedInterfaces": [{"url": url, "protocolBinding": "JSONRPC", "protocolVersion": PROTOCOL_VERSION}],
        "capabilities": {"streaming": streaming, "pushNotifications": false},
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": skills,
    })
}

/// Client-side: the JSON-RPC interface a card offers for protocol 1.0.
pub fn card_rpc_url(card: &Value) -> Result<String> {
    let interfaces = card
        .get("supportedInterfaces")
        .and_then(Value::as_array)
        .context("agent card lacks supportedInterfaces")?;
    let iface = interfaces
        .iter()
        .find(|i| {
            i["protocolBinding"] == "JSONRPC"
                && i["protocolVersion"]
                    .as_str()
                    .is_some_and(|v| version_ok(Some(v)))
        })
        .context("agent card offers no JSON-RPC interface for protocol 1.0")?;
    let url = iface["url"].as_str().context("interface url missing")?;
    ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "interface url must be http(s)"
    );
    Ok(url.to_owned())
}

/// Client-side check of a result for `method`.
pub fn check_result(method: &str, result: &Value) -> Result<()> {
    ensure!(budget_ok(result), "result exceeds the A2A bounds");
    let o: &Map<String, Value> = result.as_object().context("result must be an object")?;
    match method {
        "SendMessage" | "SendStreamingMessage" => ensure!(
            ["task", "message", "statusUpdate", "artifactUpdate"]
                .iter()
                .any(|k| o.contains_key(*k)),
            "a send result carries task, message, statusUpdate or artifactUpdate"
        ),
        "GetTask" | "CancelTask" => ensure!(
            o.get("id").is_some_and(Value::is_string)
                && o.get("status").is_some_and(Value::is_object),
            "a task result needs id and status"
        ),
        "ListTasks" => ensure!(
            o.get("tasks").is_none_or(Value::is_array),
            "tasks must be an array"
        ),
        _ => {}
    }
    Ok(())
}
