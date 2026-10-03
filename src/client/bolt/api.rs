//! Typed selected Bolt messages. No query engine, graph store or opaque byte actions.
use crate::server::bolt::{
    messages as m,
    packstream::{self, Value},
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value as Json};
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 65536;
pub const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PAGE_RECORDS: usize = 500;
pub const MAX_FIELDS: usize = 256;
pub const MAX_TEXT: usize = 65536;
pub const MAX_PASSWORD: usize = 16384;
pub const MAX_EVENT_DEPTH: usize = MAX_DEPTH + 8;
pub fn within_budget(value: &Json) -> bool {
    crate::utils::json_budget::within_budget(value, MAX_RETAINED_BYTES, MAX_NODES, MAX_DEPTH)
}
pub enum Action {
    Login {
        username: Option<String>,
        password: Option<String>,
    },
    Logoff,
    Run {
        query: String,
        parameters: Value,
        extra: Value,
    },
    Pull(usize),
    Discard,
    Begin(Value),
    Commit,
    Rollback,
    Reset,
    Disconnect,
}
fn text<'a>(value: &'a Json, key: &str, max: usize, required: bool) -> Result<Option<&'a str>> {
    match value.get(key) {
        None if !required => Ok(None),
        Some(Json::String(s)) if !s.is_empty() && s.len() <= max => Ok(Some(s)),
        _ => bail!("Bolt {key} must be a nonempty bounded string"),
    }
}
fn extra(action: &Json) -> Result<Value> {
    let mut extra = Vec::new();
    if let Some(db) = text(action, "database", 256, false)? {
        extra.push(("db".into(), Value::string(db)));
    }
    if let Some(mode) = action.get("mode") {
        let mode = match mode.as_str() {
            Some("read") => "r",
            Some("write") => "w",
            _ => bail!("Bolt mode must be read or write"),
        };
        extra.push(("mode".into(), Value::string(mode)));
    }
    if let Some(ms) = action.get("transaction_timeout_ms") {
        let ms = ms
            .as_i64()
            .filter(|n| (1..=30000).contains(n))
            .context("Bolt transaction_timeout_ms must be1..30000")?;
        extra.push(("tx_timeout".into(), Value::Int(ms)));
    }
    if let Some(bookmarks) = action.get("bookmarks") {
        let bookmarks = bookmarks
            .as_array()
            .filter(|v| v.len() <= 16)
            .context("Bolt bookmarks must be a bounded list")?;
        let values = bookmarks
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 4096)
                    .map(Value::string)
                    .context("Bolt bookmark must be bounded string")
            })
            .collect::<Result<Vec<_>>>()?;
        extra.push(("bookmarks".into(), Value::List(values)));
    }
    Ok(Value::Map(extra))
}
fn parameter(value: &Json, depth: usize) -> Result<Value> {
    ensure!(depth <= MAX_DEPTH + 1, "Bolt parameter wire depth limit");
    if value.is_array() || value.is_object() {
        ensure!(depth <= MAX_DEPTH, "Bolt parameter wire depth limit");
    }
    Ok(match value {
        Json::Null => Value::Null,
        Json::Bool(v) => Value::Bool(*v),
        Json::Number(v) => {
            if let Some(v) = v.as_i64() {
                Value::Int(v)
            } else if v.is_u64() {
                bail!("Bolt parameters require signed64-bit integers")
            } else {
                Value::Float(
                    v.as_f64()
                        .filter(|v| v.is_finite())
                        .context("Bolt finite number required")?,
                )
            }
        }
        Json::String(v) => {
            ensure!(v.len() <= MAX_TEXT, "Bolt parameter string limit");
            Value::string(v)
        }
        Json::Array(v) => {
            ensure!(v.len() <= 10000, "Bolt parameter list limit");
            Value::List(
                v.iter()
                    .map(|v| parameter(v, depth + 1))
                    .collect::<Result<_>>()?,
            )
        }
        Json::Object(v) => {
            ensure!(v.len() <= 256, "Bolt parameter map limit");
            Value::Map(
                v.iter()
                    .map(|(k, v)| {
                        ensure!(k.len() <= 4096, "Bolt parameter key limit");
                        Ok((k.clone(), parameter(v, depth + 1)?))
                    })
                    .collect::<Result<_>>()?,
            )
        }
    })
}
pub fn action(value: &Json) -> Result<Action> {
    ensure!(
        within_budget(value),
        "Bolt action depth/node/retained-content limit"
    );
    Ok(
        match value
            .get("type")
            .and_then(Json::as_str)
            .context("Bolt action type required")?
        {
            "bolt_login" => {
                let username = text(value, "username", 256, false)?.map(str::to_owned);
                let password = text(value, "password", MAX_PASSWORD, false)?.map(str::to_owned);
                ensure!(
                    username.is_some() == password.is_some(),
                    "Bolt basic login requires both username and password; omit both for none"
                );
                Action::Login { username, password }
            }
            "bolt_logoff" => Action::Logoff,
            "bolt_run" => {
                let query = text(value, "query", MAX_TEXT, true)?.unwrap().to_owned();
                let empty = json!({});
                let parameters = value.get("parameters").unwrap_or(&empty);
                ensure!(parameters.is_object(), "Bolt parameters must be an object");
                Action::Run {
                    query,
                    parameters: parameter(parameters, 2)?,
                    extra: extra(value)?,
                }
            }
            "bolt_pull" => Action::Pull(match value.get("n") {
                None => 100,
                Some(n) => n
                    .as_u64()
                    .filter(|n| (1..=MAX_PAGE_RECORDS as u64).contains(n))
                    .context("Bolt pull n must be1..500")? as usize,
            }),
            "bolt_discard" => Action::Discard,
            "bolt_begin" => Action::Begin(extra(value)?),
            "bolt_commit" => Action::Commit,
            "bolt_rollback" => Action::Rollback,
            "bolt_reset" => Action::Reset,
            "disconnect" => Action::Disconnect,
            _ => bail!("unknown selected Bolt action"),
        },
    )
}
pub fn message(tag: u8, fields: Vec<Value>) -> Result<Vec<u8>> {
    let value = Value::Struct { tag, fields };
    if !wire_budget(&value) {
        drop_wire_iteratively(value);
        bail!("Bolt encoded message depth/node/retained-content limit");
    }
    let bytes = packstream::to_bytes(&value);
    ensure!(
        bytes.len() <= packstream::MAX_MESSAGE_BYTES,
        "Bolt message byte limit"
    );
    Ok(packstream::chunk(&bytes))
}
pub fn auth(username: Option<&str>, password: Option<&str>) -> Value {
    match (username, password) {
        (Some(u), Some(p)) => Value::map([
            ("scheme", Value::string("basic")),
            ("principal", Value::string(u)),
            ("credentials", Value::string(p)),
        ]),
        _ => Value::map([("scheme", Value::string("none"))]),
    }
}
pub fn hello(minor: u8, username: Option<&str>, password: Option<&str>) -> Result<Vec<u8>> {
    let mut entries = vec![("user_agent".into(), Value::string("NetGet/0.1"))];
    if minor >= 3 {
        entries.push((
            "bolt_agent".into(),
            Value::map([
                ("product", Value::string("NetGet/0.1")),
                ("language", Value::string("Rust")),
            ]),
        ));
    }
    if minor == 0 {
        let Value::Map(auth) = auth(username, password) else {
            unreachable!()
        };
        entries.extend(auth);
    }
    message(m::HELLO, vec![Value::Map(entries)])
}
pub const PROPOSALS: [u8; 16] = [0, 2, 8, 5, 0, 4, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0];
pub fn selected_version(bytes: [u8; 4]) -> Result<u8> {
    ensure!(
        bytes[0] == 0 && bytes[1] == 0 && bytes[3] == 5 && bytes[2] <= 8 && bytes[2] != 5,
        "Bolt unsupported negotiated version"
    );
    Ok(bytes[2])
}
fn drop_wire_iteratively(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::List(values) | Value::Struct { fields: values, .. } => pending.extend(values),
            Value::Map(values) => pending.extend(values.into_iter().map(|(_, v)| v)),
            _ => {}
        }
    }
}
fn wire_budget(value: &Value) -> bool {
    let mut pending = vec![(value, 1usize)];
    let mut nodes = 0usize;
    let mut retained = 0usize;
    while let Some((v, depth)) = pending.pop() {
        nodes += 1;
        retained = retained.saturating_add(std::mem::size_of::<Value>());
        if nodes > MAX_NODES || retained > MAX_RETAINED_BYTES {
            return false;
        }
        match v {
            Value::String(s) => retained = retained.saturating_add(s.len()),
            Value::Bytes(s) => retained = retained.saturating_add(s.len()),
            Value::Map(v) => {
                if depth > MAX_DEPTH || v.len() > MAX_NODES.saturating_sub(nodes + pending.len()) {
                    return false;
                }
                for (k, v) in v {
                    retained = retained.saturating_add(k.len()).saturating_add(64);
                    pending.push((v, depth + 1));
                }
            }
            Value::List(v) | Value::Struct { fields: v, .. } => {
                if depth > MAX_DEPTH || v.len() > MAX_NODES.saturating_sub(nodes + pending.len()) {
                    return false;
                }
                pending.extend(v.iter().map(|v| (v, depth + 1)));
            }
            _ => {}
        }
        if retained > MAX_RETAINED_BYTES {
            return false;
        }
    }
    true
}
fn integer(v: &Value) -> Result<i64> {
    v.as_int().context("Bolt value requires integer")
}
fn string(v: &Value) -> Result<&str> {
    v.as_str()
        .filter(|s| s.len() <= MAX_TEXT)
        .context("Bolt value requires bounded string")
}
fn map(v: &Value) -> Result<&[(String, Value)]> {
    match v {
        Value::Map(v) if v.len() <= 10000 => Ok(v),
        _ => bail!("Bolt value requires bounded map"),
    }
}
fn list(v: &Value) -> Result<&[Value]> {
    match v {
        Value::List(v) if v.len() <= 10000 => Ok(v),
        _ => bail!("Bolt value requires bounded list"),
    }
}
fn arity(fields: &[Value], n: usize) -> Result<()> {
    ensure!(fields.len() == n, "Bolt value structure arity");
    Ok(())
}
fn float(v: f64) -> Json {
    serde_json::Number::from_f64(v).map(Json::Number).unwrap_or_else(||json!({"$float":if v.is_nan(){"NaN"}else if v.is_sign_positive(){"Infinity"}else{"-Infinity"}}))
}
fn nanos(v: &Value) -> Result<i64> {
    let n = integer(v)?;
    ensure!(
        (0..1_000_000_000).contains(&n),
        "Bolt nanosecond fraction range"
    );
    Ok(n)
}
fn day_nanos(v: &Value) -> Result<i64> {
    let n = integer(v)?;
    ensure!(
        (0..86_400_000_000_000).contains(&n),
        "Bolt time nanosecond range"
    );
    Ok(n)
}
fn offset(v: &Value) -> Result<i64> {
    let n = integer(v)?;
    ensure!(
        (-64_800..=64_800).contains(&n),
        "Bolt timezone offset range"
    );
    Ok(n)
}
fn graph(v: &Value, tag: u8) -> bool {
    matches!(v,Value::Struct{tag:t,..} if *t==tag)
}
fn value(v: &Value, depth: usize, redactor: Option<&Redactor>) -> Result<Json> {
    ensure!(depth <= MAX_DEPTH + 1, "Bolt value conversion depth limit");
    Ok(match v {
        Value::Null => Json::Null,
        Value::Bool(v) => json!(v),
        Value::Int(v) => json!(v),
        Value::Float(v) => float(*v),
        Value::String(v) => {
            ensure!(v.len() <= MAX_TEXT, "Bolt result string limit");
            json!(shown(v, redactor))
        }
        Value::Bytes(v) => json!({"$bytes":{"length":v.len(),"content_omitted":true}}),
        Value::List(v) => {
            ensure!(v.len() <= 10000, "Bolt result list limit");
            Json::Array(
                v.iter()
                    .map(|v| value(v, depth + 1, redactor))
                    .collect::<Result<_>>()?,
            )
        }
        Value::Map(v) => {
            ensure!(v.len() <= 10000, "Bolt result map limit");
            Json::Object(
                v.iter()
                    .map(|(k, v)| {
                        ensure!(k.len() <= 4096, "Bolt result key limit");
                        Ok((shown(k, redactor), value(v, depth + 1, redactor)?))
                    })
                    .collect::<Result<_>>()?,
            )
        }
        Value::Struct { tag, fields: f } => match tag {
            0x4e => {
                arity(f, 4)?;
                let labels = list(&f[1])?
                    .iter()
                    .map(|v| string(v).map(|s| shown(s, redactor)))
                    .collect::<Result<Vec<_>>>()?;
                map(&f[2])?;
                json!({"$node":{"id":integer(&f[0])?,"labels":labels,"properties":value(&f[2],depth+1,redactor)?,"element_id":shown_string(&f[3],redactor)?}})
            }
            0x52 => {
                arity(f, 8)?;
                map(&f[4])?;
                json!({"$relationship":{"id":integer(&f[0])?,"start":integer(&f[1])?,"end":integer(&f[2])?,"relationship_type":shown_string(&f[3],redactor)?,"properties":value(&f[4],depth+1,redactor)?,"element_id":shown_string(&f[5],redactor)?,"start_element_id":shown_string(&f[6],redactor)?,"end_element_id":shown_string(&f[7],redactor)?}})
            }
            0x72 => {
                arity(f, 4)?;
                map(&f[2])?;
                json!({"$unbound_relationship":{"id":integer(&f[0])?,"relationship_type":shown_string(&f[1],redactor)?,"properties":value(&f[2],depth+1,redactor)?,"element_id":shown_string(&f[3],redactor)?}})
            }
            0x50 => {
                arity(f, 3)?;
                let nodes = list(&f[0])?;
                let rels = list(&f[1])?;
                let indices = list(&f[2])?;
                ensure!(
                    !nodes.is_empty()
                        && nodes.iter().all(|v| graph(v, 0x4e))
                        && rels.iter().all(|v| graph(v, 0x72))
                        && indices.len() % 2 == 0,
                    "Bolt path table schema"
                );
                for pair in indices.chunks_exact(2) {
                    let rel = integer(&pair[0])?;
                    let node = integer(&pair[1])?;
                    ensure!(
                        rel.checked_abs()
                            .is_some_and(|r| r > 0 && (r as u64) <= rels.len() as u64)
                            && node >= 0
                            && (node as u64) < nodes.len() as u64,
                        "Bolt path table index"
                    );
                }
                json!({"$path":{"nodes":value(&f[0],depth+1,redactor)?,"relationships":value(&f[1],depth+1,redactor)?,"indices":value(&f[2],depth+1,redactor)?}})
            }
            0x44 => {
                arity(f, 1)?;
                json!({"$date":{"days_since_epoch":integer(&f[0])?}})
            }
            0x54 => {
                arity(f, 2)?;
                json!({"$time":{"nanoseconds_since_midnight":day_nanos(&f[0])?,"offset_seconds":offset(&f[1])?}})
            }
            0x74 => {
                arity(f, 1)?;
                json!({"$local_time":{"nanoseconds_since_midnight":day_nanos(&f[0])?}})
            }
            0x49 => {
                arity(f, 3)?;
                json!({"$datetime":{"epoch_seconds":integer(&f[0])?,"nanoseconds":nanos(&f[1])?,"offset_seconds":offset(&f[2])?}})
            }
            0x69 => {
                arity(f, 3)?;
                json!({"$datetime_zone":{"epoch_seconds":integer(&f[0])?,"nanoseconds":nanos(&f[1])?,"zone_id":shown_string(&f[2],redactor)?}})
            }
            0x64 => {
                arity(f, 2)?;
                json!({"$local_datetime":{"local_epoch_seconds":integer(&f[0])?,"nanoseconds":nanos(&f[1])?}})
            }
            0x45 => {
                arity(f, 4)?;
                json!({"$duration":{"months":integer(&f[0])?,"days":integer(&f[1])?,"seconds":integer(&f[2])?,"nanoseconds":nanos(&f[3])?}})
            }
            0x58 | 0x59 => {
                arity(f, if *tag == 0x58 { 3 } else { 4 })?;
                let mut coordinates = Vec::new();
                for v in &f[1..] {
                    let Value::Float(v) = v else {
                        bail!("Bolt point coordinate requires float")
                    };
                    ensure!(v.is_finite(), "Bolt point coordinate must be finite");
                    coordinates.push(float(*v));
                }
                json!({"$point":{"srid":integer(&f[0])?,"coordinates":coordinates}})
            }
            _ => bail!("unsupported Bolt value structure"),
        },
    })
}
#[derive(Debug)]
pub enum Reply {
    Success(Json),
    Record(Vec<Json>),
    Ignored,
    Failure(Json),
}
pub fn reply(bytes: &[u8], minor: u8) -> Result<Reply> {
    reply_inner(bytes, minor, None)
}
pub fn reply_private(bytes: &[u8], minor: u8, secret: Option<&str>) -> Result<Reply> {
    let redactor = secret.filter(|v| !v.is_empty()).map(Redactor::new);
    reply_inner(bytes, minor, redactor.as_ref())
}
fn reply_inner(bytes: &[u8], minor: u8, redactor: Option<&Redactor>) -> Result<Reply> {
    ensure!(
        bytes.len() <= packstream::MAX_MESSAGE_BYTES,
        "Bolt inbound byte limit"
    );
    let raw = packstream::decode(bytes)
        .map_err(|_| anyhow::anyhow!("Bolt PackStream schema/depth refusal"))?;
    ensure!(
        wire_budget(&raw),
        "Bolt inbound node/retained-content limit"
    );
    let Value::Struct { tag, fields } = raw else {
        bail!("Bolt response must be structure")
    };
    match tag {
        m::SUCCESS => {
            arity(&fields, 1)?;
            map(&fields[0])?;
            let metadata = value(&fields[0], 2, None)?;
            for name in [
                "server",
                "protocol_version",
                "connection_id",
                "bookmark",
                "db",
                "advertised_address",
            ] {
                if let Some(v) = metadata.get(name) {
                    ensure!(v.is_string(), "Bolt success string schema");
                }
            }
            if let Some(v) = metadata.get("protocol_version") {
                ensure!(
                    v.as_str() == Some(format!("5.{minor}").as_str()),
                    "Bolt HELLO protocol_version mismatch"
                );
            }
            for name in ["has_more", "credentials_expired"] {
                if let Some(v) = metadata.get(name) {
                    ensure!(v.is_boolean(), "Bolt success boolean schema");
                }
            }
            for name in ["qid", "t_first", "t_last"] {
                if let Some(v) = metadata.get(name) {
                    ensure!(
                        v.as_i64().is_some_and(|v| v >= 0),
                        "Bolt success integer schema"
                    );
                }
            }
            if let Some(v) = metadata.get("fields") {
                ensure!(
                    v.as_array().is_some_and(|v| v.len() <= MAX_FIELDS
                        && v.iter()
                            .all(|v| v.as_str().is_some_and(|s| s.len() <= 4096))),
                    "Bolt success fields schema"
                );
            }
            if let Some(v) = metadata.get("type") {
                ensure!(
                    matches!(v.as_str(), Some("r" | "w" | "rw" | "s")),
                    "Bolt query type schema"
                );
            }
            for name in ["hints", "stats", "plan", "profile"] {
                if let Some(v) = metadata.get(name) {
                    ensure!(v.is_object(), "Bolt success map schema");
                }
            }
            if let Some(stats) = metadata.get("stats").and_then(Json::as_object) {
                for (key, v) in stats {
                    if matches!(key.as_str(), "contains-updates" | "contains-system-updates") {
                        ensure!(v.is_boolean(), "Bolt update flag schema");
                    } else if matches!(
                        key.as_str(),
                        "nodes-created"
                            | "nodes-deleted"
                            | "relationships-created"
                            | "relationships-deleted"
                            | "properties-set"
                            | "labels-added"
                            | "labels-removed"
                            | "indexes-added"
                            | "indexes-removed"
                            | "constraints-added"
                            | "constraints-removed"
                            | "system-updates"
                    ) {
                        ensure!(
                            v.as_i64().is_some_and(|n| n >= 0),
                            "Bolt update counter schema"
                        );
                    }
                }
            }
            for name in ["statuses", "notifications"] {
                if let Some(v) = metadata.get(name) {
                    ensure!(
                        v.as_array().is_some_and(|v| v.iter().all(Json::is_object)),
                        "Bolt success notification schema"
                    );
                }
            }
            Ok(Reply::Success(if redactor.is_some() {
                metadata_value(&fields[0], 2, redactor, MetaKind::Success)?
            } else {
                metadata
            }))
        }
        m::RECORD => {
            arity(&fields, 1)?;
            let records = list(&fields[0])?;
            ensure!(records.len() <= MAX_FIELDS, "Bolt record width limit");
            Ok(Reply::Record(
                records
                    .iter()
                    .map(|v| value(v, 3, redactor))
                    .collect::<Result<_>>()?,
            ))
        }
        m::IGNORED => {
            arity(&fields, 0)?;
            Ok(Reply::Ignored)
        }
        m::FAILURE => {
            arity(&fields, 1)?;
            map(&fields[0])?;
            let failure = value(&fields[0], 2, None)?;
            validate_failure(&failure, minor, 1)?;
            Ok(Reply::Failure(if redactor.is_some() {
                metadata_value(&fields[0], 2, redactor, MetaKind::Failure)?
            } else {
                failure
            }))
        }
        _ => bail!("unsupported Bolt response tag"),
    }
}
fn validate_failure(value: &Json, minor: u8, depth: usize) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "Bolt failure cause depth limit");
    ensure!(
        value
            .get("message")
            .and_then(Json::as_str)
            .is_some_and(|v| v.len() <= MAX_TEXT),
        "Bolt failure message schema"
    );
    if depth == 1 {
        text(
            value,
            if minor >= 7 { "neo4j_code" } else { "code" },
            4096,
            true,
        )?;
    }
    if minor >= 7 {
        let gql = text(value, "gql_status", 5, true)?.unwrap();
        ensure!(
            gql.len() == 5
                && gql
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
            "Bolt GQL status schema"
        );
        ensure!(
            value
                .get("description")
                .and_then(Json::as_str)
                .is_some_and(|v| v.len() <= MAX_TEXT),
            "Bolt failure description schema"
        );
        if let Some(v) = value.get("diagnostic_record") {
            ensure!(v.is_object(), "Bolt diagnostic_record must be map");
        }
        if let Some(cause) = value.get("cause") {
            ensure!(cause.is_object(), "Bolt cause must be map");
            validate_failure(cause, minor, depth + 1)?;
        }
    }
    Ok(())
}
// Redact dynamic wire strings and map keys before constructing typed graph wrappers.
// Fixed metadata discriminants remain stable even when a password equals a schema key.
struct Redactor {
    forms: [String; 3],
}
impl Redactor {
    fn new(secret: &str) -> Self {
        let encoded = serde_json::to_string(secret).unwrap();
        let debug = format!("{secret:?}");
        Self {
            forms: [
                secret.into(),
                encoded[1..encoded.len() - 1].into(),
                debug[1..debug.len() - 1].into(),
            ],
        }
    }
    fn text(&self, text: &str) -> String {
        if self.forms.iter().any(|s| text.contains(s)) {
            crate::utils::redact::REDACTED.into()
        } else {
            text.into()
        }
    }
}
fn shown(text: &str, redactor: Option<&Redactor>) -> String {
    redactor.map_or_else(|| text.into(), |r| r.text(text))
}
fn shown_string(v: &Value, redactor: Option<&Redactor>) -> Result<String> {
    Ok(shown(string(v)?, redactor))
}
pub fn redact_text(text: &str, secret: Option<&str>) -> String {
    secret
        .filter(|s| !s.is_empty())
        .map_or_else(|| text.into(), |s| Redactor::new(s).text(text))
}
#[derive(Clone, Copy)]
enum MetaKind {
    Success,
    Failure,
    Stats,
    Diagnostic,
}
fn metadata_value(
    v: &Value,
    depth: usize,
    redactor: Option<&Redactor>,
    kind: MetaKind,
) -> Result<Json> {
    ensure!(depth <= MAX_DEPTH, "Bolt metadata conversion depth limit");
    let mut result = serde_json::Map::new();
    for (key, v) in map(v)? {
        let fixed = match kind {
            MetaKind::Success => matches!(
                key.as_str(),
                "server"
                    | "protocol_version"
                    | "connection_id"
                    | "bookmark"
                    | "db"
                    | "advertised_address"
                    | "has_more"
                    | "credentials_expired"
                    | "qid"
                    | "t_first"
                    | "t_last"
                    | "fields"
                    | "type"
                    | "hints"
                    | "stats"
                    | "plan"
                    | "profile"
                    | "notifications"
                    | "statuses"
            ),
            MetaKind::Failure => matches!(
                key.as_str(),
                "code"
                    | "message"
                    | "neo4j_code"
                    | "gql_status"
                    | "description"
                    | "diagnostic_record"
                    | "cause"
            ),
            MetaKind::Diagnostic => matches!(
                key.as_str(),
                "OPERATION"
                    | "OPERATION_CODE"
                    | "CURRENT_SCHEMA"
                    | "_classification"
                    | "_severity"
                    | "_position"
            ),
            MetaKind::Stats => matches!(
                key.as_str(),
                "nodes-created"
                    | "nodes-deleted"
                    | "relationships-created"
                    | "relationships-deleted"
                    | "properties-set"
                    | "labels-added"
                    | "labels-removed"
                    | "indexes-added"
                    | "indexes-removed"
                    | "constraints-added"
                    | "constraints-removed"
                    | "system-updates"
                    | "contains-updates"
                    | "contains-system-updates"
            ),
        };
        let child = match (kind, key.as_str()) {
            (MetaKind::Failure, "diagnostic_record") => {
                metadata_value(v, depth + 1, redactor, MetaKind::Diagnostic)?
            }
            (MetaKind::Failure, "cause") => {
                metadata_value(v, depth + 1, redactor, MetaKind::Failure)?
            }
            (MetaKind::Success, "stats") => {
                metadata_value(v, depth + 1, redactor, MetaKind::Stats)?
            }
            _ => value(v, depth + 1, redactor)?,
        };
        result.insert(
            if fixed {
                key.clone()
            } else {
                shown(key, redactor)
            },
            child,
        );
    }
    Ok(Json::Object(result))
}

/// Conservative converted-record retained-content measure, before adding it to a page.
pub fn retained_size(value: &Json) -> Option<usize> {
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        bytes = bytes.saturating_add(std::mem::size_of::<Json>());
        if nodes > MAX_NODES || depth > MAX_EVENT_DEPTH || bytes > MAX_RETAINED_BYTES {
            return None;
        }
        match value {
            Json::String(v) => bytes = bytes.saturating_add(v.len()),
            Json::Array(v) => {
                if v.len() > MAX_NODES.saturating_sub(nodes + pending.len()) {
                    return None;
                }
                pending.extend(v.iter().map(|v| (v, depth + 1)));
            }
            Json::Object(v) => {
                if v.len() > MAX_NODES.saturating_sub(nodes + pending.len()) {
                    return None;
                }
                for (k, v) in v {
                    bytes = bytes.saturating_add(k.len()).saturating_add(64);
                    pending.push((v, depth + 1));
                }
            }
            _ => {}
        }
        if bytes > MAX_RETAINED_BYTES {
            return None;
        }
    }
    Some(bytes)
}
