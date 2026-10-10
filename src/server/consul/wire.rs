//! Consul HTTP API shapes, shared by the server and the client: KV entries with their base64
//! values, catalog and health entries built from the few fields a handler decides, and the
//! agent's self description. Measured against Consul 1.20.2.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Map, Value};

/// Largest KV value Consul accepts, and the largest request body here.
pub const MAX_VALUE_BYTES: usize = 512 * 1024;
/// Most entries in one KV listing or catalog answer.
pub const MAX_ENTRIES: usize = 10_000;
pub const DATACENTER: &str = "dc1";
pub const NODE: &str = "netget";
pub const VERSION: &str = "1.20.2";

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("invalid base64 value")
}

/// A value as the handler sees it: text when it is UTF-8, otherwise hex, with which one.
pub fn shown(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        // Text is shown as text when its only control characters are line breaks and tabs.
        Ok(s) if !crate::utils::sanitize::has_controls(&s.replace(['\n', '\t'], "")) => {
            (s.to_string(), "utf8")
        }
        _ => (hex::encode(bytes), "hex"),
    }
}

/// The bytes of an action's `value`, by its `encoding` (utf8 by default, or hex).
pub fn value_bytes(v: &Value) -> Result<Vec<u8>> {
    let value = match &v["value"] {
        Value::Null => return Ok(Vec::new()),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let bytes = match v["encoding"].as_str().unwrap_or("utf8") {
        "utf8" => value.into_bytes(),
        "hex" => hex::decode(value.trim()).context("invalid hex value")?,
        e => bail!("encoding must be utf8 or hex, not {e}"),
    };
    ensure!(
        bytes.len() <= MAX_VALUE_BYTES,
        "value larger than {MAX_VALUE_BYTES} bytes"
    );
    Ok(bytes)
}

pub fn check_key(key: &str) -> Result<()> {
    ensure!(key.len() <= 2048, "key longer than 2048 bytes");
    ensure!(
        !key.contains(['\0', '\r', '\n']),
        "key contains a control character"
    );
    Ok(())
}

/// One KV entry as the API returns it.
pub fn kv_entry(e: &Value, index: u64) -> Result<Value> {
    let key = e["key"].as_str().context("each entry needs a key")?;
    check_key(key)?;
    let value = value_bytes(e)?;
    let modify = e["modify_index"].as_u64().unwrap_or(index);
    Ok(
        json!({"LockIndex": 0, "Key": key, "Flags": e["flags"].as_u64().unwrap_or(0),
              "Value": if e["value"].is_null() { Value::Null } else { json!(b64(&value)) },
              "CreateIndex": e["create_index"].as_u64().unwrap_or(modify), "ModifyIndex": modify}),
    )
}

fn node() -> Value {
    json!({"ID": "00000000-0000-0000-0000-00000000c0de", "Node": NODE, "Address": "127.0.0.1",
           "Datacenter": DATACENTER, "TaggedAddresses": {"lan": "127.0.0.1", "wan": "127.0.0.1"},
           "Meta": {}, "CreateIndex": 1, "ModifyIndex": 1})
}

/// A service instance the handler describes: {id?, name, address?, port?, tags?, meta?}.
pub struct Instance {
    pub id: String,
    pub name: String,
    pub address: String,
    pub port: u64,
    pub tags: Vec<String>,
    pub meta: Map<String, Value>,
}

pub fn instance(v: &Value) -> Result<Instance> {
    let name = v["name"]
        .as_str()
        .context("each service needs a name")?
        .to_string();
    ensure!(
        !name.is_empty() && name.len() <= 256,
        "service name must be 1..=256 bytes"
    );
    let port = v["port"].as_u64().unwrap_or(0);
    ensure!(port <= 65535, "port out of range");
    let tags = v["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Instance {
        id: v["id"].as_str().unwrap_or(&name).to_string(),
        address: v["address"].as_str().unwrap_or_default().to_string(),
        port,
        tags,
        meta: v["meta"].as_object().cloned().unwrap_or_default(),
        name,
    })
}

fn service(i: &Instance, index: u64) -> Value {
    json!({"ID": i.id, "Service": i.name, "Tags": i.tags, "Address": i.address, "Port": i.port,
           "Meta": i.meta, "Weights": {"Passing": 1, "Warning": 1}, "EnableTagOverride": false,
           "Datacenter": DATACENTER, "CreateIndex": index, "ModifyIndex": index})
}

/// `/v1/catalog/service/<name>` entries.
pub fn catalog_entry(i: &Instance, index: u64) -> Value {
    let mut out = node();
    let fields = json!({"ServiceKind": "", "ServiceID": i.id, "ServiceName": i.name, "ServiceTags": i.tags,
        "ServiceAddress": i.address, "ServiceWeights": {"Passing": 1, "Warning": 1}, "ServiceMeta": i.meta,
        "ServicePort": i.port, "ServiceEnableTagOverride": false, "ServiceTaggedAddresses": {},
        "CreateIndex": index, "ModifyIndex": index});
    for (k, v) in fields.as_object().unwrap() {
        out[k] = v.clone();
    }
    out
}

/// `/v1/health/service/<name>` entries, each with a passing serf check.
pub fn health_entry(i: &Instance, index: u64) -> Value {
    json!({"Node": node(), "Service": service(i, index), "Checks": [{
        "Node": NODE, "CheckID": "serfHealth", "Name": "Serf Health Status", "Status": "passing",
        "Notes": "", "Output": "Agent alive and reachable", "ServiceID": "", "ServiceName": "",
        "ServiceTags": [], "Type": "", "CreateIndex": index, "ModifyIndex": index}]})
}

/// `/v1/agent/services`: a map of ID to service.
pub fn agent_service(i: &Instance) -> Value {
    let mut s = service(i, 0);
    let m = s.as_object_mut().unwrap();
    m.remove("CreateIndex");
    m.remove("ModifyIndex");
    s
}

/// `/v1/agent/self`: enough of a description for clients that read it.
pub fn agent_self() -> Value {
    json!({"Config": {"Datacenter": DATACENTER, "PrimaryDatacenter": DATACENTER, "NodeName": NODE,
                      "NodeID": "00000000-0000-0000-0000-00000000c0de", "Server": true,
                      "Version": VERSION, "Revision": "netget"},
           "Member": {"Name": NODE, "Addr": "127.0.0.1", "Port": 8301, "Tags": {"dc": DATACENTER}, "Status": 1},
           "Meta": {}})
}

/// A field of a request body, matched without regard to case: Consul decodes JSON with Go's
/// encoding/json, which does, and clients rely on it (py-consul sends lower-case keys).
fn field<'a>(body: &'a Value, name: &str) -> &'a Value {
    body.as_object()
        .and_then(|m| {
            m.get(name).or_else(|| {
                m.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
        })
        .unwrap_or(&Value::Null)
}

/// A service registration body as the handler sees it.
pub fn registration(body: &Value) -> Result<Value> {
    let name = field(body, "Name").as_str().context("Name is required")?;
    ensure!(
        !name.is_empty() && name.len() <= 256,
        "Name must be 1..=256 bytes"
    );
    Ok(
        json!({"id": field(body, "ID").as_str().unwrap_or(name), "name": name,
              "address": field(body, "Address").as_str().unwrap_or_default(),
              "port": field(body, "Port").as_u64().unwrap_or(0), "tags": field(body, "Tags").clone(),
              "meta": field(body, "Meta").clone()}),
    )
}
