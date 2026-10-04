//! The parts of RFC 8620 a server must do itself, whatever answers the methods: request
//! validation (§3.3, §3.6.1), result references (§3.7) and creation ids (§5.3). Shared with the
//! client, which checks the same shapes on the way out.
use serde_json::{json, Map, Value};

pub const CORE: &str = "urn:ietf:params:jmap:core";
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
pub const VACATION: &str = "urn:ietf:params:jmap:vacationresponse";
pub const CAPABILITIES: &[&str] = &[CORE, MAIL, SUBMISSION, VACATION];

pub const MAX_CALLS: usize = 16;
pub const MAX_OBJECTS_IN_GET: usize = 256;
pub const MAX_OBJECTS_IN_SET: usize = 128;
/// Nesting of the request JSON; the handler's arguments are bounded the same way.
pub const MAX_DEPTH: usize = 32;

/// A request-level problem (RFC 8620 §3.6.1), answered with HTTP 400.
#[derive(Debug, PartialEq)]
pub struct Problem {
    pub kind: &'static str,
    pub detail: String,
    pub limit: Option<&'static str>,
}

impl Problem {
    fn new(kind: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            limit: None,
        }
    }
    pub fn body(&self) -> Value {
        let mut b = json!({"type": format!("urn:ietf:params:jmap:error:{}", self.kind), "status": 400, "detail": self.detail});
        if let Some(l) = self.limit {
            b["limit"] = json!(l);
        }
        b
    }
}

/// A method-level error (RFC 8620 §3.6.2).
pub fn method_error(kind: &str, description: &str) -> Value {
    let mut e = json!({"type": kind});
    if !description.is_empty() {
        e["description"] = json!(description);
    }
    e
}

pub fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

pub struct Call {
    pub name: String,
    pub arguments: Map<String, Value>,
    pub id: String,
}

pub struct Request {
    pub using: Vec<String>,
    pub calls: Vec<Call>,
    pub created_ids: Option<Map<String, Value>>,
}

/// Parse and validate a request body.
pub fn parse(body: &[u8]) -> Result<Request, Problem> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| Problem::new("notJSON", "the body is not JSON"))?;
    if depth(&v) > MAX_DEPTH {
        return Err(Problem::new("notRequest", "the request nests too deeply"));
    }
    let obj = v
        .as_object()
        .ok_or_else(|| Problem::new("notRequest", "a request is a JSON object"))?;
    let using: Vec<String> = obj
        .get("using")
        .and_then(Value::as_array)
        .ok_or_else(|| Problem::new("notRequest", "using is an array of capability URIs"))?
        .iter()
        .map(|u| u.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or_else(|| Problem::new("notRequest", "using lists strings"))?;
    if let Some(u) = using.iter().find(|u| !CAPABILITIES.contains(&u.as_str())) {
        return Err(Problem::new(
            "unknownCapability",
            format!("the server does not support {u}"),
        ));
    }
    let raw = obj
        .get("methodCalls")
        .and_then(Value::as_array)
        .ok_or_else(|| Problem::new("notRequest", "methodCalls is an array"))?;
    if raw.len() > MAX_CALLS {
        return Err(Problem {
            limit: Some("maxCallsInRequest"),
            ..Problem::new("limit", format!("at most {MAX_CALLS} method calls"))
        });
    }
    let mut calls = Vec::with_capacity(raw.len());
    for c in raw {
        let shape = || Problem::new("notRequest", "each method call is [name, arguments, id]");
        let c = c.as_array().filter(|c| c.len() == 3).ok_or_else(shape)?;
        let name = c[0].as_str().ok_or_else(shape)?;
        let arguments = c[1].as_object().ok_or_else(shape)?;
        let id = c[2].as_str().ok_or_else(shape)?;
        calls.push(Call {
            name: name.to_owned(),
            arguments: arguments.clone(),
            id: id.to_owned(),
        });
    }
    let created_ids = match obj.get("createdIds") {
        None => None,
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => return Err(Problem::new("notRequest", "createdIds is an object")),
    };
    Ok(Request {
        using,
        calls,
        created_ids,
    })
}

/// The capability a method's type needs, or None for a method this server does not offer.
pub fn capability(method: &str) -> Option<&'static str> {
    let (kind, verb) = method.split_once('/')?;
    if kind == "Core" {
        return (verb == "echo").then_some(CORE);
    }
    let verbs: &[&str] = match kind {
        "Mailbox" | "Email" => &["get", "changes", "query", "queryChanges", "set"],
        "Thread" => &["get", "changes"],
        "SearchSnippet" => &["get"],
        "Identity" | "EmailSubmission" => &["get", "changes", "set"],
        "VacationResponse" => &["get", "set"],
        _ => return None,
    };
    if !verbs.contains(&verb)
        && !(kind == "Email" && matches!(verb, "copy" | "import" | "parse"))
        && !(kind == "EmailSubmission" && matches!(verb, "query" | "queryChanges"))
    {
        return None;
    }
    Some(match kind {
        "Identity" | "EmailSubmission" => SUBMISSION,
        "VacationResponse" => VACATION,
        _ => MAIL,
    })
}

/// RFC 8620 §5.1 and §5.3 size limits, as a method error type.
pub fn check_limits(method: &str, args: &Map<String, Value>) -> Option<Value> {
    let verb = method.split_once('/').map(|(_, v)| v).unwrap_or_default();
    if verb == "get" {
        if let Some(ids) = args.get("ids").and_then(Value::as_array) {
            if ids.len() > MAX_OBJECTS_IN_GET {
                return Some(method_error(
                    "requestTooLarge",
                    &format!("at most {MAX_OBJECTS_IN_GET} ids"),
                ));
            }
        }
    }
    if verb == "set" {
        let count = |k: &str| match args.get(k) {
            Some(Value::Object(m)) => m.len(),
            Some(Value::Array(a)) => a.len(),
            _ => 0,
        };
        if count("create") + count("update") + count("destroy") > MAX_OBJECTS_IN_SET {
            return Some(method_error(
                "requestTooLarge",
                &format!("at most {MAX_OBJECTS_IN_SET} objects"),
            ));
        }
    }
    None
}

/// Evaluate an RFC 8620 §3.7 JSON Pointer, where `*` maps over an array and flattens.
pub fn pointer(v: &Value, path: &str) -> Option<Value> {
    if path.is_empty() {
        return Some(v.clone());
    }
    let rest = path.strip_prefix('/')?;
    let (token, tail) = match rest.split_once('/') {
        Some((t, tail)) => (t, format!("/{tail}")),
        None => (rest, String::new()),
    };
    let token = token.replace("~1", "/").replace("~0", "~");
    match v {
        Value::Array(items) if token == "*" => {
            let mut out = Vec::new();
            for item in items {
                match pointer(item, &tail)? {
                    Value::Array(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            Some(Value::Array(out))
        }
        Value::Array(items) => pointer(items.get(token.parse::<usize>().ok()?)?, &tail),
        Value::Object(m) => pointer(m.get(&token)?, &tail),
        _ => None,
    }
}

/// Replace every `#name` argument with the value it references. `earlier` holds the responses
/// so far as (name, arguments, call id).
pub fn resolve_references(
    args: &Map<String, Value>,
    earlier: &[(String, Value, String)],
) -> Result<Map<String, Value>, Value> {
    let mut out = Map::new();
    for (k, v) in args {
        let Some(plain) = k.strip_prefix('#') else {
            out.insert(k.clone(), v.clone());
            continue;
        };
        if args.contains_key(plain) {
            return Err(method_error(
                "invalidArguments",
                &format!("both {plain} and #{plain} are given"),
            ));
        }
        let bad = |why: &str| method_error("invalidResultReference", why);
        let r = v
            .as_object()
            .ok_or_else(|| bad("a reference is an object"))?;
        let (Some(of), Some(name), Some(path)) = (
            r.get("resultOf").and_then(Value::as_str),
            r.get("name").and_then(Value::as_str),
            r.get("path").and_then(Value::as_str),
        ) else {
            return Err(bad("a reference has resultOf, name and path"));
        };
        let source = earlier
            .iter()
            .find(|(_, _, id)| id == of)
            .ok_or_else(|| bad(&format!("no earlier response has id {of}")))?;
        if source.0 != name {
            return Err(bad(&format!(
                "the response {of} is {}, not {name}",
                source.0
            )));
        }
        let value = pointer(&source.1, path)
            .ok_or_else(|| bad(&format!("{path} does not resolve in {of}")))?;
        out.insert(plain.to_owned(), value);
    }
    Ok(out)
}

/// Replace `#creationId` with the id it was created as, wherever an id may appear: as a string
/// value and as an object key (as in `mailboxIds`).
pub fn substitute_creation_ids(v: &Value, created: &Map<String, Value>) -> Value {
    let lookup = |s: &str| {
        s.strip_prefix('#')
            .and_then(|c| created.get(c))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match v {
        Value::String(s) => lookup(s).map(Value::String).unwrap_or_else(|| v.clone()),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| substitute_creation_ids(x, created))
                .collect(),
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    (
                        lookup(k).unwrap_or_else(|| k.clone()),
                        substitute_creation_ids(x, created),
                    )
                })
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// Record the ids a /set or /copy response created.
pub fn record_created(response: &Value, created: &mut Map<String, Value>) {
    if let Some(m) = response.get("created").and_then(Value::as_object) {
        for (cid, obj) in m {
            if let Some(id) = obj.get("id").and_then(Value::as_str) {
                created.insert(cid.clone(), json!(id));
            }
        }
    }
}
